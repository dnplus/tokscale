//! Local event accounting is independent of quota fetching and filesystem IO.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use tokscale_core::{
    sessions::cursor::{account_id_from_cursor_cache_path, CursorUsageRecord},
    TokenBreakdown,
};

use super::{cursor, UsageMetric, UsageOutput};

#[derive(Debug, Default, Serialize)]
struct Row {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    calls: usize,
    cost_usd: f64,
    share_percent: f64,
    distinct_conversations: usize,
    tokens: TokenBreakdown,
    missing_token_usage_calls: usize,
}

#[derive(Debug, Serialize)]
struct Breakdown {
    rows: Vec<Row>,
    calls: usize,
    cost_week_usd: f64,
    cost_24h_usd: f64,
    tokens: TokenBreakdown,
    missing_token_usage_calls: usize,
    excluded_calls: usize,
    excluded_cost_usd: f64,
}

fn is_uuid(id: &str) -> bool {
    id.len() == 36 && uuid::Uuid::parse_str(id).is_ok()
}

fn summarize(events: &[CursorUsageRecord], now: DateTime<Utc>, start: DateTime<Utc>) -> Breakdown {
    let mut groups: BTreeMap<String, (Row, BTreeSet<String>)> = BTreeMap::new();
    let mut result = Breakdown {
        rows: vec![],
        calls: 0,
        cost_week_usd: 0.0,
        cost_24h_usd: 0.0,
        tokens: TokenBreakdown::default(),
        missing_token_usage_calls: 0,
        excluded_calls: 0,
        excluded_cost_usd: 0.0,
    };
    for record in events {
        let event = &record.message;
        if event.timestamp < start.timestamp_millis() || event.timestamp > now.timestamp_millis() {
            continue;
        }
        let cost = if event.cost.is_finite() && event.cost >= 0.0 {
            event.cost
        } else {
            0.0
        };
        let group = if event.client != "grok-bot" {
            None
        } else if record.automation_id.is_some() || event.model_id == "grok-bot-automation" {
            Some("schedules")
        } else if event.session_id.starts_with("sand-subagent-") {
            Some("subtasks")
        } else {
            Some(event.session_id.as_str())
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
        if record.has_token_usage {
            row.tokens += &event.tokens;
            result.tokens += &event.tokens;
        } else {
            row.missing_token_usage_calls += 1;
            result.missing_token_usage_calls += 1;
        }
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
    events: Option<&[CursorUsageRecord]>,
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
    mut caches: BTreeMap<String, Vec<CursorUsageRecord>>,
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

fn apply_aliases(reports: &mut [AccountReport], aliases: &BTreeMap<String, String>) {
    for report in reports {
        let Some(data) = &mut report.breakdown else {
            continue;
        };
        for row in &mut data.rows {
            if !is_uuid(&row.id) {
                continue;
            }
            row.name = aliases.get(&row.id).and_then(|name| {
                let clean: String = name.chars().filter(|c| !c.is_control()).collect();
                let clean = clean.trim();
                (!clean.is_empty()).then(|| clean.to_string())
            });
        }
    }
}

fn load_aliases(path: &std::path::Path) -> Result<BTreeMap<String, String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(serde_json::from_str(&content)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error.into()),
    }
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
                            tokscale_core::sessions::cursor::parse_cursor_usage_records(
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
    let mut reports = account_reports(cards, caches, email.as_deref(), now, since);
    let alias_path = crate::paths::get_config_dir().join("grok-bot-aliases.json");
    match load_aliases(&alias_path) {
        Ok(aliases) => apply_aliases(&mut reports, &aliases),
        Err(error) => diagnostics.push(format!("Cannot read bot aliases: {error}")),
    }
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
        "  {:<32} {:>8} {:>12} {:>8} {:>11} {:>11} {:>11} {:>11}",
        "Bot", "Calls", "Cost USD", "Share", "Input", "Output", "Cache read", "Cache write"
    );
    for row in &data.rows {
        let label = match row.id.as_str() {
            "subtasks" => format!("Subtasks ({} distinct)", row.distinct_conversations),
            "schedules" => format!(
                "Schedules/automation ({} distinct)",
                row.distinct_conversations
            ),
            _ => {
                let short_id: String = row.id.chars().take(8).collect();
                row.name
                    .as_ref()
                    .map(|name| {
                        let short_name: String = name.chars().take(20).collect();
                        format!("{short_name} ({short_id})")
                    })
                    .unwrap_or(short_id)
            }
        };
        println!(
            "  {label:<32} {:>8} {:>12.2} {:>7.1}% {:>11} {:>11} {:>11} {:>11}",
            row.calls,
            row.cost_usd,
            row.share_percent,
            row.tokens.input,
            row.tokens.output,
            row.tokens.cache_read,
            row.tokens.cache_write
        );
        if row.missing_token_usage_calls > 0 {
            println!(
                "    Missing token data: {} calls",
                row.missing_token_usage_calls
            );
        }
    }
    println!(
        "  {:<32} {:>8} {:>12.2} {:>8} {:>11} {:>11} {:>11} {:>11}",
        "Total",
        data.calls,
        data.cost_week_usd,
        "",
        data.tokens.input,
        data.tokens.output,
        data.tokens.cache_read,
        data.tokens.cache_write
    );
    println!(
        "  Missing token data: {} calls (token columns sum known data only)",
        data.missing_token_usage_calls
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
    use tokscale_core::sessions::cursor::parse_cursor_usage_records;

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
        let events = parse_cursor_usage_records(
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
    fn known_tokens_missing_data_and_automation_ids() {
        let records = parse_cursor_usage_records(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../tokscale-core/tests/fixtures/grok_bot_usage.json"
            )),
            "synthetic",
        );
        let now = time("2026-10-08T06:00:00Z");
        let data = summarize(&records, now, now - Duration::days(7));
        assert_eq!(data.calls, 6);
        assert_eq!(data.tokens.input, 720);
        assert_eq!(data.tokens.output, 72);
        assert_eq!(data.tokens.cache_read, 55);
        assert_eq!(data.tokens.cache_write, 17);
        assert_eq!(data.missing_token_usage_calls, 1);
        assert_eq!(data.cost_week_usd, 5.5);
        assert_eq!(data.excluded_calls, 2);
        assert_eq!(data.excluded_cost_usd, 1.25);
        let schedules = data.rows.iter().find(|r| r.id == "schedules").unwrap();
        assert_eq!(schedules.calls, 2); // automationId plus model-only fallback.
        assert_eq!(schedules.tokens.input, 240);
        assert_eq!(schedules.tokens.cache_write, 12);
        let bot = &data.rows[0];
        assert_eq!(bot.calls, 3);
        assert_eq!(bot.missing_token_usage_calls, 1);
        assert_eq!(bot.tokens.input, 400);
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["missing_token_usage_calls"], 1);
        assert_eq!(json["tokens"]["input"], 720);
    }

    #[test]
    fn aliases_preserve_ids_totals_and_missing_file_fallback() {
        let now = time("2026-10-08T06:00:00Z");
        let events = parse_cursor_usage_records(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../tokscale-core/tests/fixtures/grok_bot_usage.json"
            )),
            "synthetic",
        );
        let mut reports = account_reports(
            vec![],
            BTreeMap::from([("active".into(), events)]),
            None,
            now,
            None,
        );
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        apply_aliases(
            &mut reports,
            &BTreeMap::from([
                (id.into(), "Synthetic bot".into()),
                ("schedules".into(), "Do not rename categories".into()),
            ]),
        );
        let data = reports[0].breakdown.as_ref().unwrap();
        assert_eq!(data.rows[0].id, id);
        assert_eq!(data.rows[0].name.as_deref(), Some("Synthetic bot"));
        assert_eq!(data.tokens.input, 720);
        assert_eq!(data.cost_week_usd, 5.5);
        assert!(data
            .rows
            .iter()
            .filter(|row| row.id != id)
            .all(|row| row.name.is_none()));
        let json = serde_json::to_value(&reports).unwrap();
        assert_eq!(json[0]["breakdown"]["rows"][0]["id"], id);
        assert_eq!(json[0]["breakdown"]["rows"][0]["name"], "Synthetic bot");
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("aliases.json");
        assert!(load_aliases(&path).unwrap().is_empty());
        std::fs::write(&path, serde_json::json!({id: "Synthetic bot"}).to_string()).unwrap();
        assert_eq!(load_aliases(&path).unwrap()[id], "Synthetic bot");
        std::fs::write(&path, "invalid json").unwrap();
        assert!(load_aliases(&path).is_err());
    }

    #[test]
    fn empty_automation_ids_and_zero_tokens_remain_subtasks() {
        let events = parse_cursor_usage_records(&serde_json::json!({"usageEventsDisplay": [
            {"timestamp": "1791417600000", "model": "gpt-5", "conversationId": "sand-subagent-11111111-2222-3333-4444-555555555555", "automationId": " ", "tokenUsage": {}},
            {"timestamp": "1791417600000", "model": "grok-4", "conversationId": "bbbbbbbb-cccc-dddd-eeee-ffffffffffff", "automationId": "ordinary-cursor"}
        ]}).to_string(), "work");
        let now = time("2026-10-08T06:00:00Z");
        let data = summarize(&events, now, now - Duration::days(7));
        assert_eq!(data.calls, 1);
        assert_eq!(data.excluded_calls, 1);
        assert_eq!(data.rows[0].id, "subtasks");
        assert_eq!(data.tokens, TokenBreakdown::default());
        assert_eq!(data.missing_token_usage_calls, 0);
    }

    #[test]
    fn anchored_conversation_schedules_use_metadata_or_router_fallback() {
        let now = time("2026-10-08T06:00:00Z");
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let rows = serde_json::json!({"usageEventsDisplay": [
            {"timestamp": (now - Duration::days(9)).timestamp_millis(), "model": "grok-bot-default", "conversationId": id},
            {"timestamp": now.timestamp_millis(), "model": "claude-synthetic", "conversationId": id, "automationId": "job", "tokenUsage": {"inputTokens": 5}},
            {"timestamp": now.timestamp_millis(), "model": "grok-bot-automation", "conversationId": id, "automationId": null},
            {"timestamp": now.timestamp_millis(), "model": "gpt-5", "conversationId": id}
        ]});
        let events = parse_cursor_usage_records(&rows.to_string(), "work");
        let data = summarize(&events, now, now - Duration::days(7));
        assert_eq!(data.calls, 3);
        assert_eq!(data.missing_token_usage_calls, 2);
        assert_eq!(data.tokens.input, 5);
        let schedules = data.rows.iter().find(|row| row.id == "schedules").unwrap();
        assert_eq!(schedules.calls, 2);
        assert_eq!(schedules.distinct_conversations, 1);
        assert_eq!(schedules.missing_token_usage_calls, 1);
        assert_eq!(data.rows.iter().find(|row| row.id == id).unwrap().calls, 1);
    }

    #[test]
    fn alias_control_characters_are_removed_and_empty_names_ignored() {
        let now = time("2026-10-08T06:00:00Z");
        let events = parse_cursor_usage_records(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../tokscale-core/tests/fixtures/grok_bot_usage.json"
            )),
            "work",
        );
        let mut reports = vec![report(Some("work".into()), None, Some(&events), now, None)];
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        apply_aliases(
            &mut reports,
            &BTreeMap::from([(id.into(), "  Safe\n\r\u{1b} name  ".into())]),
        );
        assert_eq!(
            reports[0].breakdown.as_ref().unwrap().rows[0]
                .name
                .as_deref(),
            Some("Safe name")
        );
        apply_aliases(&mut reports, &BTreeMap::from([(id.into(), " \t\n".into())]));
        assert!(reports[0].breakdown.as_ref().unwrap().rows[0]
            .name
            .is_none());
    }

    #[test]
    fn burn_rate_before_after_reset_and_unavailable() {
        let now = time("2026-10-08T00:00:00Z");
        let data = Breakdown {
            rows: vec![],
            calls: 1,
            cost_week_usd: 100.0,
            cost_24h_usd: 24.0,
            tokens: TokenBreakdown::default(),
            missing_token_usage_calls: 0,
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
