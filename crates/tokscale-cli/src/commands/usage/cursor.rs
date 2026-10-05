//! Cursor subscription plan remaining for `tokscale usage`.
//!
//! Auth comes from the desktop `state.vscdb` JWT, then the macOS Keychain items
//! `cursor-access-token` / `cursor-refresh-token`, then a JWT embedded in a
//! saved Tokscale Cursor session cookie. The plan window is
//! `GetCurrentPeriodUsage` on `api2.cursor.sh` (same path TokenBar uses).
//! Refreshed access tokens stay in memory and are never written back.

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use serde_json::Value;

use super::{UsageMetric, UsageOutput};

const USAGE_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
const PLAN_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetPlanInfo";
const REFRESH_URL: &str = "https://api2.cursor.sh/oauth/token";
/// Public Cursor Auth0 client id. Not a user secret.
const CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";
const ACCESS_SERVICE: &str = "cursor-access-token";
const REFRESH_SERVICE: &str = "cursor-refresh-token";
const PROVIDER: &str = "Cursor";

#[derive(Debug, Clone, PartialEq)]
struct ParsedPlanWindow {
    used_percent: f64,
    remaining_percent: f64,
    reset_at: Option<String>,
}

#[derive(Debug, Clone)]
struct LocalCursorAuth {
    access_token: String,
    refresh_token: Option<String>,
    email: Option<String>,
}

pub fn has_credentials() -> bool {
    resolve_local_auth().is_ok()
}

pub fn fetch() -> Result<UsageOutput> {
    let auth = resolve_local_auth()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let now = Utc::now().timestamp();
        let access = bearer_for_request(&auth, now).await?;
        let client = plan_http_client()?;
        let usage = connect_post(&client, USAGE_URL, &access, "GetCurrentPeriodUsage").await?;
        let plan = if billing_reset_iso(usage.get("billingCycleEnd")).is_none() {
            connect_post(&client, PLAN_URL, &access, "GetPlanInfo")
                .await
                .ok()
        } else {
            None
        };
        let window = parse_plan_window(&usage, plan.as_ref())?;
        let plan_name = plan
            .as_ref()
            .and_then(|value| value.pointer("/planInfo/planName"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .or_else(|| {
                usage
                    .get("membershipType")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
            });

        Ok(UsageOutput {
            provider: PROVIDER.to_string(),
            account: None,
            credential_source: Some("desktop".into()),
            plan: plan_name,
            email: auth.email,
            metrics: vec![UsageMetric {
                label: "Plan".into(),
                used_percent: window.used_percent,
                remaining_percent: window.remaining_percent,
                remaining_label: None,
                resets_at: window.reset_at,
            }],
            reset_credits: None,
            credit_status: None,
            spend_control: None,
        })
    })
}

fn plan_http_client() -> Result<reqwest::Client> {
    // Match the Cursor CLI client: cursor.com / api2 sit behind fingerprints
    // that reject rustls on some networks (#1250).
    #[allow(clippy::disallowed_methods)]
    let builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30));
    #[cfg(not(target_os = "android"))]
    let builder = builder.use_native_tls();
    builder.build().context("Failed to build Cursor plan HTTP client")
}

fn access_token_usable(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    parts.len() == 3 && parts.iter().all(|part| !part.is_empty())
}

fn needs_refresh(token: &str, now_unix: i64) -> bool {
    jwt_exp(token).is_some_and(|exp| exp <= now_unix.saturating_add(60))
}

fn jwt_exp(token: &str) -> Option<i64> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("exp").and_then(Value::as_i64)
}

fn account_label(raw: Option<&str>) -> Option<String> {
    let email = raw?.trim();
    if (3..=254).contains(&email.len())
        && email.matches('@').count() == 1
        && !email.contains(char::is_whitespace)
        && !email.contains("Bearer")
    {
        Some(email.to_string())
    } else {
        None
    }
}

fn jwt_from_session_token(session: &str) -> Option<String> {
    let decoded = session.replace("%3A%3A", "::");
    let jwt = decoded.split("::").nth(1)?.trim();
    access_token_usable(jwt).then(|| jwt.to_string())
}

fn resolve_local_auth() -> Result<LocalCursorAuth> {
    let home = crate::paths::home_dir().context("Could not determine home directory")?;
    let db_path = crate::cursor::find_cursor_state_vscdb_for_usage(&home);
    let (db_access, db_refresh, db_email) = if let Some(path) = db_path.as_ref() {
        (
            read_vscdb_value(path, "cursorAuth/accessToken").ok().flatten(),
            read_vscdb_value(path, "cursorAuth/refreshToken")
                .ok()
                .flatten(),
            read_vscdb_value(path, "cursorAuth/cachedEmail")
                .ok()
                .flatten(),
        )
    } else {
        (None, None, None)
    };

    if let Some(access) = db_access
        .as_deref()
        .map(str::trim)
        .filter(|token| access_token_usable(token))
    {
        return Ok(LocalCursorAuth {
            access_token: access.to_string(),
            refresh_token: nonempty(db_refresh.as_deref()),
            email: account_label(db_email.as_deref()),
        });
    }

    #[cfg(target_os = "macos")]
    if let Ok(access) = super::helpers::read_keychain(ACCESS_SERVICE) {
        let access = access.trim();
        if access_token_usable(access) {
            let refresh = super::helpers::read_keychain(REFRESH_SERVICE).ok();
            return Ok(LocalCursorAuth {
                access_token: access.to_string(),
                refresh_token: nonempty(refresh.as_deref()).or_else(|| nonempty(db_refresh.as_deref())),
                email: account_label(db_email.as_deref()),
            });
        }
    }

    if let Some(creds) = crate::cursor::load_active_credentials() {
        if let Some(access) = jwt_from_session_token(&creds.session_token) {
            return Ok(LocalCursorAuth {
                access_token: access,
                refresh_token: None,
                email: account_label(db_email.as_deref()),
            });
        }
    }

    anyhow::bail!(
        "Cursor plan credentials not found. Sign in to the Cursor desktop app, or run 'tokscale cursor login'."
    )
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn read_vscdb_value(db_path: &std::path::Path, key: &str) -> Result<Option<String>> {
    use rusqlite::{Connection, OpenFlags};

    if !matches!(
        key,
        "cursorAuth/accessToken" | "cursorAuth/refreshToken" | "cursorAuth/cachedEmail"
    ) {
        anyhow::bail!("refused unexpected Cursor state key");
    }

    let uri = format!("file:{}?mode=ro", db_path.display());
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("Failed to open Cursor state DB at {}", db_path.display()))?;

    let value: Option<String> = match conn.query_row(
        "SELECT value FROM ItemTable WHERE key = ?1",
        [key],
        |row| row.get(0),
    ) {
        Ok(value) => Some(value),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(err) => return Err(err.into()),
    };
    Ok(value
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty()))
}

async fn bearer_for_request(auth: &LocalCursorAuth, now_unix: i64) -> Result<String> {
    if !needs_refresh(&auth.access_token, now_unix) {
        return Ok(auth.access_token.clone());
    }
    let Some(refresh) = auth.refresh_token.as_deref() else {
        anyhow::bail!("Cursor access token is expired and no refresh token was stored.");
    };
    refresh_access_token(refresh).await
}

async fn refresh_access_token(refresh_token: &str) -> Result<String> {
    let client = plan_http_client()?;
    let response = client
        .post(REFRESH_URL)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .context("Cursor token refresh request failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Cursor token refresh body could not be read")?;
    if !status.is_success() {
        anyhow::bail!("Cursor token refresh returned HTTP {status}");
    }
    let value: Value =
        serde_json::from_str(&body).context("Cursor token refresh was not JSON")?;
    if value
        .get("shouldLogout")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        anyhow::bail!("Cursor refresh token was rejected. Sign in again in Cursor.");
    }
    let access = value
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| access_token_usable(token))
        .ok_or_else(|| anyhow::anyhow!("Cursor token refresh returned no access token."))?;
    Ok(access.to_string())
}

async fn connect_post(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
    label: &str,
) -> Result<Value> {
    let response = client
        .post(url)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .body("{}")
        .send()
        .await
        .with_context(|| format!("Cursor {label} request failed"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("Cursor {label} body could not be read"))?;
    if !status.is_success() {
        anyhow::bail!("Cursor {label} returned HTTP {status}");
    }
    serde_json::from_str(&body).with_context(|| format!("Cursor {label} was not JSON"))
}

fn parse_plan_window(usage: &Value, plan: Option<&Value>) -> Result<ParsedPlanWindow> {
    let plan_usage = usage
        .get("planUsage")
        .filter(|value| value.is_object())
        .ok_or_else(|| plan_usage_missing(usage))?;
    let (used_percent, remaining_percent) = plan_percents(plan_usage)?;
    let reset_at = billing_reset_iso(usage.get("billingCycleEnd")).or_else(|| {
        billing_reset_iso(plan.and_then(|value| value.pointer("/planInfo/billingCycleEnd")))
    });
    Ok(ParsedPlanWindow {
        used_percent,
        remaining_percent,
        reset_at,
    })
}

fn plan_usage_missing(usage: &Value) -> anyhow::Error {
    match usage.get("code").and_then(Value::as_str) {
        Some(code) if is_short_code(code) => {
            anyhow::anyhow!("Cursor GetCurrentPeriodUsage returned code {code}.")
        }
        _ => anyhow::anyhow!("Cursor GetCurrentPeriodUsage had no planUsage object."),
    }
}

fn is_short_code(code: &str) -> bool {
    (1..40).contains(&code.len())
        && code
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn plan_percents(plan_usage: &Value) -> Result<(f64, f64)> {
    if let Some(used) = plan_usage.get("totalPercentUsed").and_then(Value::as_f64) {
        if used.is_finite() && (0.0..=100.0).contains(&used) {
            return Ok(round_pair(used));
        }
    }
    let remaining = plan_usage.get("remaining").and_then(Value::as_f64);
    let limit = plan_usage.get("limit").and_then(Value::as_f64);
    match (remaining, limit) {
        (Some(remaining), Some(limit))
            if remaining.is_finite()
                && limit.is_finite()
                && limit > 0.0
                && remaining >= 0.0
                && remaining <= limit =>
        {
            let used = ((limit - remaining) / limit) * 100.0;
            Ok(round_pair(used))
        }
        _ => anyhow::bail!(
            "Cursor planUsage had no finite remaining percent (totalPercentUsed, or remaining and limit)."
        ),
    }
}

fn round_pair(used: f64) -> (f64, f64) {
    let used = (used * 100.0).round() / 100.0;
    let remaining = ((100.0 - used) * 100.0).round() / 100.0;
    (used, remaining)
}

fn billing_reset_iso(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        let text = text.trim();
        if let Ok(raw) = text.parse::<i64>() {
            return millis_or_secs_iso(raw);
        }
        return DateTime::parse_from_rfc3339(text).ok().map(|parsed| {
            parsed
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        });
    }
    value.as_i64().and_then(millis_or_secs_iso)
}

fn millis_or_secs_iso(raw: i64) -> Option<String> {
    let ms = if raw.abs() >= 1_000_000_000_000 {
        raw
    } else {
        raw.checked_mul(1000)?
    };
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|instant| instant.to_rfc3339_opts(SecondsFormat::Millis, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_plan_window_from_total_percent_used() {
        let usage = json!({
            "planUsage": { "totalPercentUsed": 37.5 },
            "billingCycleEnd": "1767225600000"
        });
        let window = parse_plan_window(&usage, None).unwrap();
        assert!((window.used_percent - 37.5).abs() < f64::EPSILON);
        assert!((window.remaining_percent - 62.5).abs() < f64::EPSILON);
        assert_eq!(
            window.reset_at.as_deref(),
            Some("2026-01-01T00:00:00.000Z")
        );
    }

    #[test]
    fn parse_plan_window_from_remaining_and_limit() {
        let usage = json!({
            "planUsage": { "remaining": 25.0, "limit": 100.0 },
            "billingCycleEnd": null
        });
        let plan = json!({ "planInfo": { "billingCycleEnd": "2026-02-01T00:00:00Z" } });
        let window = parse_plan_window(&usage, Some(&plan)).unwrap();
        assert!((window.used_percent - 75.0).abs() < f64::EPSILON);
        assert!((window.remaining_percent - 25.0).abs() < f64::EPSILON);
        assert!(window.reset_at.as_deref().unwrap().starts_with("2026-02-01"));
    }

    #[test]
    fn jwt_from_session_token_accepts_encoded_separator() {
        let jwt = "hdr.eyJzdWIiOiJ1c2VyX2FiYyIsImV4cCI6OTk5OTk5OTk5OX0.sig";
        let session = format!("user_abc%3A%3A{jwt}");
        assert_eq!(jwt_from_session_token(&session).as_deref(), Some(jwt));
    }

    #[test]
    fn access_token_usable_requires_three_jwt_segments() {
        assert!(access_token_usable("a.b.c"));
        assert!(!access_token_usable("ciphertext-or-empty"));
        assert!(!access_token_usable("a.b"));
    }
}
