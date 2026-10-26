use crate::commands::usage::{UsageFetchDiagnosticSeverity, UsageFetchIntent, UsageFetchReport};
use crate::TsTokenContributionData;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QuotaReport {
    version: u8,
    observed_at: String,
    pools: Vec<QuotaPool>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuotaPool {
    provider: String,
    account_id: String,
    account_label: Option<String>,
    email_masked: Option<String>,
    plan: Option<String>,
    status: &'static str,
    error: Option<&'static str>,
    windows: Vec<QuotaWindow>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuotaWindow {
    label: String,
    window: &'static str,
    used_percent: Option<f64>,
    remaining_percent: Option<f64>,
    resets_at: Option<String>,
}

fn identity(provider: &str, account: Option<&str>, email: Option<&str>) -> Option<String> {
    let email = email.map(|value| value.trim().to_lowercase());
    let id = account
        .filter(|value| !value.trim().is_empty())
        .or(email.as_deref().filter(|value| !value.is_empty()))?;
    Some(format!("{:x}", Sha256::digest(format!("{provider}|{id}")))[..24].to_owned())
}

fn text(value: Option<&str>, limit: usize) -> Option<String> {
    value
        .filter(|value| !value.contains('@'))
        .map(|value| value.chars().take(limit).collect())
}

fn masked_email(email: Option<&str>) -> Option<String> {
    let (name, domain) = email?.trim().rsplit_once('@')?;
    if domain.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!(
        "{}***@{}",
        name.chars().next()?,
        domain.to_lowercase()
    ))
}

fn from_report(report: &UsageFetchReport, observed_at: &str) -> QuotaReport {
    let mut pools: Vec<QuotaPool> = Vec::new();
    for output in &report.outputs {
        let provider: String = output.provider.trim().chars().take(60).collect();
        if provider.is_empty() {
            continue;
        }
        let Some(account_id) = identity(
            &provider,
            output.account.as_ref().map(|account| account.id.as_str()),
            output.email.as_deref(),
        ) else {
            continue;
        };
        if pools.iter().any(|pool| pool.account_id == account_id) || pools.len() >= 50 {
            continue;
        }
        let mut windows: Vec<QuotaWindow> = Vec::new();
        for metric in &output.metrics {
            let label: String = metric.label.chars().take(60).collect();
            if label.is_empty()
                || windows.len() >= 8
                || windows.iter().any(|window| window.label == label)
            {
                continue;
            }
            let normalized = label.trim().to_lowercase();
            let kind = if normalized.contains("weekly") || normalized == "week" {
                "weekly"
            } else if normalized.contains("daily") || normalized == "day" {
                "daily"
            } else if normalized.starts_with("5h") || normalized.contains("5-hour") {
                "5h"
            } else {
                "other"
            };
            let remaining = metric.remaining_percent;
            let remaining_percent =
                (remaining.is_finite() && (0.0..=100.0).contains(&remaining)).then_some(remaining);
            let resets_at = metric
                .resets_at
                .as_ref()
                .filter(|value| {
                    chrono::DateTime::parse_from_rfc3339(value)
                        .is_ok_and(|time| time.timestamp_millis() >= 0)
                })
                .cloned();
            windows.push(QuotaWindow {
                label,
                window: kind,
                used_percent: remaining_percent.map(|value| 100.0 - value),
                remaining_percent,
                resets_at,
            });
        }
        pools.push(QuotaPool {
            provider,
            account_id,
            account_label: text(
                output
                    .account
                    .as_ref()
                    .and_then(|account| account.label.as_deref()),
                60,
            ),
            email_masked: masked_email(output.email.as_deref()),
            plan: text(output.plan.as_deref(), 40),
            status: "ok",
            error: None,
            windows,
        });
    }
    for diagnostic in &report.diagnostics {
        if diagnostic.severity != UsageFetchDiagnosticSeverity::Error {
            continue;
        }
        let provider: String = diagnostic.provider.trim().chars().take(60).collect();
        if provider.is_empty() {
            continue;
        }
        let Some(account_id) = identity(
            &provider,
            diagnostic
                .account
                .as_ref()
                .map(|account| account.id.as_str()),
            None,
        ) else {
            continue;
        };
        let message = diagnostic.message.to_lowercase();
        let error = if ["401", "403", "needs_auth", "expired", "unauthorized"]
            .iter()
            .any(|value| message.contains(value))
        {
            "auth"
        } else if message.contains("timeout") || message.contains("timed out") {
            "timeout"
        } else if message.contains("429") || message.contains("rate limit") {
            "rate_limited"
        } else if message.contains("network") || message.contains("connect") {
            "network"
        } else {
            "unavailable"
        };
        if let Some(pool) = pools.iter_mut().find(|pool| pool.account_id == account_id) {
            pool.status = "error";
            pool.error = Some(error);
            for window in &mut pool.windows {
                window.used_percent = None;
                window.remaining_percent = None;
                window.resets_at = None;
            }
        } else if pools.len() < 50 {
            pools.push(QuotaPool {
                provider,
                account_id,
                account_label: text(
                    diagnostic
                        .account
                        .as_ref()
                        .and_then(|account| account.label.as_deref()),
                    60,
                ),
                email_masked: None,
                plan: None,
                status: "error",
                error: Some(error),
                windows: Vec::new(),
            });
        }
    }
    QuotaReport {
        version: 1,
        observed_at: observed_at.into(),
        pools,
    }
}

struct Upload {
    report: QuotaReport,
    legacy: String,
}

fn current() -> &'static Upload {
    static UPLOAD: OnceLock<Upload> = OnceLock::new();
    UPLOAD.get_or_init(|| {
        let report =
            crate::commands::usage::fetch_all_report_with_intent(UsageFetchIntent::CliReadOnly);
        Upload {
            report: from_report(&report, &chrono::Utc::now().to_rfc3339()),
            legacy: serde_json::to_string(&report.outputs).unwrap_or_else(|_| "[]".into()),
        }
    })
}

pub(crate) fn report() -> QuotaReport {
    current().report.clone()
}

pub(crate) fn attach(payload: &mut TsTokenContributionData, report: QuotaReport) {
    if payload.contributions.is_empty() {
        payload.contributions.push(crate::TsDailyContribution {
            date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            totals: crate::TsDailyTotals {
                tokens: 0,
                cost: 0.0,
                messages: 0,
                cost_is_complete: None,
                cost_provenance: None,
            },
            intensity: 0,
            token_breakdown: crate::TsTokenBreakdown {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            clients: Vec::new(),
            active_time_ms: None,
        });
    }
    payload.quota = Some(report);
}

pub(crate) fn legacy_body() -> String {
    current().legacy.clone()
}

pub(crate) struct SubmitReply {
    pub status: reqwest::StatusCode,
    pub body: Vec<u8>,
    pub quota_stored: bool,
}

async fn read_reply(
    mut response: reqwest::Response,
    embedded: bool,
) -> Result<SubmitReply, reqwest::Error> {
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = (64 * 1024usize).saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= 64 * 1024 {
            break;
        }
    }
    let quota_stored = embedded
        && status.is_success()
        && serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value.get("quota")?.get("pools")?.as_u64())
            .is_some();
    Ok(SubmitReply {
        status,
        body,
        quota_stored,
    })
}

pub(crate) async fn send(
    client: &reqwest::Client,
    api_url: &str,
    token: &str,
    payload: &TsTokenContributionData,
) -> Result<SubmitReply, reqwest::Error> {
    let mut plain;
    let payload = if crate::commands::autosubmit::is_official_api_url(api_url) {
        plain = payload.clone();
        plain.quota = None;
        &plain
    } else {
        payload
    };
    let response = client
        .post(format!("{api_url}/api/submit"))
        .bearer_auth(token)
        .json(payload)
        .send()
        .await?;
    let reply = read_reply(response, payload.quota.is_some()).await?;
    if payload.quota.is_some()
        && matches!(
            reply.status,
            reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNPROCESSABLE_ENTITY
        )
        && String::from_utf8_lossy(&reply.body)
            .to_lowercase()
            .contains("quota")
    {
        plain = payload.clone();
        plain.quota = None;
        let response = client
            .post(format!("{api_url}/api/submit"))
            .bearer_auth(token)
            .json(&plain)
            .send()
            .await?;
        return read_reply(response, false).await;
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::usage::{UsageFetchDiagnostic, UsageOutput};
    use serde_json::json;

    fn report() -> UsageFetchReport {
        let output: UsageOutput = serde_json::from_value(json!({
            "provider": "Codex", "account": {"id": "fixture-account", "label": "Work"},
            "plan": "pro", "email": "fixture@example.com",
            "metrics": [{"label": "Weekly", "used_percent": 40.0, "remaining_percent": 60.0, "remaining_label": null, "resets_at": "2026-10-27T09:00:00Z"}]
        })).unwrap();
        UsageFetchReport {
            outputs: vec![output],
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn embedded_quota_serializes_the_cafe_versioned_contract_without_raw_identity() {
        let value = serde_json::to_value(from_report(&report(), "2026-10-26T09:00:00Z")).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["observedAt"], "2026-10-26T09:00:00Z");
        assert_eq!(value["pools"][0]["windows"][0]["remainingPercent"], 60.0);
        assert_eq!(value["pools"][0]["windows"][0]["window"], "weekly");
        assert_eq!(value["pools"][0]["emailMasked"], "f***@example.com");
        let encoded = value.to_string();
        assert!(!encoded.contains("fixture-account"));
        assert!(!encoded.contains("fixture@example.com"));
        assert_eq!(value["pools"][0]["accountId"].as_str().unwrap().len(), 24);
        assert_eq!(value["pools"][0]["accountId"], "ca883d54c1b98d5591b7d32d");
    }

    #[test]
    fn embedded_quota_reports_identified_auth_failures_without_reusing_percentages() {
        let mut input = report();
        input.diagnostics.push(UsageFetchDiagnostic::new(
            "Codex",
            input.outputs[0].account.clone(),
            "HTTP 401",
        ));
        input
            .diagnostics
            .push(UsageFetchDiagnostic::new("Claude", None, "HTTP 401"));
        let value = serde_json::to_value(from_report(&input, "2026-10-26T09:00:00Z")).unwrap();
        assert_eq!(value["pools"].as_array().unwrap().len(), 1);
        assert_eq!(value["pools"][0]["status"], "error");
        assert_eq!(value["pools"][0]["error"], "auth");
        assert!(value["pools"][0]["windows"][0]["remainingPercent"].is_null());
        assert!(!value.to_string().contains("HTTP 401"));
    }

    #[test]
    fn embedded_quota_normalizes_email_identity_and_bounds_reports() {
        let mut input = report();
        input.outputs[0].account = None;
        input.outputs[0].email = Some(" Fixture@Example.com ".into());
        input.outputs[0].metrics[0].remaining_percent = 200.0;
        let one = serde_json::to_value(from_report(&input, "2026-10-26T09:00:00Z")).unwrap();
        input.outputs[0].email = Some("fixture@example.com".into());
        let two = serde_json::to_value(from_report(&input, "2026-10-26T09:00:00Z")).unwrap();
        assert_eq!(one["pools"][0]["accountId"], two["pools"][0]["accountId"]);
        assert!(one["pools"][0]["windows"][0]["remainingPercent"].is_null());
        input.outputs[0].account = Some(crate::commands::usage::UsageAccount {
            id: "fixture-account".into(),
            label: None,
            is_active: true,
        });
        input.outputs[0].metrics = (0..12)
            .map(|n| {
                let mut metric = input.outputs[0].metrics[0].clone();
                metric.label = format!("Window {n}");
                metric
            })
            .collect();
        input.outputs = (0..60)
            .map(|n| {
                let mut output = input.outputs[0].clone();
                output.account.as_mut().unwrap().id = format!("fixture-{n}");
                output
            })
            .collect();
        let value = serde_json::to_value(from_report(&input, "2026-10-26T09:00:00Z")).unwrap();
        assert_eq!(value["pools"].as_array().unwrap().len(), 50);
        assert_eq!(value["pools"][0]["windows"].as_array().unwrap().len(), 8);
    }

    fn payload() -> TsTokenContributionData {
        TsTokenContributionData {
            meta: crate::TsExportMeta {
                generated_at: "2026-10-26T09:00:00Z".into(),
                version: "fixture".into(),
                date_range: crate::DateRange {
                    start: "2026-10-26".into(),
                    end: "2026-10-26".into(),
                },
            },
            device: None,
            scan_scope: None,
            summary: crate::TsDataSummary {
                total_tokens: 0,
                total_cost: 0.0,
                total_days: 0,
                active_days: 0,
                average_per_day: 0.0,
                max_cost_in_single_day: 0.0,
                clients: Vec::new(),
                models: Vec::new(),
                cost_provenance: None,
            },
            years: Vec::new(),
            contributions: Vec::new(),
            time_metrics: None,
            mcp_servers: None,
            provenance: None,
            sessions: None,
            quota: None,
        }
    }

    fn server(
        responses: Vec<(u16, &'static str)>,
    ) -> (
        std::net::SocketAddr,
        std::thread::JoinHandle<Vec<(String, serde_json::Value)>>,
    ) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let thread = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(5))
                        }
                        Err(error) => panic!("fixture connection failed: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut chunk = [0; 4096];
                let (end, length) = loop {
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&chunk[..count]);
                    if let Some(end) = request.windows(4).position(|value| value == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap();
                        break (end + 4, length);
                    }
                };
                while request.len() < end + length {
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&chunk[..count]);
                }
                let path = String::from_utf8_lossy(&request[..end])
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                let json = serde_json::from_slice(&request[end..end + length]).unwrap();
                requests.push((path, json));
                write!(stream, "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        (address, thread)
    }

    #[test]
    fn embedded_quota_sends_one_submit_and_requires_explicit_quota_acknowledgement() {
        for (body, stored) in [
            (r#"{"quota":{"pools":1}}"#, true),
            (
                r#"{"warnings":["quota: report dropped, usage kept"]}"#,
                false,
            ),
            ("{}", false),
        ] {
            let (address, thread) = server(vec![(200, body)]);
            let mut input = payload();
            attach(&mut input, from_report(&report(), "2026-10-26T09:00:00Z"));
            let rt = tokio::runtime::Runtime::new().unwrap();
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let reply = rt
                .block_on(send(
                    &client,
                    &format!("http://{address}"),
                    "fixture-upload",
                    &input,
                ))
                .unwrap();
            assert_eq!(reply.quota_stored, stored);
            let requests = thread.join().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].0, "/api/submit");
            assert_eq!(requests[0].1["quota"]["version"], 1);
            assert_eq!(requests[0].1["contributions"][0]["totals"]["tokens"], 0);
            assert_eq!(requests[0].1["contributions"][0]["clients"], json!([]));
        }
    }

    #[test]
    fn embedded_quota_retries_only_explicit_quota_rejection_without_losing_usage() {
        for status in [400, 422] {
            let (address, thread) = server(vec![
                (status, r#"{"error":"unknown field quota"}"#),
                (200, "{}"),
            ]);
            let mut input = payload();
            attach(&mut input, from_report(&report(), "2026-10-26T09:00:00Z"));
            let rt = tokio::runtime::Runtime::new().unwrap();
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let reply = rt
                .block_on(send(
                    &client,
                    &format!("http://{address}"),
                    "fixture-upload",
                    &input,
                ))
                .unwrap();
            assert!(reply.status.is_success());
            assert!(!reply.quota_stored);
            let requests = thread.join().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].1["quota"]["version"], 1);
            assert!(requests[1].1.get("quota").is_none());
            assert_eq!(
                requests[0].1["contributions"],
                requests[1].1["contributions"]
            );
        }
        for status in [401, 429, 500] {
            let (address, thread) =
                server(vec![(status, r#"{"error":"quota upload unavailable"}"#)]);
            let mut input = payload();
            attach(&mut input, from_report(&report(), "2026-10-26T09:00:00Z"));
            let rt = tokio::runtime::Runtime::new().unwrap();
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let reply = rt
                .block_on(send(
                    &client,
                    &format!("http://{address}"),
                    "fixture-upload",
                    &input,
                ))
                .unwrap();
            assert_eq!(reply.status.as_u16(), status);
            assert!(!reply.quota_stored);
            assert_eq!(thread.join().unwrap().len(), 1);
        }
    }

    #[test]
    fn embedded_quota_is_omitted_for_official_receivers_and_no_quota_uploads() {
        let (address, thread) = server(vec![(200, "{}")]);
        let mut input = payload();
        attach(&mut input, from_report(&report(), "2026-10-26T09:00:00Z"));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .resolve("tokscale.ai", address)
            .build()
            .unwrap();
        rt.block_on(send(
            &client,
            &format!("http://tokscale.ai:{}", address.port()),
            "fixture-upload",
            &input,
        ))
        .unwrap();
        assert!(thread.join().unwrap()[0].1.get("quota").is_none());
        let (address, thread) = server(vec![(200, "{}")]);
        rt.block_on(send(
            &client,
            &format!("http://{address}"),
            "fixture-upload",
            &payload(),
        ))
        .unwrap();
        assert!(thread.join().unwrap()[0].1.get("quota").is_none());
    }

    #[test]
    fn embedded_quota_uses_the_legacy_endpoint_only_when_the_submit_omits_ack() {
        for stored in [true, false] {
            let mut responses = vec![(
                200,
                if stored {
                    r#"{"quota":{"pools":1}}"#
                } else {
                    "{}"
                },
            )];
            if !stored {
                responses.push((200, "{}"));
            }
            let (address, thread) = server(responses);
            let url = format!("http://{address}");
            let mut input = payload();
            attach(&mut input, from_report(&report(), "2026-10-26T09:00:00Z"));
            let rt = tokio::runtime::Runtime::new().unwrap();
            let reply = rt
                .block_on(send(
                    &reqwest::Client::builder().no_proxy().build().unwrap(),
                    &url,
                    "fixture-upload",
                    &input,
                ))
                .unwrap();
            let auth = crate::auth::ApiTokenAuth {
                token: "fixture-upload".into(),
                username: None,
                source: crate::auth::ApiTokenSource::StoredCredentials,
                api_url: Some(url.clone()),
            };
            let legacy = serde_json::to_string(&report().outputs).unwrap();
            let result = crate::commands::autosubmit::after_submit_quota(
                false,
                &url,
                reply.quota_stored,
                || crate::commands::autosubmit::upload_quota(&auth, &url, &legacy),
                |_| panic!("unexpected quota failure"),
            );
            assert!(result.unwrap().is_ok());
            let requests = thread.join().unwrap();
            assert_eq!(requests.len(), if stored { 1 } else { 2 });
            if !stored {
                assert_eq!(requests[1].0, "/api/quota");
                assert!(requests[1].1.is_array());
            }
        }
    }
}
