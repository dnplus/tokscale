//! Local event accounting is independent of quota fetching and filesystem IO.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use tokscale_core::{sessions::cursor::account_id_from_cursor_cache_path, UnifiedMessage};

use super::{cursor, UsageMetric, UsageOutput};

#[derive(Debug, Default, Serialize)]
struct Row {
    id: String,
    calls: usize,
    cost_usd: f64,
    share_percent: f64,
    distinct_conversations: usize,
}

#[derive(Debug, Serialize)]
struct Breakdown {
    rows: Vec<Row>,
    calls: usize,
    cost_week_usd: f64,
    cost_24h_usd: f64,
    excluded_calls: usize,
    excluded_cost_usd: f64,
}

fn is_uuid(id: &str) -> bool {
    id.len() == 36 && uuid::Uuid::parse_str(id).is_ok()
}

fn summarize(events: &[UnifiedMessage], now: DateTime<Utc>, start: DateTime<Utc>) -> Breakdown {
    // Identify across the entire cache: the bot's router event may precede this week.
    let bots: BTreeSet<&str> = events
        .iter()
        .filter(|e| is_uuid(&e.session_id) && e.model_id.starts_with("grok-bot-"))
        .map(|e| e.session_id.as_str())
        .collect();
    let mut groups: BTreeMap<String, (Row, BTreeSet<String>)> = BTreeMap::new();
    let mut result = Breakdown {
        rows: vec![],
        calls: 0,
        cost_week_usd: 0.0,
        cost_24h_usd: 0.0,
        excluded_calls: 0,
        excluded_cost_usd: 0.0,
    };
    for event in events {
        if event.timestamp < start.timestamp_millis() || event.timestamp > now.timestamp_millis() {
            continue;
        }
        let cost = if event.cost.is_finite() && event.cost >= 0.0 {
            event.cost
        } else {
            0.0
        };
        let group = if event
            .session_id
            .strip_prefix("sand-subagent-")
            .is_some_and(is_uuid)
        {
            Some(if event.model_id == "grok-bot-automation" {
                "schedules"
            } else {
                "subtasks"
            })
        } else if bots.contains(event.session_id.as_str()) {
            Some(event.session_id.as_str())
        } else {
            None
        };
        let Some(group) = group else {
            result.excluded_calls += 1;
            result.excluded_cost_usd += cost;
            continue;
        };
        let (row, conversations) = groups.entry(group.to_string()).or_default();
        row.id = group.to_string();
        row.calls += 1;
        row.cost_usd += cost;
        conversations.insert(event.session_id.clone());
        result.calls += 1;
        result.cost_week_usd += cost;
        if event.timestamp >= (now - Duration::hours(24)).timestamp_millis() {
            result.cost_24h_usd += cost;
        }
    }
    result.rows = groups
        .into_values()
        .map(|(mut row, ids)| {
            row.distinct_conversations = ids.len();
            if result.cost_week_usd > 0.0 {
                row.share_percent = row.cost_usd / result.cost_week_usd * 100.0;
            }
            row
        })
        .collect();
    result
}

#[derive(Debug, Serialize)]
struct BurnRate {
    percent_per_hour: f64,
    exhausted_at: DateTime<Utc>,
    before_reset: bool,
    hours_before_reset: f64,
    remaining_at_reset_percent: f64,
}

fn estimate(
    data: &Breakdown,
    now: DateTime<Utc>,
    reset: Option<DateTime<Utc>>,
    used: Option<f64>,
) -> Option<BurnRate> {
    let reset = reset.filter(|reset| *reset > now)?;
    let used = used.filter(|v| v.is_finite() && *v > 0.0)?;
    if data.cost_week_usd <= 0.0 || data.cost_24h_usd <= 0.0 {
        return None;
    }
    let rate = used * (data.cost_24h_usd / data.cost_week_usd) / 24.0;
    let hours = (100.0 - used).max(0.0) / rate;
    if !rate.is_finite() || rate <= 0.0 || !hours.is_finite() {
        return None;
    }
    let exhausted_at = now.checked_add_signed(Duration::try_seconds((hours * 3600.0) as i64)?)?;
    let until_reset = (reset - now).num_seconds() as f64 / 3600.0;
    Some(BurnRate {
        percent_per_hour: rate,
        exhausted_at,
        before_reset: exhausted_at < reset,
        hours_before_reset: (until_reset - hours).max(0.0),
        remaining_at_reset_percent: (100.0 - used - rate * until_reset).max(0.0),
    })
}

#[derive(Serialize)]
struct AccountReport {
    cache_account_id: Option<String>,
    quota: Option<UsageOutput>,
    status: String,
    week_start: DateTime<Utc>,
    reset_at: Option<DateTime<Utc>>,
    breakdown: Option<Breakdown>,
    burn_rate: Option<BurnRate>,
}

fn weekly(card: &UsageOutput) -> Option<&UsageMetric> {
    card.metrics.iter().find(|m| m.label == "Weekly")
}

fn report(
    id: Option<String>,
    card: Option<UsageOutput>,
    events: Option<&[UnifiedMessage]>,
    now: DateTime<Utc>,
    since: Option<DateTime<Utc>>,
) -> AccountReport {
    let metric = card.as_ref().and_then(weekly);
    let reset = metric
        .and_then(|m| m.resets_at.as_deref())
        .and_then(|r| DateTime::parse_from_rfc3339(r).ok())
        .map(|r| r.with_timezone(&Utc));
    let valid_reset = reset.filter(|r| *r > now && *r - Duration::days(7) <= now);
    let start = valid_reset
        .map(|r| r - Duration::days(7))
        .unwrap_or_else(|| since.unwrap_or(now - Duration::days(7)));
    let data = events.map(|e| summarize(e, now, start));
    let burn = data
        .as_ref()
        .and_then(|d| estimate(d, now, valid_reset, metric.map(|m| m.used_percent)));
    let status = if events.is_none() {
        "This account has no local Cursor usage cache"
    } else if card.is_none() {
        "Cannot map cache to a Grok Bot account; fallback window"
    } else if valid_reset.is_none() {
        "Weekly reset unavailable; fallback window"
    } else {
        "Weekly window"
    };
    AccountReport {
        cache_account_id: id,
        quota: card,
        status: status.into(),
        week_start: start,
        reset_at: reset,
        breakdown: data,
        burn_rate: burn,
    }
}

fn account_reports(
    cards: Vec<UsageOutput>,
    mut caches: BTreeMap<String, Vec<UnifiedMessage>>,
    email: Option<&str>,
    now: DateTime<Utc>,
    since: Option<DateTime<Utc>>,
) -> Vec<AccountReport> {
    let mut reports = vec![];
    for card in cards {
        let matched = email
            .zip(card.email.as_deref())
            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b));
        let events = if matched {
            caches.remove("active")
        } else {
            None
        };
        reports.push(report(
            events.as_ref().map(|_| "active".into()),
            Some(card),
            events.as_deref(),
            now,
            since,
        ));
    }
    for (id, events) in caches {
        reports.push(report(Some(id), None, Some(&events), now, since));
    }
    reports
}

pub fn run(json: bool, since: Option<DateTime<Utc>>) -> Result<()> {
    let now = Utc::now();
    anyhow::ensure!(
        since.is_none_or(|s| s <= now),
        "--since must not be in the future"
    );
    let cards = cursor::fetch_grok_bot_all();
    let mut diagnostics = vec![];
    let cards = match cards {
        Ok(cards) => cards,
        Err(e) => {
            diagnostics.push(format!("Weekly quota unavailable: {e}"));
            vec![]
        }
    };
    let email = cursor::local_cursor_email();
    let mut caches = BTreeMap::new();
    let dir = crate::cursor::get_cursor_cache_dir()?;
    if dir.exists() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name == "usage.json" || (name.starts_with("usage.") && name.ends_with(".json")) {
                let id = account_id_from_cursor_cache_path(&path);
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        if serde_json::from_str::<serde_json::Value>(&content)
                            .ok()
                            .and_then(|v| {
                                v.get("usageEventsDisplay")
                                    .and_then(|v| v.as_array())
                                    .map(|_| ())
                            })
                            .is_none()
                        {
                            diagnostics.push(format!("Invalid Cursor usage cache: {name}"));
                            continue;
                        }
                        caches.insert(
                            id.clone(),
                            tokscale_core::sessions::cursor::parse_cursor_events_json_content(
                                &content, &id,
                            ),
                        );
                    }
                    Err(e) => {
                        diagnostics.push(format!("Cannot read Cursor usage cache {name}: {e}"))
                    }
                }
            }
        }
    }
    let reports = account_reports(cards, caches, email.as_deref(), now, since);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"accounts": reports, "diagnostics": diagnostics})
            )?
        );
    } else {
        for diagnostic in diagnostics {
            println!("{diagnostic}");
        }
        if reports.is_empty() {
            println!("No Grok Bot accounts or local Cursor usage caches found");
        }
        for account in reports {
            render(&account);
        }
    }
    Ok(())
}

fn display_time(time: DateTime<Utc>) -> String {
    time.with_timezone(&chrono::Local)
        .format("%a %b %d %Y %H:%M %:z")
        .to_string()
}

fn render(account: &AccountReport) {
    let title = account
        .quota
        .as_ref()
        .map(UsageOutput::display_name)
        .unwrap_or_else(|| {
            format!(
                "Cursor cache ({})",
                account.cache_account_id.as_deref().unwrap_or("unknown")
            )
        });
    println!("\n{title} · {}", account.status);
    if let Some(card) = &account.quota {
        if let Some(email) = &card.email {
            println!("  {email}");
        }
        if let Some(metric) = weekly(card) {
            println!("  Weekly: {:.1}% remaining", metric.remaining_percent);
        }
    }
    println!("  Since: {}", display_time(account.week_start));
    if let Some(reset) = account.reset_at {
        println!("  Reset: {}", display_time(reset));
    }
    let Some(data) = &account.breakdown else {
        return;
    };
    println!(
        "  {:<32} {:>8} {:>12} {:>8}",
        "Bot", "Calls", "Cost USD", "Share"
    );
    for row in &data.rows {
        let label = match row.id.as_str() {
            "subtasks" => format!("Subtasks ({} distinct)", row.distinct_conversations),
            "schedules" => format!(
                "Schedules/automation ({} distinct)",
                row.distinct_conversations
            ),
            _ => row.id.chars().take(8).collect(),
        };
        println!(
            "  {label:<32} {:>8} {:>12.2} {:>7.1}%",
            row.calls, row.cost_usd, row.share_percent
        );
    }
    println!(
        "  {:<32} {:>8} {:>12.2}",
        "Total", data.calls, data.cost_week_usd
    );
    print!("  Last 24h: ${:.2}", data.cost_24h_usd);
    match &account.burn_rate {
        Some(burn) => {
            print!(
                " ({:.3}%/h) → estimated exhaustion {}",
                burn.percent_per_hour,
                display_time(burn.exhausted_at)
            );
            if burn.before_reset {
                println!("; before reset ({:.1}h early)", burn.hours_before_reset);
            } else {
                println!(
                    "; lasts until reset ({:.1}% remaining at reset)",
                    burn.remaining_at_reset_percent
                );
            }
        }
        None => println!("; cannot estimate quota %/h or exhaustion"),
    }
    println!(
        "  Non-Grok Bot Cursor usage (excluded): {} calls / ${:.2}",
        data.excluded_calls, data.excluded_cost_usd
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokscale_core::sessions::cursor::parse_cursor_events_json_content;

    fn time(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn synthetic_json_groups_conversations_and_costs() {
        let now = time("2026-10-08T00:00:00Z");
        let start = now - Duration::days(7);
        let bot = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let sub = "sand-subagent-11111111-2222-3333-4444-555555555555";
        let mut rows = vec![];
        for (id, model, hours, cents, charged) in [
            (bot, "grok-bot-default", 200, 50.0, 0.0),
            (bot, "claude-opus-synthetic", 1, 100.0, 0.0),
            (sub, "claude-opus-synthetic", 2, -1.0, 50.0),
            (sub, "grok-bot-automation", 3, 25.0, 0.0),
            (
                "bbbbbbbb-cccc-dddd-eeee-ffffffffffff",
                "default",
                1,
                20.0,
                0.0,
            ),
            ("bc-synthetic", "grok-bot-default", 1, 20.0, 0.0),
            (bot, "grok-bot-default", -1, 90.0, 0.0),
        ] {
            rows.push(serde_json::json!({"conversationId":id,"model":model,"timestamp":(now - Duration::hours(hours)).timestamp_millis().to_string(),"tokenUsage":{"totalCents":cents},"chargedCents":charged}));
        }
        let events = parse_cursor_events_json_content(
            &serde_json::json!({"usageEventsDisplay":rows}).to_string(),
            "synthetic",
        );
        let data = summarize(&events, now, start);
        assert_eq!(data.calls, 3);
        assert_eq!(data.rows.len(), 3);
        assert_eq!(data.cost_week_usd, 1.75);
        assert_eq!(data.cost_24h_usd, 1.75);
        assert_eq!(data.excluded_calls, 2);
        assert_eq!(data.excluded_cost_usd, 0.4);
        assert!((data.rows.iter().map(|r| r.share_percent).sum::<f64>() - 100.0).abs() < 1e-9);
        assert_eq!(data.rows[0].id, bot);
    }

    #[test]
    fn burn_rate_before_after_reset_and_unavailable() {
        let now = time("2026-10-08T00:00:00Z");
        let data = Breakdown {
            rows: vec![],
            calls: 1,
            cost_week_usd: 100.0,
            cost_24h_usd: 24.0,
            excluded_calls: 0,
            excluded_cost_usd: 0.0,
        };
        let burn = estimate(&data, now, Some(now + Duration::hours(48)), Some(80.0)).unwrap();
        assert!((burn.percent_per_hour - 0.8).abs() < 1e-9);
        assert_eq!(burn.exhausted_at, now + Duration::hours(25));
        assert!(burn.before_reset);
        assert!((burn.hours_before_reset - 23.0).abs() < 1e-9);
        let burn = estimate(&data, now, Some(now + Duration::hours(10)), Some(80.0)).unwrap();
        assert!(!burn.before_reset);
        assert!((burn.remaining_at_reset_percent - 12.0).abs() < 1e-9);
        assert!(estimate(&data, now, None, Some(80.0)).is_none());
        assert!(estimate(&data, now, Some(now), Some(80.0)).is_none());
        assert!(estimate(&data, now, Some(now + Duration::days(1)), Some(f64::NAN)).is_none());
        let empty = summarize(&[], now, now - Duration::days(7));
        assert!(estimate(&empty, now, Some(now + Duration::days(1)), Some(80.0)).is_none());
    }

    #[test]
    fn account_mapping_and_weekly_window() {
        let now = time("2026-10-08T00:00:00Z");
        let reset = now + Duration::days(3);
        let card: UsageOutput = serde_json::from_value(serde_json::json!({
            "provider":"Grok Bot", "plan":null, "email":"synthetic@example.invalid",
            "metrics":[{"label":"Weekly","used_percent":60.0,"remaining_percent":40.0,
            "remaining_label":null,"resets_at":reset.to_rfc3339()}]
        }))
        .unwrap();
        let caches = BTreeMap::from([
            ("active".into(), vec![]),
            ("synthetic-secondary".into(), vec![]),
        ]);
        let results = account_reports(
            vec![card.clone()],
            caches,
            Some("SYNTHETIC@example.invalid"),
            now,
            Some(now - Duration::days(1)),
        );
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].cache_account_id.as_deref(), Some("active"));
        assert_eq!(results[0].week_start, reset - Duration::days(7));
        assert!(results[1].quota.is_none());
        let results = account_reports(vec![card], BTreeMap::new(), None, now, None);
        assert!(results[0].breakdown.is_none());
        assert_eq!(results[0].reset_at, Some(reset));
    }

    #[test]
    fn fallback_since_and_missing_cache() {
        let now = time("2026-10-08T00:00:00Z");
        let since = now - Duration::days(2);
        let result = report(Some("synthetic".into()), None, Some(&[]), now, Some(since));
        assert_eq!(result.week_start, since);
        assert!(result.burn_rate.is_none());
        let result = report(None, None, None, now, None);
        assert!(result.breakdown.is_none());
        assert!(result.status.contains("no local"));
    }
}
