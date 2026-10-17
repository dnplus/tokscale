//! Fork-only: upload each submission to extra servers ("targets") as well as
//! the logged-in one, without switching profiles.
//!
//! Every target keeps its own URL and token in `targets.json`, next to
//! `credentials.json`, and a token is only ever sent to the origin it was saved
//! with. Targets are independent of each other and of the primary server: a
//! failure is reported and recorded in `targets-state.json`, never returned.
//! The rest of the CLI touches this module only through [`ExtraTargetFlags`],
//! [`Plan`] and [`TargetSubcommand`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::TsTokenContributionData;

const TARGETS_FILE: &str = "targets.json";
const STATE_FILE: &str = "targets-state.json";
const MAX_NAME_LEN: usize = 32;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Whole-request bound for a submit. Payloads covering a full history are
/// large, so this is longer than the 30s used for token checks.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(120);
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Subcommand)]
pub enum TargetSubcommand {
    #[command(about = "Save (or replace) an extra server that every submit also uploads to")]
    Add {
        #[arg(help = "Short name for the target, e.g. pa")]
        name: String,
        #[arg(long, value_name = "URL", help = "Server URL of the target")]
        api_url: String,
        #[arg(long, value_name = "TOKEN", help = "API token issued by that server")]
        token: String,
    },
    #[command(about = "List extra targets and their last upload result")]
    List {
        #[arg(long, help = "Output as JSON (tokens are never printed)")]
        json: bool,
    },
    #[command(about = "Delete a saved extra target")]
    Remove { name: String },
    #[command(about = "Include a target in every submit and scheduled run")]
    Enable { name: String },
    #[command(about = "Keep a target saved but skip it unless named with --extra-target")]
    Disable { name: String },
}

/// Per-run selection, flattened into `submit` and `autosubmit run`.
#[derive(Args, Debug, Clone, Default)]
pub struct ExtraTargetFlags {
    #[arg(
        long = "extra-target",
        value_name = "NAME",
        help = "Also upload to this saved target, instead of every enabled one (repeatable)"
    )]
    pub names: Vec<String>,
    #[arg(
        long,
        conflicts_with = "names",
        help = "Upload only to the logged-in server this run"
    )]
    pub no_extra_targets: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub name: String,
    pub api_url: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub created_at: String,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TargetsFile {
    #[serde(default)]
    targets: Vec<Target>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetState {
    #[serde(default)]
    pub last_run_at_ms: Option<i64>,
    #[serde(default)]
    pub last_success_at_ms: Option<i64>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_quota_error: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    targets: BTreeMap<String, TargetState>,
}

/// What `target list --json` and `autosubmit status --json` report. Never
/// carries the token.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetStatus {
    pub name: String,
    pub api_url: String,
    pub username: Option<String>,
    pub enabled: bool,
    #[serde(flatten)]
    pub state: TargetState,
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Same directory as `credentials.json`: scheduled jobs run without the
/// shell's environment, and only the home-based path is guaranteed to match.
fn config_dir() -> Result<PathBuf> {
    Ok(crate::paths::home_dir()
        .context("Could not determine home directory")?
        .join(".config")
        .join("tokscale"))
}

fn load_targets_from(dir: &Path) -> Result<Vec<Target>> {
    let path = dir.join(TARGETS_FILE);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("Could not read {}", path.display()))
        }
    };
    let file: TargetsFile = serde_json::from_str(&content)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    Ok(file.targets)
}

/// Create the config directory private to the user, as `auth` does for
/// `credentials.json`. An existing directory is left as it is.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("Could not create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Written only by the `target` subcommands, never by a submit, so a scheduled
/// run cannot race a token rewrite.
fn save_targets_to(dir: &Path, targets: &[Target]) -> Result<()> {
    ensure_private_dir(dir)?;
    let json = serde_json::to_string_pretty(&TargetsFile {
        targets: targets.to_vec(),
    })?;
    crate::commands::usage::helpers::atomic_write_secret(&dir.join(TARGETS_FILE), json.as_bytes())
        .context("Could not save extra targets")
}

fn load_state_from(dir: &Path) -> BTreeMap<String, TargetState> {
    std::fs::read_to_string(dir.join(STATE_FILE))
        .ok()
        .and_then(|content| serde_json::from_str::<StateFile>(&content).ok())
        .map(|file| file.targets)
        .unwrap_or_default()
}

fn save_state_to(dir: &Path, targets: BTreeMap<String, TargetState>) -> Result<()> {
    ensure_private_dir(dir)?;
    let json = serde_json::to_string_pretty(&StateFile { targets })?;
    crate::commands::usage::helpers::atomic_write_secret(&dir.join(STATE_FILE), json.as_bytes())?;
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !valid {
        bail!(
            "Invalid target name `{name}`: use up to {MAX_NAME_LEN} lowercase letters, digits, `-` or `_`."
        );
    }
    Ok(())
}

fn origin(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()
        .map(|url| url.origin().ascii_serialization())
}

fn redact(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_string()
    } else {
        text.replace(token, "[REDACTED]")
    }
}

/// Saved targets with their last results. An unreadable `targets.json` is an
/// error, so callers can say so instead of showing no targets.
pub fn statuses() -> Result<Vec<TargetStatus>> {
    statuses_in(&config_dir()?)
}

fn statuses_in(dir: &Path) -> Result<Vec<TargetStatus>> {
    let mut state = load_state_from(dir);
    Ok(load_targets_from(dir)?
        .into_iter()
        .map(|target| TargetStatus {
            state: state.remove(&target.name).unwrap_or_default(),
            name: target.name,
            api_url: target.api_url,
            username: target.username,
            enabled: target.enabled,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

fn select(
    targets: Vec<Target>,
    flags: &ExtraTargetFlags,
    primary_url: &str,
) -> Result<(Vec<Target>, Vec<String>)> {
    if flags.no_extra_targets {
        return Ok((Vec::new(), Vec::new()));
    }
    for name in &flags.names {
        if !targets.iter().any(|target| &target.name == name) {
            bail!(
                "Unknown extra target `{name}`. Run `tokscale target list` to see saved targets."
            );
        }
    }
    let primary_origin = origin(primary_url);
    let mut selected = Vec::new();
    let mut notices = Vec::new();
    for target in targets {
        let wanted = if flags.names.is_empty() {
            target.enabled
        } else {
            flags.names.contains(&target.name)
        };
        if !wanted {
            continue;
        }
        // `targets.json` can be edited by hand; never send a token to a URL
        // `target add` would have refused, such as plain http off localhost.
        let api_url = match crate::auth::normalize_api_url(&target.api_url) {
            Ok(api_url) => api_url,
            Err(error) => {
                notices.push(format!("Skipping extra target {}: {error:#}", target.name));
                continue;
            }
        };
        let target = Target { api_url, ..target };
        if origin(&target.api_url) == primary_origin {
            notices.push(format!(
                "Skipping extra target {}: it is the server this submit already uploads to.",
                target.name
            ));
            continue;
        }
        selected.push(target);
    }
    Ok((selected, notices))
}

/// The extra targets one submit uploads to.
pub struct Plan {
    targets: Vec<Target>,
}

impl Plan {
    /// Resolve before scanning, so an unknown `--extra-target` fails early.
    /// An unreadable `targets.json` only fails the run when targets were named
    /// explicitly; otherwise the logged-in server still gets the submission.
    pub fn resolve(flags: &ExtraTargetFlags, primary_url: &str) -> Result<Self> {
        let targets = if flags.no_extra_targets {
            Vec::new()
        } else {
            match config_dir().and_then(|dir| load_targets_from(&dir)) {
                Ok(targets) => targets,
                Err(error) if flags.names.is_empty() => {
                    eprintln!("  Warning: skipping extra targets: {error:#}");
                    Vec::new()
                }
                Err(error) => return Err(error),
            }
        };
        let (targets, notices) = select(targets, flags, primary_url)?;
        for notice in notices {
            eprintln!("  {notice}");
        }
        Ok(Self { targets })
    }

    pub fn print_dry_run(&self) {
        for target in &self.targets {
            println!(
                "  Would also submit to {} ({}).",
                target.name, target.api_url
            );
        }
    }

    /// Upload `payload` (and, unless `no_quota`, the quota reading) to every
    /// target. Each failure is printed and recorded; none is returned, so the
    /// primary upload proceeds regardless.
    pub fn submit_all(
        &self,
        rt: &tokio::runtime::Runtime,
        payload: &TsTokenContributionData,
        no_quota: bool,
    ) {
        if self.targets.is_empty() {
            return;
        }
        let results: Vec<(String, TargetState)> = self
            .targets
            .iter()
            .map(|target| {
                (
                    target.name.clone(),
                    submit_one(rt, target, payload, no_quota),
                )
            })
            .collect();
        if let Err(error) = config_dir().and_then(|dir| record_results(&dir, results)) {
            eprintln!("  Warning: could not record extra target results: {error}");
        }
    }
}

fn record_results(dir: &Path, results: Vec<(String, TargetState)>) -> Result<()> {
    let mut state = load_state_from(dir);
    for (name, result) in results {
        let entry = state.entry(name).or_default();
        entry.last_run_at_ms = result.last_run_at_ms;
        entry.last_error = result.last_error;
        entry.last_quota_error = result.last_quota_error;
        if result.last_success_at_ms.is_some() {
            entry.last_success_at_ms = result.last_success_at_ms;
        }
    }
    save_state_to(dir, state)
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    Ok(tokscale_core::http::client_builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        .build()?)
}

/// Read at most [`MAX_RESPONSE_BYTES`]; a misbehaving receiver must not make
/// the CLI buffer an unbounded body.
async fn read_bounded(mut response: reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = MAX_RESPONSE_BYTES.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= MAX_RESPONSE_BYTES {
            break;
        }
    }
    body
}

fn error_message(body: &[u8], fallback: &str, status: reqwest::StatusCode) -> String {
    let message = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|body| body.get("error")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| fallback.to_string());
    format!("{message} ({status})")
}

async fn post_submission(target: &Target, payload: &TsTokenContributionData) -> Result<()> {
    let payload = crate::submit_compat::prepare_submission(&target.api_url, payload).await?;
    let response = http_client(SUBMIT_TIMEOUT)?
        .post(format!("{}/api/submit", target.api_url))
        .header("Authorization", format!("Bearer {}", target.token))
        .json(&*payload)
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "Failed to connect to server: {}",
                tokscale_core::pricing::describe_error(&error)
            )
        })?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = read_bounded(response).await;
    bail!(error_message(&body, "Submission failed", status))
}

fn submit_one(
    rt: &tokio::runtime::Runtime,
    target: &Target,
    payload: &TsTokenContributionData,
    no_quota: bool,
) -> TargetState {
    println!(
        "  Submitting to extra target {} ({})...",
        target.name, target.api_url
    );
    let mut state = TargetState {
        last_run_at_ms: Some(chrono::Utc::now().timestamp_millis()),
        ..TargetState::default()
    };
    if let Err(error) = rt.block_on(post_submission(target, payload)) {
        let error = redact(&format!("{error:#}"), &target.token);
        eprintln!(
            "  Warning: extra target {} upload failed: {error}",
            target.name
        );
        state.last_error = Some(error);
        return state;
    }
    println!("  Submitted to extra target {}.", target.name);
    state.last_success_at_ms = state.last_run_at_ms;

    // Outside `block_on`: the quota upload drives its own runtime.
    let auth = crate::auth::ApiTokenAuth {
        token: target.token.clone(),
        username: target.username.clone(),
        source: crate::auth::ApiTokenSource::StoredCredentials,
        api_url: Some(target.api_url.clone()),
    };
    if let Some(result) = crate::commands::autosubmit::after_submit_quota(
        no_quota,
        &target.api_url,
        || crate::commands::autosubmit::upload_quota(&auth, &target.api_url),
        |warning| {
            eprintln!(
                "  {} (extra target {})",
                redact(warning, &target.token),
                target.name
            )
        },
    ) {
        match result {
            Ok(()) => println!("  Quota reading uploaded to extra target {}.", target.name),
            Err(error) => state.last_quota_error = Some(redact(&error.to_string(), &target.token)),
        }
    }
    state
}

#[derive(Debug, Deserialize)]
struct TokenValidationResponse {
    user: TokenUser,
}

#[derive(Debug, Deserialize)]
struct TokenUser {
    username: String,
}

async fn validate_token(api_url: &str, token: &str) -> Result<String> {
    let response = http_client(VALIDATE_TIMEOUT)?
        .get(format!("{api_url}/api/auth/token"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "Failed to connect to {api_url}: {}",
                tokscale_core::pricing::describe_error(&error)
            )
        })?;
    let status = response.status();
    let body = read_bounded(response).await;
    if !status.is_success() {
        bail!(error_message(&body, "API token validation failed", status));
    }
    let data: TokenValidationResponse = serde_json::from_slice(&body)
        .with_context(|| format!("{api_url}/api/auth/token returned an unexpected response"))?;
    Ok(data.user.username)
}

// ---------------------------------------------------------------------------
// `tokscale target`
// ---------------------------------------------------------------------------

pub fn run(subcommand: TargetSubcommand) -> Result<()> {
    let dir = config_dir()?;
    match subcommand {
        TargetSubcommand::Add {
            name,
            api_url,
            token,
        } => add(&dir, &name, &api_url, &token),
        TargetSubcommand::List { json } => list(&dir, json),
        TargetSubcommand::Remove { name } => {
            let mut targets = load_targets_from(&dir)?;
            let before = targets.len();
            targets.retain(|target| target.name != name);
            if targets.len() == before {
                bail!("No extra target named `{name}`.");
            }
            save_targets_to(&dir, &targets)?;
            let mut state = load_state_from(&dir);
            if state.remove(&name).is_some() {
                let _ = save_state_to(&dir, state);
            }
            println!("Removed extra target {name}.");
            Ok(())
        }
        TargetSubcommand::Enable { name } => set_enabled(&dir, &name, true),
        TargetSubcommand::Disable { name } => set_enabled(&dir, &name, false),
    }
}

fn add(dir: &Path, name: &str, api_url: &str, token: &str) -> Result<()> {
    validate_name(name)?;
    let api_url = crate::auth::normalize_api_url(api_url)?;
    let token = token.trim();
    if !token.starts_with("tt_") {
        bail!("Tokscale API tokens must start with `tt_`.");
    }
    // Read before the network call so a corrupt file fails fast.
    let mut targets = load_targets_from(dir)?;
    let rt = tokio::runtime::Runtime::new()?;
    let username = rt
        .block_on(validate_token(&api_url, token))
        .map_err(|error| anyhow::anyhow!(redact(&format!("{error:#}"), token)))?;
    let target = Target {
        name: name.to_string(),
        api_url: api_url.clone(),
        token: token.to_string(),
        username: Some(username.clone()),
        enabled: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let replaced = upsert(&mut targets, target);
    save_targets_to(dir, &targets)?;
    println!(
        "{} extra target {name}: {api_url} as {username}.",
        if replaced { "Updated" } else { "Saved" }
    );
    println!("  Every `tokscale submit` and scheduled run now also uploads there.");
    Ok(())
}

fn upsert(targets: &mut Vec<Target>, target: Target) -> bool {
    match targets
        .iter_mut()
        .find(|existing| existing.name == target.name)
    {
        Some(existing) => {
            *existing = target;
            true
        }
        None => {
            targets.push(target);
            false
        }
    }
}

fn set_enabled(dir: &Path, name: &str, enabled: bool) -> Result<()> {
    let mut targets = load_targets_from(dir)?;
    let target = targets
        .iter_mut()
        .find(|target| target.name == name)
        .ok_or_else(|| anyhow::anyhow!("No extra target named `{name}`."))?;
    target.enabled = enabled;
    save_targets_to(dir, &targets)?;
    println!(
        "Extra target {name} {}.",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(())
}

fn list(dir: &Path, json: bool) -> Result<()> {
    let statuses = statuses_in(dir)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&statuses)?);
        return Ok(());
    }
    if statuses.is_empty() {
        println!("No extra targets. Add one with `tokscale target add <name> --api-url <url> --token <token>`.");
        return Ok(());
    }
    for status in &statuses {
        print_status_line(status);
    }
    Ok(())
}

pub fn print_status_line(status: &TargetStatus) {
    println!(
        "  {} ({}) {} as {}",
        status.name,
        if status.enabled {
            "enabled"
        } else {
            "disabled"
        },
        status.api_url,
        status.username.as_deref().unwrap_or("unknown user"),
    );
    if let Some(at) = status.state.last_success_at_ms {
        println!(
            "    Last upload: {}",
            crate::commands::autosubmit::format_timestamp_ms(at)
        );
    }
    if let Some(error) = &status.state.last_error {
        println!("    Last error: {error}");
    }
    if let Some(error) = &status.state.last_quota_error {
        println!("    Last quota error: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str, api_url: &str, enabled: bool) -> Target {
        Target {
            name: name.to_string(),
            api_url: api_url.to_string(),
            token: format!("tt_{name}"),
            username: Some("user".to_string()),
            enabled,
            created_at: "2026-10-17T00:00:00Z".to_string(),
        }
    }

    fn flags(names: &[&str], none: bool) -> ExtraTargetFlags {
        ExtraTargetFlags {
            names: names.iter().map(|name| name.to_string()).collect(),
            no_extra_targets: none,
        }
    }

    fn names(targets: &[Target]) -> Vec<&str> {
        targets.iter().map(|target| target.name.as_str()).collect()
    }

    #[test]
    fn missing_files_mean_no_targets_and_no_state() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_targets_from(dir.path()).unwrap().is_empty());
        assert!(load_state_from(dir.path()).is_empty());
    }

    #[test]
    fn targets_round_trip_and_default_to_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let saved = vec![target("pa", "https://pa.example.com", false)];
        save_targets_to(dir.path(), &saved).unwrap();
        assert_eq!(load_targets_from(dir.path()).unwrap(), saved);

        std::fs::write(
            dir.path().join(TARGETS_FILE),
            r#"{"targets":[{"name":"pa","apiUrl":"https://pa.example.com","token":"tt_x","createdAt":"t"}]}"#,
        )
        .unwrap();
        assert!(load_targets_from(dir.path()).unwrap()[0].enabled);
    }

    #[test]
    fn corrupt_targets_file_is_an_error_not_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(TARGETS_FILE), "{not json").unwrap();
        assert!(load_targets_from(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn targets_file_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        save_targets_to(dir.path(), &[target("pa", "https://pa.example.com", true)]).unwrap();
        let mode = std::fs::metadata(dir.path().join(TARGETS_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn saving_replaces_an_existing_file() {
        // `fs::rename` must overwrite on Windows too.
        let dir = tempfile::tempdir().unwrap();
        save_targets_to(dir.path(), &[target("a", "https://a.example.com", true)]).unwrap();
        save_targets_to(dir.path(), &[target("b", "https://b.example.com", true)]).unwrap();
        assert_eq!(names(&load_targets_from(dir.path()).unwrap()), ["b"]);
    }

    #[test]
    fn state_never_contains_tokens_and_keeps_last_success() {
        let dir = tempfile::tempdir().unwrap();
        save_targets_to(dir.path(), &[target("pa", "https://pa.example.com", true)]).unwrap();
        let ok = TargetState {
            last_run_at_ms: Some(1),
            last_success_at_ms: Some(1),
            ..TargetState::default()
        };
        record_results(dir.path(), vec![("pa".into(), ok)]).unwrap();
        let failed = TargetState {
            last_run_at_ms: Some(2),
            last_error: Some("boom".into()),
            ..TargetState::default()
        };
        record_results(dir.path(), vec![("pa".into(), failed)]).unwrap();

        let state = load_state_from(dir.path());
        assert_eq!(state["pa"].last_run_at_ms, Some(2));
        assert_eq!(state["pa"].last_success_at_ms, Some(1));
        assert_eq!(state["pa"].last_error.as_deref(), Some("boom"));
        let raw = std::fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        assert!(!raw.contains("tt_pa"));
        let listed = serde_json::to_string(&statuses_in(dir.path()).unwrap()).unwrap();
        assert!(!listed.contains("tt_pa"));
        assert!(listed.contains("\"lastError\":\"boom\""));
    }

    #[test]
    fn selection_defaults_to_enabled_targets() {
        let targets = vec![
            target("pa", "https://pa.example.com", true),
            target("off", "https://off.example.com", false),
        ];
        let (selected, _) = select(targets, &flags(&[], false), "https://tokscale.ai").unwrap();
        assert_eq!(names(&selected), ["pa"]);
    }

    #[test]
    fn named_selection_includes_disabled_targets_and_rejects_unknown_ones() {
        let targets = vec![
            target("pa", "https://pa.example.com", true),
            target("off", "https://off.example.com", false),
        ];
        let (selected, _) = select(
            targets.clone(),
            &flags(&["off"], false),
            "https://tokscale.ai",
        )
        .unwrap();
        assert_eq!(names(&selected), ["off"]);
        assert!(select(targets, &flags(&["nope"], false), "https://tokscale.ai").is_err());
    }

    #[test]
    fn no_extra_targets_selects_nothing() {
        let targets = vec![target("pa", "https://pa.example.com", true)];
        let (selected, _) = select(targets, &flags(&[], true), "https://tokscale.ai").unwrap();
        assert!(selected.is_empty());
    }

    #[test]
    fn a_target_on_the_primary_origin_is_skipped() {
        let targets = vec![target("pa", "https://pa.example.com/tokscale", true)];
        let (selected, notices) =
            select(targets, &flags(&[], false), "https://pa.example.com").unwrap();
        assert!(selected.is_empty());
        assert_eq!(notices.len(), 1);
    }

    #[test]
    fn targets_with_an_unsafe_saved_url_are_skipped() {
        let targets = vec![
            target("plain", "http://pa.example.com", true),
            target("local", "http://127.0.0.1:8080/", true),
        ];
        let (selected, notices) =
            select(targets, &flags(&[], false), "https://tokscale.ai").unwrap();
        assert_eq!(names(&selected), ["local"]);
        assert_eq!(selected[0].api_url, "http://127.0.0.1:8080");
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("plain"), "{}", notices[0]);
        assert!(!notices[0].contains("tt_plain"), "{}", notices[0]);
    }

    #[cfg(unix)]
    #[test]
    fn a_new_config_dir_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("tokscale");
        save_targets_to(&dir, &[target("pa", "https://pa.example.com", true)]).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn target_names_are_restricted() {
        for valid in ["pa", "pa-cafe", "pa_2", "0x"] {
            assert!(validate_name(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "PA", "-pa", "pa cafe", "pa/../x", &"a".repeat(33)] {
            assert!(validate_name(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn upsert_replaces_by_name() {
        let mut targets = vec![target("pa", "https://old.example.com", false)];
        assert!(upsert(
            &mut targets,
            target("pa", "https://new.example.com", true)
        ));
        assert!(!upsert(
            &mut targets,
            target("pb", "https://pb.example.com", true)
        ));
        assert_eq!(targets[0].api_url, "https://new.example.com");
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn errors_report_the_server_message_and_redact_tokens() {
        let status = reqwest::StatusCode::BAD_GATEWAY;
        assert_eq!(
            error_message(br#"{"error":"down"}"#, "fallback", status),
            "down (502 Bad Gateway)"
        );
        assert_eq!(
            error_message(b"<html>", "fallback", status),
            "fallback (502 Bad Gateway)"
        );
        assert_eq!(redact("bad tt_secret", "tt_secret"), "bad [REDACTED]");
    }

    #[test]
    fn http_client_builds_with_bounded_timeouts() {
        assert!(http_client(SUBMIT_TIMEOUT).is_ok());
        assert!(CONNECT_TIMEOUT < SUBMIT_TIMEOUT);
        assert!(SUBMIT_TIMEOUT <= Duration::from_secs(120));
    }
}
