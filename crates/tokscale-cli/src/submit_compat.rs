//! Receiver-first rollout for the atomic Cursor/Grok Bot accounting family.
//! Never fold Bot back into Cursor: a migrated ledger may already contain both.
use std::borrow::Cow;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::StatusCode;

use crate::{
    DateRange, TsDataSummary, TsTokenContributionData, TsYearSummary,
    CURSOR_SUBMISSION_PARSER_VERSION, GROK_BOT_SUBMISSION_PARSER_VERSION,
};

fn is_family(client: &str) -> bool {
    matches!(client, "cursor" | "grok-bot")
}

fn touches_family(payload: &TsTokenContributionData) -> bool {
    payload
        .summary
        .clients
        .iter()
        .any(|client| is_family(client))
        || payload
            .contributions
            .iter()
            .any(|day| day.clients.iter().any(|cell| is_family(&cell.client)))
        || payload
            .scan_scope
            .as_ref()
            .is_some_and(|scope| scope.parser_versions.keys().any(|client| is_family(client)))
}

/// Only definitive protocol responses permit a legacy fallback. Connection,
/// auth and server errors never turn into silent loss of the submitted family.
fn supports_family(status: StatusCode, body: &[u8]) -> Result<bool> {
    if matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    ) {
        return Ok(false);
    }
    anyhow::ensure!(
        status.is_success(),
        "Submission capability check failed with HTTP {status}; no usage was uploaded"
    );
    let body: serde_json::Value = serde_json::from_slice(body)
        .context("Invalid submission capability response; no usage was uploaded")?;
    let versions = &body["capabilities"]["cursorGrokBot"]["parserVersions"];
    Ok(
        versions["cursor"].as_u64() == Some(CURSOR_SUBMISSION_PARSER_VERSION as u64)
            && versions["grok-bot"].as_u64() == Some(GROK_BOT_SUBMISSION_PARSER_VERSION as u64),
    )
}

const MAX_CAPABILITY_BYTES: usize = 64 * 1024;

async fn receiver_supports_family(api_url: &str) -> Result<bool> {
    let mut response = tokscale_core::http::client()
        .get(format!("{}/api/submit", api_url.trim_end_matches('/')))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("Could not check submission capabilities; no usage was uploaded")?;
    let status = response.status();
    // Older Next receivers return a body-less 405. Other errors need no body.
    if !status.is_success() {
        return supports_family(status, &[]);
    }
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= MAX_CAPABILITY_BYTES as u64),
        "Submission capability response exceeds 64 KiB; no usage was uploaded"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("Could not read submission capabilities; no usage was uploaded")?
    {
        anyhow::ensure!(
            chunk.len() <= MAX_CAPABILITY_BYTES - body.len(),
            "Submission capability response exceeds 64 KiB; no usage was uploaded"
        );
        body.extend_from_slice(&chunk);
    }
    supports_family(status, &body)
}

pub(crate) async fn prepare_submission<'a>(
    api_url: &str,
    payload: &'a TsTokenContributionData,
) -> Result<Cow<'a, TsTokenContributionData>> {
    if !touches_family(payload) {
        return Ok(Cow::Borrowed(payload));
    }
    let supported = receiver_supports_family(api_url).await?;
    if !supported {
        eprintln!("  This receiver does not support atomic Cursor/Grok Bot accounting. Both clients are excluded from this upload; their previously credited history is unchanged. Local statistics are unaffected. Upgrade the receiver to submit this family.");
    }
    prepare_for_capability(payload, supported).context(
        "The receiver does not support Cursor/Grok Bot and no compatible usage remains; no usage was uploaded. Upgrade the receiver before retrying."
    )
}

fn prepare_for_capability(
    payload: &TsTokenContributionData,
    supported: bool,
) -> Option<Cow<'_, TsTokenContributionData>> {
    if supported || !touches_family(payload) {
        return Some(Cow::Borrowed(payload));
    }
    let mut filtered = payload.clone();
    if let Some(scope) = &mut filtered.scan_scope {
        scope.parser_versions.retain(|client, _| !is_family(client));
    }
    let mut removed_usage = false;
    for day in &mut filtered.contributions {
        let previous_count = day.clients.len();
        day.clients.retain(|cell| !is_family(&cell.client));
        if day.clients.len() != previous_count {
            removed_usage = true;
            // Mixed-day time cannot be attributed to retained clients safely.
            day.active_time_ms = None;
        }
    }
    filtered.contributions.retain(|day| !day.clients.is_empty());
    if filtered.contributions.is_empty() {
        return None;
    }
    if removed_usage {
        filtered.time_metrics = None;
    }

    // Reuse the report's canonical saturated token aggregation and summary /
    // year / intensity functions, rather than subtracting from old totals.
    let mut days: Vec<tokscale_core::DailyContribution> = filtered
        .contributions
        .iter()
        .map(|day| {
            let clients: Vec<tokscale_core::ClientContribution> = day
                .clients
                .iter()
                .map(|cell| tokscale_core::ClientContribution {
                    client: cell.client.clone(),
                    model_id: cell.model_id.clone(),
                    provider_id: cell.provider_id.clone().unwrap_or_default(),
                    tokens: tokscale_core::TokenBreakdown {
                        input: cell.tokens.input,
                        output: cell.tokens.output,
                        cache_read: cell.tokens.cache_read,
                        cache_write: cell.tokens.cache_write,
                        reasoning: cell.tokens.reasoning,
                        ..Default::default()
                    },
                    cost: cell.cost,
                    messages: cell.messages,
                })
                .collect();
            let mut token_breakdown = tokscale_core::TokenBreakdown::default();
            for cell in &clients {
                token_breakdown += &cell.tokens;
            }
            let totals = tokscale_core::DailyTotals {
                tokens: token_breakdown.total(),
                cost: clients.iter().map(|cell| cell.cost).sum::<f64>() + 0.0,
                messages: clients
                    .iter()
                    .fold(0i32, |sum, cell| sum.saturating_add(cell.messages)),
            };
            tokscale_core::DailyContribution {
                date: day.date.clone(),
                totals,
                intensity: 0,
                token_breakdown,
                clients,
                active_time_ms: day.active_time_ms,
            }
        })
        .collect();
    tokscale_core::calculate_intensities(&mut days);
    let summary = tokscale_core::calculate_summary(&days);
    filtered.summary = TsDataSummary {
        total_tokens: summary.total_tokens,
        total_cost: summary.total_cost,
        total_days: summary.total_days,
        active_days: summary.active_days,
        average_per_day: summary.average_per_day,
        max_cost_in_single_day: summary.max_cost_in_single_day,
        clients: summary.clients,
        models: summary.models,
    };
    filtered.years = tokscale_core::calculate_years(&days)
        .into_iter()
        .map(|year| TsYearSummary {
            year: year.year,
            total_tokens: year.total_tokens,
            total_cost: year.total_cost,
            range: DateRange {
                start: year.range_start,
                end: year.range_end,
            },
        })
        .collect();
    for (target, day) in filtered.contributions.iter_mut().zip(&days) {
        target.totals.tokens = day.totals.tokens;
        target.totals.cost = day.totals.cost;
        target.totals.messages = day.totals.messages;
        // Keep incomplete-pricing evidence conservative after removing clients.
        target.intensity = day.intensity;
        target.token_breakdown = crate::TsTokenBreakdown {
            input: day.token_breakdown.input,
            output: day.token_breakdown.output,
            cache_read: day.token_breakdown.cache_read,
            cache_write: day.token_breakdown.cache_write,
            reasoning: day.token_breakdown.reasoning,
        };
    }
    filtered.meta.date_range = DateRange {
        start: days.iter().map(|day| &day.date).min().unwrap().clone(),
        end: days.iter().map(|day| &day.date).max().unwrap().clone(),
    };
    Some(Cow::Owned(filtered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        TsDailyContribution, TsDailyTotals, TsExportMeta, TsSourceContribution, TsTokenBreakdown,
    };

    fn payload(clients: &[&str]) -> TsTokenContributionData {
        let mut graph = TsTokenContributionData {
            meta: TsExportMeta {
                generated_at: "2026-10-08T00:00:00Z".into(),
                version: "test".into(),
                date_range: DateRange {
                    start: "2026-10-07".into(),
                    end: "2026-10-08".into(),
                },
            },
            device: Some(crate::TsSubmitDevice {
                id: "device-test".into(),
                name: None,
            }),
            scan_scope: crate::submit_scan_scope(
                Some(&clients.iter().map(|c| c.to_string()).collect::<Vec<_>>()),
                true,
            ),
            summary: TsDataSummary {
                total_tokens: 0,
                total_cost: 0.0,
                total_days: 0,
                active_days: 0,
                average_per_day: 0.0,
                max_cost_in_single_day: 0.0,
                clients: clients.iter().map(|c| c.to_string()).collect(),
                models: vec![],
            },
            years: vec![],
            contributions: vec![],
            time_metrics: Some(crate::TsTimeMetrics {
                total_active_time_ms: 100,
                longest_continuous_ms: 100,
                max_concurrent_sessions: 1,
                session_count: 1,
            }),
            mcp_servers: Some(vec!["kept".into()]),
            provenance: None,
        };
        for (index, client) in clients.iter().enumerate() {
            graph.contributions.push(TsDailyContribution {
                date: if index == 0 {
                    "2026-10-07"
                } else {
                    "2026-10-08"
                }
                .into(),
                totals: TsDailyTotals {
                    tokens: 100,
                    cost: 1.0,
                    messages: 1,
                    cost_is_complete: Some(false),
                },
                intensity: 4,
                token_breakdown: TsTokenBreakdown {
                    input: 70,
                    output: 20,
                    cache_read: 5,
                    cache_write: 4,
                    reasoning: 1,
                },
                clients: vec![TsSourceContribution {
                    client: client.to_string(),
                    model_id: format!("{client}-model"),
                    provider_id: Some("provider".into()),
                    tokens: TsTokenBreakdown {
                        input: 70,
                        output: 20,
                        cache_read: 5,
                        cache_write: 4,
                        reasoning: 1,
                    },
                    cost: 1.0,
                    messages: 1,
                }],
                active_time_ms: Some(100),
            });
        }
        graph
    }

    #[test]
    fn submit_compat_legacy_removes_family_everywhere_and_rebuilds_totals() {
        let original = payload(&["cursor", "grok-bot", "claude"]);
        let before = serde_json::to_value(&original).unwrap();
        let filtered = prepare_for_capability(&original, false).unwrap();
        let json = serde_json::to_value(&*filtered).unwrap();
        let wire = serde_json::to_string(&*filtered).unwrap();
        assert!(!wire.contains("grok-bot"));
        assert!(!wire.contains("cursor"));
        assert_eq!(json["summary"]["clients"], serde_json::json!(["claude"]));
        assert_eq!(
            json["summary"]["models"],
            serde_json::json!(["claude-model"])
        );
        assert_eq!(json["summary"]["totalTokens"], 100);
        assert_eq!(json["summary"]["totalCost"], 1.0);
        assert_eq!(json["summary"]["totalDays"], 1);
        assert_eq!(json["years"][0]["totalTokens"], 100);
        assert_eq!(json["meta"]["dateRange"]["start"], "2026-10-08");
        assert_eq!(
            json["scanScope"]["parserVersions"],
            serde_json::json!({"claude": 1})
        );
        assert_eq!(
            json["contributions"][0]["clients"][0],
            before["contributions"][2]["clients"][0]
        );
        assert_eq!(json["contributions"][0]["totals"]["costIsComplete"], false);
        assert!(json.get("timeMetrics").is_none());
        assert_eq!(json["contributions"][0]["activeTimeMs"], 100);
        assert_eq!(json["device"], before["device"]);
        assert_eq!(json["mcpServers"], before["mcpServers"]);
        assert_eq!(serde_json::to_value(&original).unwrap(), before);
    }

    #[test]
    fn submit_compat_legacy_rebuilds_mixed_day_token_buckets() {
        let mut original = payload(&["cursor", "grok-bot", "claude"]);
        let mut retained = original.contributions.pop().unwrap();
        let bot = original.contributions.pop().unwrap();
        retained.clients.extend(bot.clients);
        original.contributions.push(retained);
        let filtered = prepare_for_capability(&original, false).unwrap();
        assert_eq!(filtered.contributions.len(), 1);
        assert_eq!(filtered.contributions[0].totals.tokens, 100);
        assert_eq!(filtered.contributions[0].token_breakdown.reasoning, 1);
        assert_eq!(filtered.contributions[0].totals.messages, 1);
        assert_eq!(filtered.contributions[0].clients.len(), 1);
        assert!(filtered.contributions[0].active_time_ms.is_none());
    }

    #[test]
    fn submit_compat_legacy_family_only_has_no_post_payload() {
        assert!(prepare_for_capability(&payload(&["cursor", "grok-bot"]), false).is_none());
        assert!(prepare_for_capability(&payload(&["grok-bot"]), false).is_none());
    }

    #[test]
    fn submit_compat_capable_and_unrelated_payloads_are_unchanged() {
        for (input, supported) in [
            (payload(&["cursor", "grok-bot", "claude"]), true),
            (payload(&["claude"]), false),
        ] {
            let output = prepare_for_capability(&input, supported).unwrap();
            assert!(matches!(output, Cow::Borrowed(_)));
            assert_eq!(
                serde_json::to_value(&*output).unwrap(),
                serde_json::to_value(&input).unwrap()
            );
        }
    }

    #[test]
    fn submit_compat_empty_family_scan_keys_are_removed_without_losing_unrelated_time() {
        let mut input = payload(&["claude"]);
        let scope = input.scan_scope.as_mut().unwrap();
        scope.parser_versions.insert("cursor".into(), 4);
        scope.parser_versions.insert("grok-bot".into(), 1);
        let filtered = prepare_for_capability(&input, false).unwrap();
        let json = serde_json::to_value(&*filtered).unwrap();
        assert_eq!(
            json["scanScope"]["parserVersions"],
            serde_json::json!({"claude":1})
        );
        assert_eq!(json["timeMetrics"]["totalActiveTimeMs"], 100);
        assert_eq!(json["contributions"][0]["activeTimeMs"], 100);
    }

    #[test]
    fn submit_compat_capability_requires_exact_atomic_pair() {
        let supported = serde_json::json!({"capabilities":{"cursorGrokBot":{"parserVersions":{"cursor":4,"grok-bot":1}}}});
        assert!(supports_family(StatusCode::OK, &serde_json::to_vec(&supported).unwrap()).unwrap());
        for body in [
            serde_json::json!({}),
            serde_json::json!({"capabilities":{"cursorGrokBot":{"parserVersions":{"cursor":4}}}}),
            serde_json::json!({"capabilities":{"cursorGrokBot":{"parserVersions":{"cursor":5,"grok-bot":1}}}}),
        ] {
            assert!(!supports_family(StatusCode::OK, &serde_json::to_vec(&body).unwrap()).unwrap());
        }
        assert!(!supports_family(StatusCode::NOT_FOUND, b"").unwrap());
        assert!(!supports_family(StatusCode::METHOD_NOT_ALLOWED, b"").unwrap());
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            assert!(supports_family(status, b"{}").is_err());
        }
        assert!(supports_family(StatusCode::OK, b"not-json").is_err());
    }

    #[test]
    fn submit_compat_network_failure_does_not_fall_back() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let input = payload(&["cursor", "grok-bot", "claude"]);
        let result = runtime.block_on(prepare_submission(&format!("http://{address}"), &input));
        assert!(result.is_err());
    }

    #[test]
    fn submit_compat_rejects_oversized_fixed_and_chunked_responses() {
        use std::io::{Read, Write};
        for chunked in [false, true] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = [0u8; 4096];
                let count = socket.read(&mut request).unwrap();
                assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /api/submit "));
                if chunked {
                    // No Content-Length: the accumulated-body guard must fire.
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
                    for chunk in [vec![b' '; MAX_CAPABILITY_BYTES], vec![b' '; 1]] {
                        let _ = write!(socket, "{:x}\r\n", chunk.len());
                        let _ = socket.write_all(&chunk);
                        let _ = socket.write_all(b"\r\n");
                    }
                    let _ = socket.write_all(b"0\r\n\r\n");
                } else {
                    // Reject the declared size without trying to read a body.
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        MAX_CAPABILITY_BYTES + 1
                    );
                }
            });
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let error = runtime
                .block_on(receiver_supports_family(&format!("http://{address}")))
                .unwrap_err();
            assert!(error.to_string().contains("exceeds 64 KiB"), "{error:#}");
            server.join().unwrap();
        }
    }

    #[test]
    fn submit_compat_family_only_legacy_returns_error_not_autosubmit_success() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0u8; 4096];
            let count = socket.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /api/submit "));
            socket.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let input = payload(&["cursor", "grok-bot"]);
        let result = runtime.block_on(prepare_submission(&format!("http://{address}"), &input));
        let error = result
            .err()
            .expect("no POST payload must propagate an error to autosubmit");
        assert!(error.to_string().contains("no compatible usage remains"));
        server.join().unwrap();
    }

    #[test]
    fn submit_compat_no_family_skips_network_probe() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let input = payload(&["claude"]);
        let result = runtime
            .block_on(prepare_submission("not-a-url", &input))
            .unwrap();
        assert!(matches!(result, Cow::Borrowed(_)));
    }
}
