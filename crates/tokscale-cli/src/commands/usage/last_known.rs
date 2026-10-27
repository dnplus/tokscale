//! The last usage each provider reported, so a failed refresh shows that
//! reading, marked cached with its observation time, instead of dropping the
//! card. Stores observations only, never credentials.
use super::{UsageFetchDiagnosticSeverity, UsageFetchReport, UsageFreshness, UsageOutput};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub(super) const FILE: &str = "usage-last-known.json";
const MAX_AGE_SECS: u64 = 7 * 24 * 3600;

#[derive(Default, Deserialize, Serialize)]
struct Store {
    version: u8,
    /// Each output carries its observation time in `freshness.fetched_at`.
    entries: Vec<UsageOutput>,
}

/// Claude keeps token-scoped state of its own, and Warp reads its own synced
/// cache: both already report their freshness.
fn self_managed(output: &UsageOutput) -> bool {
    output.provider == "Claude" || output.provider == "Warp/Oz" || output.freshness.is_some()
}

fn same_card(a: &UsageOutput, b: &UsageOutput) -> bool {
    a.provider == b.provider
        && a.account.as_ref().map(|account| &account.id)
            == b.account.as_ref().map(|account| &account.id)
}

fn load(path: &Path) -> Store {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Store>(&bytes).ok())
        .filter(|store| store.version == 1)
        .unwrap_or_default()
}

fn save(path: &Path, store: &Store) {
    let Ok(json) = serde_json::to_vec(store) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = super::helpers::atomic_write_secret(path, &json);
}

/// The stored reading as it can still be shown at `now`: windows past their
/// reset no longer describe the account and are dropped, and a card left with
/// none is not shown.
fn last_known(entry: &UsageOutput, now: u64) -> Option<UsageOutput> {
    let fetched_at = entry.freshness.as_ref()?.fetched_at;
    if fetched_at > now || now - fetched_at > MAX_AGE_SECS {
        return None;
    }
    let mut output = entry.clone();
    output.metrics.retain(|metric| {
        metric.resets_at.as_deref().is_none_or(|reset| {
            chrono::DateTime::parse_from_rfc3339(reset)
                .is_ok_and(|reset| reset.timestamp() > now as i64)
        })
    });
    if output.metrics.is_empty() {
        return None;
    }
    output.freshness = Some(UsageFreshness {
        fetched_at,
        stale: true,
    });
    Some(output)
}

/// Records this report's live cards and, for each provider or account that
/// failed with an error, restores its last known card in place of the missing
/// one. A provider-wide failure restores only when the provider returned no
/// card at all, so a partial success never resurrects accounts it no longer
/// reports.
pub(super) fn apply(path: Option<&Path>, report: &mut UsageFetchReport, now: u64) {
    let Some(path) = path else {
        return;
    };
    let mut store = load(path);
    store.entries.retain(|entry| {
        entry
            .freshness
            .as_ref()
            .is_some_and(|f| f.fetched_at <= now && now - f.fetched_at <= MAX_AGE_SECS)
    });
    for output in report.outputs.iter().filter(|output| !self_managed(output)) {
        let mut entry = output.clone();
        entry.freshness = Some(UsageFreshness {
            fetched_at: now,
            stale: false,
        });
        store.entries.retain(|stored| !same_card(stored, &entry));
        store.entries.push(entry);
    }
    store.version = 1;
    save(path, &store);

    let mut restored = Vec::new();
    for diagnostic in &report.diagnostics {
        if diagnostic.severity != UsageFetchDiagnosticSeverity::Error {
            continue;
        }
        let provider_has_cards = report
            .outputs
            .iter()
            .any(|output| output.provider == diagnostic.provider);
        for entry in &store.entries {
            if entry.provider != diagnostic.provider {
                continue;
            }
            let matches = match &diagnostic.account {
                Some(account) => entry
                    .account
                    .as_ref()
                    .is_some_and(|stored| stored.id == account.id),
                None => !provider_has_cards,
            };
            if !matches
                || report.outputs.iter().any(|output| same_card(output, entry))
                || restored.iter().any(|output| same_card(output, entry))
            {
                continue;
            }
            restored.extend(last_known(entry, now));
        }
    }
    report.outputs.extend(restored);
}

#[cfg(test)]
mod tests {
    use super::super::{UsageAccount, UsageFetchDiagnostic, UsageMetric};
    use super::*;

    const T0: u64 = 1_785_751_200; // 2026-08-03T10:00:00Z
    const DAY: u64 = 86_400;

    fn card(provider: &str, account: Option<&str>, resets_at: &str) -> UsageOutput {
        UsageOutput {
            provider: provider.into(),
            account: account.map(|id| UsageAccount {
                id: id.into(),
                label: None,
                is_active: false,
            }),
            credential_source: None,
            plan: Some("Pro".into()),
            email: None,
            metrics: vec![UsageMetric {
                label: "Weekly".into(),
                used_percent: 40.0,
                remaining_percent: 60.0,
                remaining_label: None,
                resets_at: Some(resets_at.into()),
            }],
            reset_credits: None,
            credit_status: None,
            spend_control: None,
            freshness: None,
        }
    }

    fn failure(provider: &str, account: Option<&str>) -> UsageFetchDiagnostic {
        UsageFetchDiagnostic::new(
            provider,
            account.map(|id| UsageAccount {
                id: id.into(),
                label: None,
                is_active: false,
            }),
            "HTTP 401",
        )
    }

    fn report(
        outputs: Vec<UsageOutput>,
        diagnostics: Vec<UsageFetchDiagnostic>,
    ) -> UsageFetchReport {
        UsageFetchReport {
            outputs,
            diagnostics,
        }
    }

    const LATER: &str = "2026-09-30T00:00:00Z";

    #[test]
    fn failed_refresh_shows_the_last_known_card_only_where_it_still_applies() {
        // (later outputs, later diagnostics, seconds later, cards expected)
        type Case = (Vec<UsageOutput>, Vec<UsageFetchDiagnostic>, u64, usize);
        let cases: Vec<(&str, Case)> = vec![
            (
                "provider failed",
                (vec![], vec![failure("Z.ai", None)], DAY, 1),
            ),
            (
                "older than a week",
                (vec![], vec![failure("Z.ai", None)], 7 * DAY + 1, 0),
            ),
            ("no failure reported", (vec![], vec![], DAY, 0)),
            (
                "another provider failed",
                (vec![], vec![failure("Amp", None)], DAY, 0),
            ),
            (
                "a warning is not a failure",
                (
                    vec![],
                    vec![UsageFetchDiagnostic::with_kind(
                        "Z.ai",
                        None,
                        Default::default(),
                        UsageFetchDiagnosticSeverity::Warning,
                        "quota unknown",
                    )],
                    DAY,
                    0,
                ),
            ),
            (
                "live card already present",
                (
                    vec![card("Z.ai", None, LATER)],
                    vec![failure("Z.ai", None)],
                    DAY,
                    1,
                ),
            ),
        ];
        for (name, (outputs, diagnostics, after, expected)) in cases {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join(FILE);
            apply(
                Some(&path),
                &mut report(vec![card("Z.ai", None, LATER)], vec![]),
                T0,
            );
            let mut later = report(outputs, diagnostics);
            apply(Some(&path), &mut later, T0 + after);
            assert_eq!(later.outputs.len(), expected, "{name}");
            if name == "provider failed" {
                assert_eq!(
                    later.outputs[0].freshness,
                    Some(UsageFreshness {
                        fetched_at: T0,
                        stale: true
                    })
                );
            }
            if name == "live card already present" {
                assert_eq!(later.outputs[0].freshness, None, "{name}");
            }
        }
    }

    #[test]
    fn account_failures_restore_only_that_account() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(FILE);
        apply(
            Some(&path),
            &mut report(
                vec![
                    card("Codex", Some("work"), LATER),
                    card("Codex", Some("home"), LATER),
                ],
                vec![],
            ),
            T0,
        );
        // One account fails, the other still reports.
        let mut later = report(
            vec![card("Codex", Some("home"), LATER)],
            vec![failure("Codex", Some("work"))],
        );
        apply(Some(&path), &mut later, T0 + 60);
        assert_eq!(later.outputs.len(), 2);
        let restored = later
            .outputs
            .iter()
            .find(|o| o.freshness.is_some())
            .unwrap();
        assert_eq!(restored.account.as_ref().unwrap().id, "work");
        // A provider-wide failure next to a live card must not resurrect the
        // account that is no longer reported (for example, one removed).
        let mut later = report(
            vec![card("Codex", Some("home"), LATER)],
            vec![failure("Codex", None)],
        );
        apply(Some(&path), &mut later, T0 + 60);
        assert_eq!(later.outputs.len(), 1);
        // An unknown account never borrows another account's reading.
        let mut later = report(vec![], vec![failure("Codex", Some("other"))]);
        apply(Some(&path), &mut later, T0 + 60);
        assert!(later.outputs.is_empty());
    }

    #[test]
    fn windows_past_their_reset_are_not_shown_as_known() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(FILE);
        let mut first = card("Z.ai", None, "2026-08-03T12:00:00Z");
        first.metrics.push(UsageMetric {
            label: "Balance".into(),
            used_percent: 0.0,
            remaining_percent: 100.0,
            remaining_label: Some("$5 left".into()),
            resets_at: None,
        });
        apply(Some(&path), &mut report(vec![first], vec![]), T0);
        let mut later = report(vec![], vec![failure("Z.ai", None)]);
        apply(Some(&path), &mut later, T0 + 3 * 3600);
        assert_eq!(later.outputs.len(), 1);
        let labels: Vec<_> = later.outputs[0].metrics.iter().map(|m| &m.label).collect();
        assert_eq!(labels, ["Balance"]);
    }

    #[test]
    fn self_managed_providers_are_neither_recorded_nor_restored() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(FILE);
        let mut claude = card("Claude", None, LATER);
        claude.freshness = Some(UsageFreshness {
            fetched_at: T0,
            stale: false,
        });
        apply(
            Some(&path),
            &mut report(vec![claude, card("Warp/Oz", None, LATER)], vec![]),
            T0,
        );
        let mut later = report(
            vec![],
            vec![failure("Claude", None), failure("Warp/Oz", None)],
        );
        apply(Some(&path), &mut later, T0 + 60);
        assert!(later.outputs.is_empty());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("Claude") && !raw.contains("Warp"), "{raw}");
    }
}
