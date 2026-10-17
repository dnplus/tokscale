use super::{UsageMetric, UsageOutput};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::path::Path;

pub fn has_credentials() -> bool {
    has_cache_file_at(&crate::warp::get_warp_cache_dir().join("usage.json"))
}

fn has_cache_file_at(path: &Path) -> bool {
    path.is_file()
}

fn build_metrics(usage: &crate::warp::WarpAggregateUsage) -> Vec<UsageMetric> {
    let mut metrics = Vec::new();

    if let Some(used) = usage.requests_used {
        let (used_percent, remaining_percent, remaining_label) =
            if let Some(limit) = usage.request_limit.filter(|limit| *limit > 0) {
                let used_percent = (used as f64 / limit as f64 * 100.0).clamp(0.0, 100.0);
                let remaining = limit.saturating_sub(used);
                (
                    used_percent,
                    100.0 - used_percent,
                    Some(format!("{remaining} requests left")),
                )
            } else {
                // No request limit: this is an informational counter, not a
                // capped quota. Render the bar as full (100% remaining) rather
                // than an empty "exhausted" bar.
                (0.0, 100.0, Some(format!("{used} requests used")))
            };
        metrics.push(UsageMetric {
            label: "Requests".to_string(),
            used_percent,
            remaining_percent,
            remaining_label,
            resets_at: usage.next_refresh_time.clone(),
        });
    }

    if let Some(spend_cents) = usage.spend_cents {
        metrics.push(UsageMetric {
            label: "Spend".to_string(),
            used_percent: 0.0,
            // Spend is an informational dollar figure, not a consumed quota, so
            // keep the bar full instead of rendering a false "exhausted" bar.
            remaining_percent: 100.0,
            remaining_label: Some(format!("${:.2}", spend_cents as f64 / 100.0)),
            resets_at: usage.next_refresh_time.clone(),
        });
    }

    metrics
}

pub fn fetch() -> Result<UsageOutput> {
    read_cache_at(
        &crate::warp::get_warp_cache_dir().join("usage.json"),
        Utc::now(),
    )
}

fn read_cache_at(path: &Path, now: DateTime<Utc>) -> Result<UsageOutput> {
    let content = std::fs::read_to_string(path).context("Warp aggregate usage cache not found")?;
    let cache = serde_json::from_str(&content).context("Invalid Warp aggregate usage cache")?;
    build_output(&cache, now)
}

fn build_output(cache: &crate::warp::WarpUsageCache, now: DateTime<Utc>) -> Result<UsageOutput> {
    anyhow::ensure!(
        cache.version == 1,
        "Unsupported Warp aggregate usage cache version {}",
        cache.version
    );
    let synced_at = DateTime::parse_from_rfc3339(&cache.synced_at)
        .context("Invalid Warp aggregate usage cache source timestamp")?;
    let age = now.signed_duration_since(synced_at);
    anyhow::ensure!(
        age >= chrono::Duration::zero(),
        "Warp aggregate usage cache source timestamp is in the future"
    );
    anyhow::ensure!(
        age <= chrono::Duration::seconds(super::USAGE_CACHE_MAX_AGE_SECONDS as i64),
        "Warp aggregate usage cache is stale; sync Warp usage to refresh it"
    );
    let metrics = build_metrics(&cache.usage);

    Ok(UsageOutput {
        provider: "Warp/Oz".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::warp::WarpAggregateUsage;

    fn now() -> DateTime<Utc> {
        "2026-10-17T12:00:00Z".parse().unwrap()
    }

    fn fixture(synced_at: &str) -> crate::warp::WarpUsageCache {
        crate::warp::WarpUsageCache {
            version: 1,
            synced_at: synced_at.to_string(),
            usage: WarpAggregateUsage {
                requests_used: Some(0),
                request_limit: Some(100),
                spend_cents: Some(0),
                ..Default::default()
            },
            workspaces: Vec::new(),
        }
    }

    #[test]
    fn freshness_accepts_current_and_boundary_known_zero_usage() {
        for timestamp in ["2026-10-17T12:00:00Z", "2026-10-17T11:55:00Z"] {
            let output = build_output(&fixture(timestamp), now()).unwrap();
            assert_eq!(output.metrics.len(), 2);
            assert_eq!(output.metrics[0].used_percent, 0.0);
            assert_eq!(output.metrics[0].remaining_percent, 100.0);
            assert_eq!(output.metrics[1].remaining_label.as_deref(), Some("$0.00"));
        }
    }

    #[test]
    fn freshness_rejects_stale_future_and_malformed_source_times() {
        for (timestamp, reason) in [
            ("2026-10-17T11:54:59Z", "stale"),
            ("2026-10-17T11:54:59.999Z", "stale"),
            ("2026-10-17T12:00:01Z", "future"),
            ("2026-10-17T12:00:00.001Z", "future"),
            ("", "timestamp"),
            ("not-a-time", "timestamp"),
        ] {
            let error = build_output(&fixture(timestamp), now())
                .unwrap_err()
                .to_string();
            assert!(error.contains(reason), "{error}");
        }
    }

    #[test]
    fn freshness_rejects_unsupported_producer_version() {
        let mut cache = fixture("2026-10-17T12:00:00Z");
        cache.version = 2;
        let error = build_output(&cache, now()).unwrap_err().to_string();
        assert!(error.contains("version"), "{error}");
    }

    #[test]
    fn freshness_admission_keeps_malformed_cache_visible_for_diagnostics() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("usage.json");
        assert!(!has_cache_file_at(&path));
        std::fs::write(&path, "malformed").unwrap();
        assert!(has_cache_file_at(&path));
        let report = super::super::fetch_provider_report(
            "Warp/Oz",
            read_cache_at(&path, now()).map(|output| vec![output]),
        );
        assert!(report.outputs.is_empty());
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].provider, "Warp/Oz");
        assert!(report.diagnostics[0].message.contains("Invalid"));
    }

    #[test]
    fn freshness_reads_only_explicit_fixture_path() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("usage.json");
        std::fs::write(
            &path,
            serde_json::to_string(&fixture("2026-10-17T11:54:59Z")).unwrap(),
        )
        .unwrap();
        let error = read_cache_at(&path, now()).unwrap_err().to_string();
        assert!(error.contains("stale"), "{error}");
        assert!(path.exists());
        std::fs::write(&path, r#"{"version":1,"usage":{},"workspaces":[]}"#).unwrap();
        assert!(read_cache_at(&path, now())
            .unwrap_err()
            .to_string()
            .contains("Invalid"));
    }

    #[test]
    fn spend_metric_reads_full_not_exhausted() {
        let usage = WarpAggregateUsage {
            spend_cents: Some(1234),
            ..Default::default()
        };
        let metrics = build_metrics(&usage);
        let spend = metrics.iter().find(|m| m.label == "Spend").unwrap();
        // Informational $ figure: bar must read full, not a false 0%-remaining.
        assert_eq!(spend.remaining_percent, 100.0);
        assert_eq!(spend.used_percent, 0.0);
        assert_eq!(spend.remaining_label.as_deref(), Some("$12.34"));
    }

    #[test]
    fn unlimited_requests_read_full_not_exhausted() {
        let usage = WarpAggregateUsage {
            requests_used: Some(42),
            request_limit: None,
            ..Default::default()
        };
        let metrics = build_metrics(&usage);
        let requests = metrics.iter().find(|m| m.label == "Requests").unwrap();
        assert_eq!(requests.remaining_percent, 100.0);
        assert_eq!(requests.used_percent, 0.0);
        assert_eq!(
            requests.remaining_label.as_deref(),
            Some("42 requests used")
        );
    }

    #[test]
    fn limited_requests_compute_usage() {
        let usage = WarpAggregateUsage {
            requests_used: Some(25),
            request_limit: Some(100),
            ..Default::default()
        };
        let metrics = build_metrics(&usage);
        let requests = metrics.iter().find(|m| m.label == "Requests").unwrap();
        assert_eq!(requests.used_percent, 25.0);
        assert_eq!(requests.remaining_percent, 75.0);
        assert_eq!(
            requests.remaining_label.as_deref(),
            Some("75 requests left")
        );
    }
}
