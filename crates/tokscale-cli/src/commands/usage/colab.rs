use anyhow::{Context, Result};
use chrono::{NaiveDateTime, Utc};
use serde_json::Value;
use std::path::PathBuf;

use super::{UsageMetric, UsageOutput};

fn token_path() -> Option<PathBuf> {
    std::env::var_os("TOKSCALE_COLAB_TOKEN_PATH")
        .map(PathBuf::from)
        .or_else(|| crate::paths::home_dir().map(|home| home.join(".config/colab-cli/token.json")))
}

fn field<'a>(doc: &'a Value, name: &str) -> Option<&'a str> {
    doc.get(name)?.as_str().filter(|s| !s.trim().is_empty())
}

pub fn has_credentials() -> bool {
    token_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|doc| field(&doc, "token").is_some() || field(&doc, "refresh_token").is_some())
}

fn number(doc: &Value, name: &str) -> Option<f64> {
    let value = doc.get(name)?;
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
}

fn parse_usage(body: &str) -> Result<Value> {
    let body = body.trim_start();
    serde_json::from_str(body.strip_prefix(")]}'").unwrap_or(body).trim_start())
        .context("Invalid Colab usage response")
}

fn build_metrics(doc: &Value) -> Result<Vec<UsageMetric>> {
    let balance = number(doc, "currentBalance")
        .context("Colab response is missing a valid compute-unit balance")?;
    let metric = |label: &str, value: String| UsageMetric {
        label: label.to_string(),
        used_percent: 0.0,
        remaining_percent: 100.0,
        remaining_label: Some(value),
        resets_at: None,
    };
    let mut metrics = vec![metric("Compute units", format!("{balance:.2} CU left"))];
    if let Some(rate) = number(doc, "consumptionRateHourly").filter(|n| *n > 0.0) {
        metrics.push(metric("Burn rate", format!("{rate:.2} CU/hr")));
    }
    if let Some(count) = number(doc, "assignmentsCount") {
        metrics.push(metric("Runtimes", format!("{count:.0} active")));
    }
    Ok(metrics)
}

fn expired(doc: &Value) -> bool {
    field(doc, "expiry")
        .and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|d| d.with_timezone(&Utc))
                .ok()
                .or_else(|| {
                    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                        .ok()
                        .map(|d| d.and_utc())
                })
        })
        .is_some_and(|expiry| expiry <= Utc::now())
}

async fn refresh(client: &reqwest::Client, doc: &Value) -> Result<String> {
    let mut form = Vec::new();
    for name in ["refresh_token", "client_id", "client_secret"] {
        form.push((
            name,
            field(doc, name).with_context(|| {
                format!("Colab OAuth credentials missing {name}; log in with colab-cli")
            })?,
        ));
    }
    form.push(("grant_type", "refresh_token"));
    let response = client
        .post(field(doc, "token_uri").unwrap_or("https://oauth2.googleapis.com/token"))
        .form(&form)
        .send()
        .await
        .context("Colab OAuth refresh request failed")?;
    if !response.status().is_success() {
        anyhow::bail!(
            "Colab OAuth refresh failed (HTTP {}); log in with colab-cli",
            response.status()
        );
    }
    let refreshed: Value = response
        .json()
        .await
        .context("Invalid Colab OAuth refresh response")?;
    // Keep the fresh token in memory: this integration must not write outside
    // the repository, including colab-cli's credential file.
    Ok(field(&refreshed, "access_token")
        .context("Colab OAuth refresh returned no access token")?
        .to_string())
}

async fn fetch_with(doc: &Value, base: &str) -> Result<UsageOutput> {
    let client = tokscale_core::http::client_builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let refreshed = expired(doc) || field(doc, "token").is_none();
    let mut token = if refreshed {
        refresh(&client, doc).await?
    } else {
        field(doc, "token").unwrap().to_string()
    };
    let url = format!("{}/tun/m/ccu-info?authuser=0", base.trim_end_matches('/'));
    let request = |token: &str| {
        client
            .get(&url)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .header("X-Colab-Client-Agent", "colab-cli")
    };
    let mut response = request(&token)
        .send()
        .await
        .context("Colab usage request failed")?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
        token = refresh(&client, doc).await?;
        response = request(&token)
            .send()
            .await
            .context("Colab usage retry failed")?;
    }
    if !response.status().is_success() {
        anyhow::bail!(
            "Colab usage request failed (HTTP {}); log in with colab-cli if authentication expired",
            response.status()
        );
    }
    let metrics = build_metrics(&parse_usage(&response.text().await?)?)?;
    Ok(UsageOutput {
        provider: "Colab".to_string(),
        account: None,
        credential_source: None,
        plan: None,
        email: None,
        metrics,
        reset_credits: None,
        credit_status: None,
        spend_control: None,
    })
}

pub fn fetch() -> Result<UsageOutput> {
    let path = token_path().context("Could not locate Colab credentials")?;
    let doc: Value = serde_json::from_slice(
        &std::fs::read(path).context("Could not read colab-cli token.json")?,
    )
    .context("Invalid colab-cli token.json")?;
    let base = std::env::var("TOKSCALE_COLAB_BASE_URL")
        .unwrap_or_else(|_| "https://colab.research.google.com".to_string());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(fetch_with(&doc, &base))
}

#[cfg(test)]
mod tests {
    use super::super::test_server::spawn_server;
    use super::*;
    use serde_json::json;

    #[test]
    fn colab_xssi_and_metrics() {
        for prefix in ["", ")]}'", ")]}'\n"] {
            let doc = parse_usage(&format!("{prefix}{}", json!({"currentBalance":"200.25", "consumptionRateHourly":1.23,"assignmentsCount":"2","extra":true}))).unwrap();
            let metrics = build_metrics(&doc).unwrap();
            assert_eq!(
                metrics[0].remaining_label.as_deref(),
                Some("200.25 CU left")
            );
            assert!(metrics
                .iter()
                .all(|m| m.used_percent == 0.0 && m.remaining_percent == 100.0));
            assert_eq!(metrics[1].remaining_label.as_deref(), Some("1.23 CU/hr"));
            assert_eq!(metrics[2].remaining_label.as_deref(), Some("2 active"));
        }
        assert!(build_metrics(&json!({"currentBalance":"NaN"})).is_err());
        assert!(parse_usage(")]}'not json").is_err());
        assert_eq!(
            build_metrics(&json!({"currentBalance":0,"consumptionRateHourly":0}))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn colab_expiry_formats() {
        for expiry in [
            "2000-01-01T00:00:00Z",
            "2000-01-01T00:00:00.123456",
            "2000-01-01T01:00:00+01:00",
        ] {
            assert!(expired(&json!({"expiry":expiry})));
        }
        assert!(!expired(&json!({"expiry":"unknown"})));
    }

    #[test]
    fn colab_http_refresh_retry() {
        let (base, seen) = spawn_server(|path, call| match call {
            0 => {
                assert_eq!(path, "/tun/m/ccu-info?authuser=0");
                (401, "{}".into())
            }
            1 => {
                assert_eq!(path, "/token");
                (200, r#"{"access_token":"fresh"}"#.into())
            }
            _ => (200, ")]}'\n{\"currentBalance\":200}".into()),
        });
        let doc = json!({"token":"old","refresh_token":"refresh","client_id":"id","client_secret":"secret","token_uri":format!("{base}/token")});
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = rt.block_on(fetch_with(&doc, &base)).unwrap();
        assert_eq!(output.provider, "Colab");
        let log = seen.lock().unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].bearer.as_deref(), Some("old"));
        assert_eq!(log[1].request, "POST /token");
        assert_eq!(log[2].bearer.as_deref(), Some("fresh"));
        assert_eq!(doc["token"], "old");
    }

    #[test]
    fn colab_refresh_only_and_retry_limit() {
        let (base, seen) = spawn_server(|path, _| {
            if path == "/token" {
                (200, r#"{"access_token":"fresh"}"#.into())
            } else {
                (401, "{}".into())
            }
        });
        let doc = json!({"refresh_token":"refresh","client_id":"id","client_secret":"secret","token_uri":format!("{base}/token")});
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(rt.block_on(fetch_with(&doc, &base)).is_err());
        assert_eq!(seen.lock().unwrap().len(), 2);
    }
}
