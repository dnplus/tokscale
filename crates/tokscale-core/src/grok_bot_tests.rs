use super::*;

const FIXTURE: &str = include_str!("../tests/fixtures/grok_bot_usage.json");

fn events() -> Vec<UnifiedMessage> {
    sessions::cursor::parse_cursor_events_json_content(FIXTURE, "synthetic")
}

#[test]
fn cursor_split_preserves_every_token_bucket_cost_and_message() {
    let split = events();
    assert_eq!(split.len(), 8);
    assert_eq!(split.iter().filter(|m| m.client == "grok-bot").count(), 6);
    assert_eq!(split.iter().filter(|m| m.client == "cursor").count(), 2);
    assert_eq!(split[1].client, "grok-bot"); // Non-Grok model in a bot UUID.
    assert_eq!(split[5].client, "cursor"); // bc- automation is excluded.
    let mut before = split.clone();
    for message in &mut before {
        message.client = "cursor".into();
    }
    let baseline = aggregator::aggregate_by_date(before);
    let after = aggregator::aggregate_by_date(split);
    assert_eq!(baseline[0].totals, after[0].totals);
    assert_eq!(baseline[0].token_breakdown, after[0].token_breakdown);
    assert_eq!(after[0].token_breakdown.input, 920);
    assert_eq!(after[0].token_breakdown.output, 92);
    assert_eq!(after[0].token_breakdown.cache_read, 57);
    assert_eq!(after[0].token_breakdown.cache_write, 18);
    assert_eq!(after[0].totals.cost, 6.75);
    assert_eq!(after[0].totals.messages, 8);
}

#[test]
fn submission_maps_and_merges_cursor_and_bot_once() {
    let mut split = events();
    // Force the same client/model/provider key on both sides of the split.
    split[5].model_id = split[1].model_id.clone();
    split[5].provider_id = split[1].provider_id.clone();
    let mut before = split.clone();
    for message in &mut before {
        message.client = "cursor".into();
    }
    let baseline = aggregator::aggregate_by_date(before);
    let mut local = GraphSink::new(None, None, GraphPricingRequirement::Lenient);
    let pricing = pricing::PricingService::new(HashMap::new(), HashMap::new());
    let mut submission = GraphSink::new(None, Some(&pricing), GraphPricingRequirement::Submission);
    for message in split {
        local.accept(message.clone());
        submission.accept(message);
    }
    local.drain_batch();
    submission.drain_batch();
    assert!(submission.failure.is_none(), "{:?}", submission.failure);
    let local = local.daily.finish();
    assert!(local[0].clients.iter().any(|c| c.client == "grok-bot"));
    let uploaded = submission.daily.finish();
    assert_eq!(uploaded[0].totals, baseline[0].totals);
    assert_eq!(uploaded[0].token_breakdown, baseline[0].token_breakdown);
    assert_eq!(uploaded[0].clients.len(), baseline[0].clients.len());
    assert!(uploaded[0].clients.iter().all(|c| c.client == "cursor"));
    for expected in &baseline[0].clients {
        let actual = uploaded[0]
            .clients
            .iter()
            .find(|c| c.model_id == expected.model_id)
            .unwrap();
        assert_eq!(actual, expected);
    }
    let payload = serde_json::to_value(&uploaded).unwrap();
    assert!(payload[0]["clients"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["client"] == "cursor"));
}

#[test]
fn shared_cursor_scan_is_single_and_filters_are_exclusive() {
    let home = tempfile::tempdir().unwrap();
    let cache = home.path().join(".config/tokscale/cursor-cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("usage.json"), FIXTURE).unwrap();
    // Old CSV spelling must not become a duplicate source.
    std::fs::write(cache.join("usage.csv"), "Date,Model,Cost\n").unwrap();
    for (clients, expected) in [
        (vec!["cursor".to_string()], 2),
        (vec!["grok-bot".to_string()], 6),
        (vec!["cursor".to_string(), "grok-bot".to_string()], 8),
    ] {
        let scan = scanner::scan_all_clients_with_scanner_settings(
            home.path().to_str().unwrap(),
            &clients,
            false,
            &Default::default(),
        );
        assert_eq!(scan.get(ClientId::Cursor).len(), 1);
        assert!(scan.get(ClientId::GrokBot).is_empty());
        let parsed = parse_all_messages_with_pricing_with_cache_policy(
            home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &Default::default(),
            SourceCachePolicy::InMemory,
        );
        assert_eq!(parsed.len(), expected);
        assert!(parsed.iter().all(|m| clients.contains(&m.client)));
    }
}

#[test]
fn cursor_records_distinguish_missing_and_partial_token_usage() {
    let records = sessions::cursor::parse_cursor_usage_records(
        r#"{"usageEventsDisplay":[
          {"timestamp":"1791417600000","model":"grok-bot-default","conversationId":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","tokenUsage":{},"automationId":42},
          {"timestamp":"1791417600000","model":"default","conversationId":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"}
        ]}"#,
        "synthetic",
    );
    assert!(records[0].has_token_usage);
    assert!(records[0].automation_id.is_none());
    assert!(!records[1].has_token_usage);
    assert_eq!(records[0].message.tokens, TokenBreakdown::default());
    assert_eq!(records[1].message.client, "grok-bot");
}
