//! Cursor subscription plan remaining for `tokscale usage`.
//!
//! Auth comes from the desktop `state.vscdb` JWT, then the macOS Keychain items
//! `cursor-access-token` / `cursor-refresh-token`, then a JWT embedded in a
//! saved Tokscale Cursor session cookie. The IDE plan window is
//! `GetCurrentPeriodUsage` on `api2.cursor.sh`. Grok Bot weekly usage is a
//! separate Cursor-metered pool from `GetSandUsageStatus` (not the SuperGrok
//! `cli-chat-proxy` credits used by the Grok / Grok Build card).
//!
//! On macOS, Grok Bot.app may store multiple Cursor accounts under
//! `~/Library/Application Support/Grok Bot/sand-secrets.json` (`cursor-accounts`),
//! encrypted with Electron safeStorage. Those tokens are decrypted via the
//! Keychain item `Grok Bot Safe Storage` / `Grok Bot Key` so each signed-in
//! account can get its own Grok Bot card. Refreshed access tokens stay in
//! memory and are never written back.

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use serde_json::Value;

use super::{UsageAccount, UsageMetric, UsageOutput};

const USAGE_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
const PLAN_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetPlanInfo";
/// Grok Bot weekly included usage. Metered on the Cursor account, not on xAI.
const SAND_USAGE_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetSandUsageStatus";
const REFRESH_URL: &str = "https://api2.cursor.sh/oauth/token";
/// Public Cursor Auth0 client id. Not a user secret.
const CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";
const ACCESS_SERVICE: &str = "cursor-access-token";
const REFRESH_SERVICE: &str = "cursor-refresh-token";
const PROVIDER: &str = "Cursor";
const GROK_BOT_PROVIDER: &str = "Grok Bot";
/// Chromium / Electron safeStorage KDF salt and iteration count.
const SAFE_STORAGE_SALT: &[u8] = b"saltysalt";
const SAFE_STORAGE_ITERATIONS: u32 = 1003;

#[derive(Debug, Clone, PartialEq)]
struct ParsedPlanWindow {
    used_percent: f64,
    remaining_percent: f64,
    reset_at: Option<String>,
}

/// Cursor meters Auto models and named/API models on separate included-usage
/// tracks. `totalPercentUsed` is a blended figure; the UI wants the two tracks.
#[derive(Debug, Clone, PartialEq)]
struct ParsedPlanTracks {
    auto: Option<ParsedPlanWindow>,
    api: Option<ParsedPlanWindow>,
    /// Fallback when neither track percent is present.
    total: Option<ParsedPlanWindow>,
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

/// Grok Bot.app multi-account store, or Cursor desktop auth as a fallback.
pub fn has_grok_bot_credentials() -> bool {
    grok_bot_secrets_path().is_some_and(|path| path.is_file()) || has_credentials()
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
        let tracks = parse_plan_tracks(&usage, plan.as_ref())?;
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
        let metrics = metrics_from_tracks(&tracks);
        if metrics.is_empty() {
            anyhow::bail!("Cursor planUsage had no Auto, API, or total percent");
        }

        Ok(UsageOutput {
            provider: PROVIDER.to_string(),
            account: None,
            credential_source: Some("desktop".into()),
            plan: plan_name,
            email: auth.email,
            metrics,
            reset_credits: None,
            credit_status: None,
            spend_control: None,
        })
    })
}

/// One Grok Bot card per signed-in Cursor account in Grok Bot.app when that
/// store decrypts; otherwise a single card from Cursor desktop auth.
pub fn fetch_grok_bot_all() -> Result<Vec<UsageOutput>> {
    match load_grok_bot_app_accounts() {
        Ok(accounts) if !accounts.is_empty() => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(async {
                let client = plan_http_client()?;
                let mut outputs = Vec::new();
                let mut errors = Vec::new();
                for account in accounts {
                    match fetch_sand_for_token(&client, &account.access_token).await {
                        Ok(parsed) => outputs.push(UsageOutput {
                            provider: GROK_BOT_PROVIDER.to_string(),
                            account: Some(UsageAccount {
                                id: account.id,
                                label: account
                                    .name
                                    .clone()
                                    .or_else(|| account.email.clone()),
                                is_active: account.is_active,
                            }),
                            credential_source: Some("grok-bot-app".into()),
                            plan: parsed.plan,
                            email: account.email,
                            metrics: vec![parsed.metric],
                            reset_credits: None,
                            credit_status: None,
                            spend_control: None,
                        }),
                        Err(error) => errors.push(error.to_string()),
                    }
                }
                if outputs.is_empty() {
                    anyhow::bail!(
                        "Grok Bot app accounts found but Sand usage failed: {}",
                        errors.join("; ")
                    );
                }
                // Active account first so the card order matches the app switcher.
                outputs.sort_by_key(|output| {
                    !output
                        .account
                        .as_ref()
                        .map(|account| account.is_active)
                        .unwrap_or(false)
                });
                Ok(outputs)
            })
        }
        Ok(_) | Err(_) => fetch_grok_bot_desktop().map(|output| vec![output]),
    }
}

fn fetch_grok_bot_desktop() -> Result<UsageOutput> {
    let auth = resolve_local_auth()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let now = Utc::now().timestamp();
        let access = bearer_for_request(&auth, now).await?;
        let client = plan_http_client()?;
        let parsed = fetch_sand_for_token(&client, &access).await?;
        Ok(UsageOutput {
            provider: GROK_BOT_PROVIDER.to_string(),
            account: None,
            credential_source: Some("desktop".into()),
            plan: parsed.plan,
            email: auth.email,
            metrics: vec![parsed.metric],
            reset_credits: None,
            credit_status: None,
            spend_control: None,
        })
    })
}

async fn fetch_sand_for_token(
    client: &reqwest::Client,
    access: &str,
) -> Result<ParsedSandUsage> {
    let sand = connect_post(client, SAND_USAGE_URL, access, "GetSandUsageStatus").await?;
    parse_sand_usage(&sand)
}

#[derive(Debug, Clone)]
struct GrokBotAppAccount {
    id: String,
    access_token: String,
    email: Option<String>,
    name: Option<String>,
    is_active: bool,
}

fn grok_bot_secrets_path() -> Option<std::path::PathBuf> {
    crate::paths::home_dir().map(|home| {
        home.join("Library/Application Support/Grok Bot/sand-secrets.json")
    })
}

/// Decrypt every Cursor account stored by Grok Bot.app (macOS Electron
/// safeStorage). Non-macOS builds return an empty list so the desktop fallback
/// stays in charge.
fn load_grok_bot_app_accounts() -> Result<Vec<GrokBotAppAccount>> {
    #[cfg(not(target_os = "macos"))]
    {
        Ok(Vec::new())
    }

    #[cfg(target_os = "macos")]
    {
        let path = grok_bot_secrets_path()
            .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let secrets: Value =
            serde_json::from_str(&raw).context("Grok Bot sand-secrets.json was not JSON")?;
        let accounts_blob = secrets
            .get("cursor-accounts")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Grok Bot sand-secrets.json had no cursor-accounts"))?;
        let doc: Value = serde_json::from_str(accounts_blob)
            .context("Grok Bot cursor-accounts was not JSON")?;
        let active = doc
            .get("active")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let accounts = doc
            .get("accounts")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow::anyhow!("Grok Bot cursor-accounts had no accounts object"))?;
        let password = read_grok_bot_safe_storage_key()?;
        let mut out = Vec::new();
        for (id, entry) in accounts {
            let Some(entry) = entry.as_object() else {
                continue;
            };
            let Some(cipher) = entry
                .get("cursor-access-token")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|token| !token.is_empty())
            else {
                continue;
            };
            let access_token = match decrypt_electron_safe_storage(cipher, password.as_bytes()) {
                Ok(token) if access_token_usable(&token) => token,
                _ => continue,
            };
            let profile = entry
                .get("cursor-account-profile")
                .and_then(Value::as_str)
                .and_then(|cipher| decrypt_electron_safe_storage(cipher, password.as_bytes()).ok())
                .and_then(|text| serde_json::from_str::<Value>(&text).ok());
            let email = profile
                .as_ref()
                .and_then(|value| value.get("email"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|email| !email.is_empty())
                .map(str::to_string);
            let name = profile
                .as_ref()
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string);
            out.push(GrokBotAppAccount {
                id: id.clone(),
                access_token,
                email,
                name,
                is_active: id == &active,
            });
        }
        if out.is_empty() {
            anyhow::bail!("Grok Bot sand-secrets.json had no decryptable access tokens");
        }
        Ok(out)
    }
}

#[cfg(target_os = "macos")]
fn read_grok_bot_safe_storage_key() -> Result<String> {
    let out = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Grok Bot Safe Storage",
            "-a",
            "Grok Bot Key",
            "-w",
        ])
        .output()
        .context("Failed to invoke macOS security for Grok Bot Safe Storage")?;
    if !out.status.success() {
        anyhow::bail!("Grok Bot Safe Storage Keychain item was not readable");
    }
    Ok(String::from_utf8(out.stdout)
        .context("Grok Bot Safe Storage key was not UTF-8")?
        .trim_end()
        .to_string())
}

/// Decrypt a Chromium / Electron `v10` safeStorage blob (base64).
fn decrypt_electron_safe_storage(cipher_b64: &str, password: &[u8]) -> Result<String> {
    use aes::Aes128;
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    use pbkdf2::pbkdf2_hmac;
    use sha1::Sha1;

    let raw = B64
        .decode(cipher_b64.trim())
        .context("Grok Bot safeStorage blob was not base64")?;
    let payload = raw
        .strip_prefix(b"v10")
        .ok_or_else(|| anyhow::anyhow!("Grok Bot safeStorage blob was not a v10 envelope"))?;
    let mut key = [0u8; 16];
    pbkdf2_hmac::<Sha1>(password, SAFE_STORAGE_SALT, SAFE_STORAGE_ITERATIONS, &mut key);
    let iv = [b' '; 16];
    type Aes128CbcDec = cbc::Decryptor<Aes128>;
    let mut buffer = payload.to_vec();
    let plaintext = Aes128CbcDec::new(&key.into(), &iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map_err(|_| anyhow::anyhow!("Grok Bot safeStorage decrypt failed"))?;
    String::from_utf8(plaintext.to_vec()).context("Grok Bot safeStorage plaintext was not UTF-8")
}

#[derive(Debug, Clone)]
struct ParsedSandUsage {
    metric: UsageMetric,
    plan: Option<String>,
}

/// Parse `GetSandUsageStatus`. Hide when the account has no personal included
/// Grok Bot allowance (pooled enterprise, zero limit, or missing percent).
fn parse_sand_usage(value: &Value) -> Result<ParsedSandUsage> {
    if value
        .get("usesPooledEnterpriseAllowance")
        .and_then(Value::as_bool)
        == Some(true)
    {
        anyhow::bail!("Grok Bot uses a pooled enterprise allowance with no personal share");
    }
    if value.get("hasNonZeroIncludedLimit").and_then(Value::as_bool) == Some(false) {
        anyhow::bail!("Grok Bot has no included weekly allowance on this Cursor account");
    }
    let used = value
        .get("usagePercent")
        .and_then(Value::as_f64)
        .filter(|percent| percent.is_finite() && (0.0..=100.0).contains(percent))
        .ok_or_else(|| anyhow::anyhow!("Grok Bot GetSandUsageStatus had no usagePercent"))?;
    let (used_percent, remaining_percent) = round_pair(used);
    let resets_at = sand_reset_iso(value.get("nextResetTimestampUtc"));
    let plan = value
        .get("grokPlanLabel")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_string)
        .or_else(|| {
            value
                .get("cursorPlanName")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        });
    Ok(ParsedSandUsage {
        metric: UsageMetric {
            label: "Weekly".into(),
            used_percent,
            remaining_percent,
            remaining_label: None,
            resets_at,
        },
        plan,
    })
}

fn sand_reset_iso(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    DateTime::parse_from_rfc3339(text).ok().map(|parsed| {
        parsed
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    })
}

fn metrics_from_tracks(tracks: &ParsedPlanTracks) -> Vec<UsageMetric> {
    let mut metrics = Vec::new();
    if let Some(auto) = tracks.auto.as_ref() {
        metrics.push(metric_from_window("Auto", auto, tracks.reset_at.clone()));
    }
    if let Some(api) = tracks.api.as_ref() {
        metrics.push(metric_from_window("API", api, tracks.reset_at.clone()));
    }
    if metrics.is_empty() {
        if let Some(total) = tracks.total.as_ref() {
            metrics.push(metric_from_window("Plan", total, tracks.reset_at.clone()));
        }
    }
    metrics
}

fn metric_from_window(label: &str, window: &ParsedPlanWindow, reset_at: Option<String>) -> UsageMetric {
    UsageMetric {
        label: label.into(),
        used_percent: window.used_percent,
        remaining_percent: window.remaining_percent,
        remaining_label: None,
        resets_at: window.reset_at.clone().or(reset_at),
    }
}

fn plan_http_client() -> Result<reqwest::Client> {
    // Reuse Cursor's TLS selection (#1250), overriding its default 15s timeout.
    crate::cursor::cursor_http_client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("Failed to build Cursor plan HTTP client")
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

pub(super) fn local_cursor_email() -> Option<String> {
    resolve_local_auth().ok().and_then(|auth| auth.email)
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

fn parse_plan_tracks(usage: &Value, plan: Option<&Value>) -> Result<ParsedPlanTracks> {
    let plan_usage = usage
        .get("planUsage")
        .filter(|value| value.is_object())
        .ok_or_else(|| plan_usage_missing(usage))?;
    let reset_at = billing_reset_iso(usage.get("billingCycleEnd")).or_else(|| {
        billing_reset_iso(plan.and_then(|value| value.pointer("/planInfo/billingCycleEnd")))
    });
    let auto = percent_field(plan_usage, "autoPercentUsed").map(|(used, remaining)| {
        ParsedPlanWindow {
            used_percent: used,
            remaining_percent: remaining,
            reset_at: reset_at.clone(),
        }
    });
    let api = percent_field(plan_usage, "apiPercentUsed").map(|(used, remaining)| {
        ParsedPlanWindow {
            used_percent: used,
            remaining_percent: remaining,
            reset_at: reset_at.clone(),
        }
    });
    let total = plan_percents(plan_usage)
        .ok()
        .map(|(used_percent, remaining_percent)| ParsedPlanWindow {
            used_percent,
            remaining_percent,
            reset_at: reset_at.clone(),
        });
    if auto.is_none() && api.is_none() && total.is_none() {
        anyhow::bail!(
            "Cursor planUsage had no finite remaining percent (autoPercentUsed, apiPercentUsed, totalPercentUsed, or remaining and limit)."
        );
    }
    Ok(ParsedPlanTracks {
        auto,
        api,
        total,
        reset_at,
    })
}

fn percent_field(plan_usage: &Value, key: &str) -> Option<(f64, f64)> {
    let used = plan_usage.get(key).and_then(Value::as_f64)?;
    if used.is_finite() && (0.0..=100.0).contains(&used) {
        Some(round_pair(used))
    } else {
        None
    }
}

#[cfg(test)]
fn parse_plan_window(usage: &Value, plan: Option<&Value>) -> Result<ParsedPlanWindow> {
    let tracks = parse_plan_tracks(usage, plan)?;
    tracks
        .total
        .or(tracks.auto)
        .or(tracks.api)
        .ok_or_else(|| anyhow::anyhow!("Cursor planUsage had no usable percent"))
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
    fn test_plan_http_client_applies_thirty_second_timeout() {
        let client = plan_http_client().expect("plan client builds");
        // reqwest has no public timeout getter. Its Debug output exposes the
        // built client's total timeout, verifying the shared 15s was overridden.
        let rendered = format!("{client:?}");
        assert!(
            rendered.contains("TotalTimeout: 30s"),
            "the plan client must retain its 30s timeout, got: {rendered}"
        );
    }

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
    fn parse_plan_tracks_prefers_auto_and_api_over_total() {
        let usage = json!({
            "planUsage": {
                "autoPercentUsed": 16.0,
                "apiPercentUsed": 36.0,
                "totalPercentUsed": 19.0
            },
            "billingCycleEnd": "1767225600000"
        });
        let tracks = parse_plan_tracks(&usage, None).unwrap();
        let metrics = metrics_from_tracks(&tracks);
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].label, "Auto");
        assert!((metrics[0].remaining_percent - 84.0).abs() < f64::EPSILON);
        assert_eq!(metrics[1].label, "API");
        assert!((metrics[1].remaining_percent - 64.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_sand_usage_weekly_percent() {
        let sand = json!({
            "hasNonZeroIncludedLimit": true,
            "usagePercent": 15.954741,
            "nextResetTimestampUtc": "2026-10-06T09:12:29.574Z",
            "grokPlanLabel": "Grok Bot Plan",
            "cursorPlanName": "Ultra"
        });
        let parsed = parse_sand_usage(&sand).unwrap();
        assert_eq!(parsed.metric.label, "Weekly");
        assert!((parsed.metric.used_percent - 15.95).abs() < 0.01);
        assert!((parsed.metric.remaining_percent - 84.05).abs() < 0.01);
        assert_eq!(
            parsed.metric.resets_at.as_deref(),
            Some("2026-10-06T09:12:29.574Z")
        );
        assert_eq!(parsed.plan.as_deref(), Some("Grok Bot Plan"));
    }

    #[test]
    fn parse_sand_usage_hides_pooled_and_zero_limit() {
        assert!(parse_sand_usage(&json!({
            "usesPooledEnterpriseAllowance": true,
            "usagePercent": 10.0,
            "hasNonZeroIncludedLimit": true
        }))
        .is_err());
        assert!(parse_sand_usage(&json!({
            "hasNonZeroIncludedLimit": false,
            "usagePercent": 10.0
        }))
        .is_err());
        assert!(parse_sand_usage(&json!({
            "hasNonZeroIncludedLimit": true
        }))
        .is_err());
    }

    #[test]
    fn decrypt_electron_safe_storage_round_trip() {
        use aes::Aes128;
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        use pbkdf2::pbkdf2_hmac;
        use sha1::Sha1;

        let password = b"unit-test-password";
        let mut key = [0u8; 16];
        pbkdf2_hmac::<Sha1>(password, SAFE_STORAGE_SALT, SAFE_STORAGE_ITERATIONS, &mut key);
        let iv = [b' '; 16];
        let plaintext = b"{\"email\":\"dnplus@example.com\"}";
        let mut buffer = vec![0u8; plaintext.len() + 16];
        buffer[..plaintext.len()].copy_from_slice(plaintext);
        type Aes128CbcEnc = cbc::Encryptor<Aes128>;
        let encrypted_len = Aes128CbcEnc::new(&key.into(), &iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
            .expect("encrypt")
            .len();
        let mut envelope = Vec::with_capacity(3 + encrypted_len);
        envelope.extend_from_slice(b"v10");
        envelope.extend_from_slice(&buffer[..encrypted_len]);
        let cipher_b64 = B64.encode(envelope);
        let decrypted = decrypt_electron_safe_storage(&cipher_b64, password).unwrap();
        assert_eq!(decrypted, "{\"email\":\"dnplus@example.com\"}");
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
