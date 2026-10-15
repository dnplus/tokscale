use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::IsTerminal;
use std::io::Write;
use std::path::PathBuf;

const API_TOKEN_ENV_VAR: &str = "TOKSCALE_API_TOKEN";
const API_URL_ENV_VAR: &str = "TOKSCALE_API_URL";
pub const DEFAULT_API_URL: &str = "https://tokscale.ai";

fn home_dir() -> Result<PathBuf> {
    crate::paths::home_dir().context("Could not determine home directory")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub token: String,
    pub username: String,
    #[serde(rename = "avatarUrl", skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// Server the token was issued by. Scheduled runs carry no environment, so
    /// this is how they know where to submit, and it is the only origin the
    /// token is ever sent to. `None` on credentials saved before this field
    /// existed, which are treated as belonging to [`DEFAULT_API_URL`].
    #[serde(rename = "apiUrl", default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiTokenSource {
    Environment,
    StoredCredentials,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiTokenAuth {
    pub token: String,
    pub username: Option<String>,
    pub source: ApiTokenSource,
    /// For stored credentials, the server the token was saved with.
    pub api_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    #[serde(rename = "deviceCode")]
    device_code: String,
    #[serde(rename = "userCode")]
    user_code: String,
    #[serde(rename = "verificationUrl")]
    verification_url: String,
    #[serde(rename = "expiresIn")]
    #[allow(dead_code)]
    expires_in: u64,
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct PollResponse {
    status: String,
    token: Option<String>,
    user: Option<UserInfo>,
    #[allow(dead_code)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserInfo {
    username: String,
    #[serde(rename = "avatarUrl")]
    avatar_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenValidationResponse {
    user: UserInfo,
}

fn get_credentials_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/tokscale/credentials.json"))
}

fn ensure_config_dir() -> Result<()> {
    let config_dir = home_dir()?.join(".config/tokscale");

    if !config_dir.exists() {
        fs::create_dir_all(&config_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

pub fn save_credentials(credentials: &Credentials) -> Result<()> {
    ensure_config_dir()?;
    let path = get_credentials_path()?;
    let json = serde_json::to_string_pretty(credentials)?;

    crate::commands::usage::helpers::atomic_write_secret(&path, json.as_bytes())?;

    Ok(())
}

pub fn load_credentials() -> Option<Credentials> {
    let path = get_credentials_path().ok()?;
    if !path.exists() {
        return None;
    }

    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn load_api_token_from_env() -> Option<String> {
    let token = std::env::var(API_TOKEN_ENV_VAR).ok()?;
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

pub fn resolve_api_token() -> Option<ApiTokenAuth> {
    if let Some(token) = load_api_token_from_env() {
        return Some(ApiTokenAuth {
            token,
            username: None,
            source: ApiTokenSource::Environment,
            api_url: None,
        });
    }

    load_credentials().map(|credentials| ApiTokenAuth {
        token: credentials.token,
        username: Some(credentials.username),
        source: ApiTokenSource::StoredCredentials,
        api_url: credentials.api_url,
    })
}

pub fn clear_credentials() -> Result<bool> {
    let path = get_credentials_path()?;
    if path.exists() {
        fs::remove_file(path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Validate and canonicalize a Tokscale server URL: `https` only (plain `http`
/// is accepted for `localhost`, `127.0.0.1` and `[::1]`), no credentials,
/// query or fragment, and no trailing slash.
pub fn normalize_api_url(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("API URL cannot be empty.");
    }
    let url =
        reqwest::Url::parse(trimmed).with_context(|| format!("Invalid API URL `{trimmed}`"))?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("API URL `{trimmed}` has no host"))?;
    match url.scheme() {
        "https" => {}
        "http" if matches!(host, "localhost" | "127.0.0.1" | "[::1]") => {}
        _ => anyhow::bail!(
            "API URL `{trimmed}` must use https (http is only allowed for localhost)."
        ),
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("API URL `{trimmed}` must not contain credentials.");
    }
    if url.query().is_some() || url.fragment().is_some() {
        anyhow::bail!("API URL `{trimmed}` must not contain a query or fragment.");
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// `scheme://host[:port]` of an already-normalized API URL.
fn api_origin(url: &str) -> Result<String> {
    Ok(reqwest::Url::parse(url)
        .with_context(|| format!("Invalid API URL `{url}`"))?
        .origin()
        .ascii_serialization())
}

fn api_url_from_env() -> Option<String> {
    std::env::var(API_URL_ENV_VAR)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Effective server URL: `TOKSCALE_API_URL`, then the URL saved with the
/// credentials, then [`DEFAULT_API_URL`].
fn resolve_api_base_url_from(env: Option<&str>, stored: Option<&str>) -> Result<String> {
    match (env, stored) {
        (Some(env), _) => normalize_api_url(env)
            .with_context(|| format!("{API_URL_ENV_VAR} is not a valid Tokscale server URL")),
        (None, Some(stored)) => normalize_api_url(stored),
        (None, None) => Ok(DEFAULT_API_URL.to_string()),
    }
}

/// Effective server URL. Requests that send a token use [`api_url_for`]
/// instead, which also checks the token's origin.
pub fn get_api_base_url() -> Result<String> {
    let stored = load_credentials().and_then(|credentials| credentials.api_url);
    resolve_api_base_url_from(api_url_from_env().as_deref(), stored.as_deref())
}

/// Refuse to pair a stored token with a server other than the one it was saved
/// with. An environment token is supplied together with its URL by whoever set
/// the environment, so it is not checked.
fn ensure_token_origin(auth: &ApiTokenAuth, effective_url: &str) -> Result<()> {
    if auth.source != ApiTokenSource::StoredCredentials {
        return Ok(());
    }
    let saved_url = auth.api_url.as_deref().unwrap_or(DEFAULT_API_URL);
    let saved_origin = api_origin(&normalize_api_url(saved_url)?)?;
    let effective_origin = api_origin(effective_url)?;
    if saved_origin != effective_origin {
        anyhow::bail!(
            "Refusing to send the saved Tokscale token to {effective_origin}: it was saved for {saved_origin}. \
             Unset {API_URL_ENV_VAR}, or run `tokscale login --token <token> --api-url {effective_url}` \
             to save a token for that server."
        );
    }
    Ok(())
}

/// Server URL to send `auth`'s token to, after checking that a stored token is
/// not leaving the origin it was saved with.
pub fn api_url_for(auth: &ApiTokenAuth) -> Result<String> {
    let url = get_api_base_url()?;
    ensure_token_origin(auth, &url)?;
    Ok(url)
}

/// URL for a new login: an explicit `--api-url`, then `TOKSCALE_API_URL`,
/// then [`DEFAULT_API_URL`]. A previously saved URL is deliberately ignored so
/// that logging in never silently reuses another server.
fn login_api_url(explicit: Option<&str>) -> Result<String> {
    match explicit {
        Some(url) => normalize_api_url(url),
        None => resolve_api_base_url_from(api_url_from_env().as_deref(), None),
    }
}

fn get_device_name() -> String {
    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());
    format!("CLI on {}", hostname)
}

#[cfg(target_os = "linux")]
fn has_non_empty_env_var(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

#[cfg(target_os = "linux")]
fn should_auto_open_browser() -> bool {
    has_non_empty_env_var("DISPLAY") || has_non_empty_env_var("WAYLAND_DISPLAY")
}

#[cfg(not(target_os = "linux"))]
fn should_auto_open_browser() -> bool {
    true
}

fn open_browser(url: &str) -> bool {
    if !should_auto_open_browser() {
        return false;
    }

    #[cfg(target_os = "macos")]
    {
        return std::process::Command::new("open").arg(url).spawn().is_ok();
    }

    #[cfg(target_os = "windows")]
    {
        return std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .is_ok();
    }

    #[cfg(target_os = "linux")]
    {
        return std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .is_ok();
    }

    #[allow(unreachable_code)]
    false
}

/// Validate a `--device-name` (or `TOKSCALE_DEVICE_NAME`) before any network
/// call, so a bad value fails the login up front instead of after it.
fn validated_device_name(device_name: Option<&str>) -> Result<Option<String>> {
    device_name
        .map(crate::device::validate_device_name)
        .transpose()
}

/// Persist what a successful login established, then report it.
fn finish_login(credentials: &Credentials, device_name: Option<&str>) -> Result<()> {
    use colored::Colorize;

    save_credentials(credentials)?;
    if let Some(name) = device_name {
        crate::device::store_device_name(name)?;
    }

    println!(
        "\n  {}",
        format!("Success! Logged in as {}", credentials.username.bold()).green()
    );
    if let Some(api_url) = credentials.api_url.as_deref() {
        if api_url != DEFAULT_API_URL {
            println!("{}", format!("  Server: {api_url}").bright_black());
        }
    }
    if let Some(name) = device_name {
        println!("{}", format!("  Device name: {name}").bright_black());
    }
    println!(
        "{}",
        "  You can now use 'bunx tokscale@latest submit' to share your usage.\n".bright_black()
    );
    Ok(())
}

pub async fn login(api_url: Option<&str>, device_name: Option<&str>) -> Result<()> {
    use colored::Colorize;

    if let Some(creds) = load_credentials() {
        println!(
            "\n  {}",
            format!("Already logged in as {}", creds.username.bold()).yellow()
        );
        println!(
            "{}",
            "  Run 'bunx tokscale@latest logout' to sign out first.\n".bright_black()
        );
        return Ok(());
    }

    let base_url = login_api_url(api_url)?;
    let device_name = validated_device_name(device_name)?;

    println!("\n  {}\n", "Tokscale - Login".cyan());
    println!("{}", "  Requesting authorization code...".bright_black());

    let client = tokscale_core::http::client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let device_code_response = client
        .post(format!("{}/api/auth/device", base_url))
        .json(&serde_json::json!({
            "deviceName": get_device_name()
        }))
        .send()
        .await?;

    if !device_code_response.status().is_success() {
        anyhow::bail!("Server returned {}", device_code_response.status());
    }

    let device_data: DeviceCodeResponse = device_code_response.json().await?;

    println!();
    println!("{}", "  Open this URL in your browser:".white());
    let url_display = if std::io::stdout().is_terminal() {
        format!(
            "\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\",
            device_data.verification_url, device_data.verification_url
        )
    } else {
        device_data.verification_url.clone()
    };
    println!("{}", format!("  {}\n", url_display).cyan());
    println!("{}", "  Enter this code:".white());
    println!(
        "{}\n",
        format!("  {}", device_data.user_code).green().bold()
    );

    if !open_browser(&device_data.verification_url) {
        println!(
            "{}",
            "  Browser auto-open unavailable in this environment. Continue with the URL above.\n"
                .bright_black()
        );
    }

    println!("{}", "  Waiting for authorization...".bright_black());

    let poll_interval = std::time::Duration::from_secs(device_data.interval);
    let max_attempts = 180;

    for attempt in 0..max_attempts {
        tokio::time::sleep(poll_interval).await;

        let poll_response = client
            .post(format!("{}/api/auth/device/poll", base_url))
            .json(&serde_json::json!({
                "deviceCode": device_data.device_code
            }))
            .send()
            .await;

        match poll_response {
            Ok(response) => {
                if let Ok(data) = response.json::<PollResponse>().await {
                    if data.status == "complete" {
                        if let (Some(token), Some(user)) = (data.token, data.user) {
                            let credentials = Credentials {
                                token,
                                username: user.username,
                                avatar_url: user.avatar_url,
                                created_at: chrono::Utc::now().to_rfc3339(),
                                api_url: Some(base_url.clone()),
                            };
                            return finish_login(&credentials, device_name.as_deref());
                        }
                    }

                    if data.status == "expired" {
                        anyhow::bail!("Authorization code expired. Please try again.");
                    }

                    print!("{}", ".".bright_black());
                    use std::io::Write;
                    std::io::stdout().flush()?;
                }
            }
            Err(_) => {
                print!("{}", "!".red());
                use std::io::Write;
                std::io::stdout().flush()?;
            }
        }

        if attempt >= max_attempts - 1 {
            anyhow::bail!("Timeout: Authorization took too long. Please try again.");
        }
    }

    Ok(())
}

pub async fn login_with_token(
    token: &str,
    api_url: Option<&str>,
    device_name: Option<&str>,
) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        anyhow::bail!("API token cannot be empty.");
    }
    if !token.starts_with("tt_") {
        anyhow::bail!("Tokscale API tokens must start with `tt_`.");
    }

    let base_url = login_api_url(api_url)?;
    let device_name = validated_device_name(device_name)?;
    let client = tokscale_core::http::client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let response = client
        .get(format!("{}/api/auth/token", base_url))
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        let error = body
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("API token validation failed");
        anyhow::bail!("{} ({})", error, status);
    }

    let data: TokenValidationResponse = response.json().await?;
    let credentials = Credentials {
        token: token.to_string(),
        username: data.user.username,
        avatar_url: data.user.avatar_url,
        created_at: chrono::Utc::now().to_rfc3339(),
        api_url: Some(base_url),
    };
    finish_login(&credentials, device_name.as_deref())
}

pub fn logout() -> Result<()> {
    use colored::Colorize;

    let credentials = load_credentials();

    let Some(creds) = credentials else {
        println!("\n  {}\n", "Not logged in.".yellow());
        return Ok(());
    };

    let username = creds.username;
    let cleared = clear_credentials()?;

    if cleared {
        println!(
            "\n  {}\n",
            format!("Logged out from {}", username.bold()).green()
        );
    } else {
        anyhow::bail!("Failed to clear credentials.");
    }

    Ok(())
}

pub fn whoami() -> Result<()> {
    use colored::Colorize;

    let Some(creds) = load_credentials() else {
        println!("\n  {}", "Not logged in.".yellow());
        println!(
            "{}",
            "  Run 'bunx tokscale@latest login' to authenticate.\n".bright_black()
        );
        return Ok(());
    };

    println!("\n  {}\n", "Tokscale - Account Info".cyan());
    println!(
        "{}",
        format!("  Username:  {}", creds.username.bold()).white()
    );

    println!(
        "{}",
        format!(
            "  Server:    {}",
            creds.api_url.as_deref().unwrap_or(DEFAULT_API_URL)
        )
        .white()
    );

    if let Ok(created) = chrono::DateTime::parse_from_rfc3339(&creds.created_at) {
        println!(
            "{}",
            format!("  Logged in: {}", created.format("%Y-%m-%d")).bright_black()
        );
    }

    println!();

    Ok(())
}

/// Build the JSON payload encoded into the login QR code.
///
/// Uses `serde_json` so that usernames or tokens containing `"` / `\` cannot
/// break the payload structure or inject extra fields. Exposed for tests.
pub(crate) fn qr_login_payload(token: &str, username: &str) -> Result<String> {
    serde_json::to_string(&serde_json::json!({
        "token": token,
        "username": username,
    }))
    .context("Failed to encode QR payload")
}

pub fn show_qr(yes: bool) -> Result<()> {
    use colored::Colorize;
    use qrcode::render::unicode;
    use qrcode::QrCode;

    let Some(creds) = load_credentials() else {
        println!("\n  {}", "Not logged in.".yellow());
        println!(
            "{}",
            "  Run 'bunx tokscale@latest login' to authenticate.\n".bright_black()
        );
        return Ok(());
    };

    // Anyone who can see the terminal can scan and replay the token: screen
    // shares, recorded demos, office cameras, shoulder surfing. Block unless
    // the user explicitly confirms (or passes --yes for scripted use).
    println!();
    println!(
        "  {}",
        "⚠  This will render your API token as a QR code on screen.".yellow()
    );
    println!(
        "  {}",
        "Anyone who can see your terminal (screen share, recording, camera)".bright_black()
    );
    println!(
        "  {}",
        "can scan it and gain full access to your tokscale account.".bright_black()
    );
    println!();

    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("Refusing to render token QR: stdin is not a TTY. Pass --yes to bypass.");
        }
        print!("  Continue? [y/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .context("Failed to read confirmation")?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("\n  {}\n", "Aborted.".bright_black());
            return Ok(());
        }
    }

    let payload = qr_login_payload(&creds.token, &creds.username)?;
    let code = QrCode::new(payload.as_bytes()).context("Failed to generate QR code")?;

    let image = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build();

    println!("\n  {}\n", "Tokscale - API Token QR Code".cyan());
    println!("  {}\n", "Scan to get your API token:".bright_black());

    for line in image.lines() {
        println!("  {}", line);
    }

    println!("\n  {}: {}\n", "User".bright_black(), creds.username.bold());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    struct TestEnvGuard {
        name: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl TestEnvGuard {
        fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let original = env::var_os(name);
            unsafe {
                env::set_var(name, value);
            }
            Self { name, original }
        }
    }

    impl Drop for TestEnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => unsafe {
                    env::set_var(self.name, value);
                },
                None => unsafe {
                    env::remove_var(self.name);
                },
            }
        }
    }

    #[cfg(target_os = "linux")]
    struct EnvVarGuard {
        name: &'static str,
        original: Option<std::ffi::OsString>,
    }

    #[cfg(target_os = "linux")]
    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let original = env::var_os(name);
            unsafe {
                env::set_var(name, value);
            }
            Self { name, original }
        }

        fn remove(name: &'static str) -> Self {
            let original = env::var_os(name);
            unsafe {
                env::remove_var(name);
            }
            Self { name, original }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => unsafe {
                    env::set_var(self.name, value);
                },
                None => unsafe {
                    env::remove_var(self.name);
                },
            }
        }
    }

    fn stored_auth(api_url: Option<&str>) -> ApiTokenAuth {
        ApiTokenAuth {
            token: "tt_saved".to_string(),
            username: Some("saved-user".to_string()),
            source: ApiTokenSource::StoredCredentials,
            api_url: api_url.map(str::to_string),
        }
    }

    fn credentials_for(api_url: Option<&str>) -> Credentials {
        Credentials {
            token: "tt_saved".to_string(),
            username: "saved-user".to_string(),
            avatar_url: None,
            created_at: "2026-10-15T00:00:00Z".to_string(),
            api_url: api_url.map(str::to_string),
        }
    }

    #[test]
    #[serial]
    fn test_get_api_base_url_default() {
        let temp_dir = TempDir::new().unwrap();
        let _home = TestEnvGuard::set("HOME", temp_dir.path());
        let _url = TestEnvGuard::set("TOKSCALE_API_URL", "");
        assert_eq!(get_api_base_url().unwrap(), "https://tokscale.ai");
    }

    #[test]
    #[serial]
    fn test_get_api_base_url_custom() {
        let temp_dir = TempDir::new().unwrap();
        let _home = TestEnvGuard::set("HOME", temp_dir.path());
        let _url = TestEnvGuard::set("TOKSCALE_API_URL", "https://custom.api.url/");
        assert_eq!(get_api_base_url().unwrap(), "https://custom.api.url");
    }

    #[test]
    #[serial]
    fn test_get_api_base_url_uses_saved_url_without_env() {
        let temp_dir = TempDir::new().unwrap();
        let _home = TestEnvGuard::set("HOME", temp_dir.path());
        let _url = TestEnvGuard::set("TOKSCALE_API_URL", "");
        save_credentials(&credentials_for(Some("https://pa.example.com"))).unwrap();
        assert_eq!(get_api_base_url().unwrap(), "https://pa.example.com");
    }

    #[test]
    fn api_url_resolution_prefers_env_then_saved_then_default() {
        assert_eq!(
            resolve_api_base_url_from(
                Some("https://env.example.com"),
                Some("https://saved.example.com")
            )
            .unwrap(),
            "https://env.example.com"
        );
        assert_eq!(
            resolve_api_base_url_from(None, Some("https://saved.example.com/")).unwrap(),
            "https://saved.example.com"
        );
        assert_eq!(
            resolve_api_base_url_from(None, None).unwrap(),
            DEFAULT_API_URL
        );
        assert!(resolve_api_base_url_from(Some("http://env.example.com"), None).is_err());
    }

    #[test]
    fn normalize_api_url_strips_trailing_slashes_and_lowercases_host() {
        assert_eq!(
            normalize_api_url("  https://PA.Example.com//  ").unwrap(),
            "https://pa.example.com"
        );
        assert_eq!(
            normalize_api_url("https://example.com/tokscale/").unwrap(),
            "https://example.com/tokscale"
        );
        assert_eq!(
            normalize_api_url("https://example.com:8443").unwrap(),
            "https://example.com:8443"
        );
    }

    #[test]
    fn normalize_api_url_requires_https_except_for_localhost() {
        assert_eq!(
            normalize_api_url("http://localhost:3000/").unwrap(),
            "http://localhost:3000"
        );
        assert_eq!(
            normalize_api_url("http://127.0.0.1:3000").unwrap(),
            "http://127.0.0.1:3000"
        );
        assert_eq!(
            normalize_api_url("http://[::1]:3000").unwrap(),
            "http://[::1]:3000"
        );
        for rejected in [
            "",
            "pa.example.com",
            "http://pa.example.com",
            "http://localhost.example.com",
            "ftp://pa.example.com",
            "https://user:pass@pa.example.com",
            "https://pa.example.com/?next=1",
            "https://pa.example.com/#top",
        ] {
            assert!(
                normalize_api_url(rejected).is_err(),
                "{rejected:?} should be rejected"
            );
        }
    }

    #[test]
    fn origin_guard_allows_the_saved_origin() {
        let auth = stored_auth(Some("https://pa.example.com"));
        assert!(ensure_token_origin(&auth, "https://pa.example.com").is_ok());
        // Path differences stay on the same origin.
        assert!(ensure_token_origin(&auth, "https://pa.example.com/api-prefix").is_ok());
    }

    #[test]
    fn origin_guard_refuses_a_different_origin_for_a_saved_token() {
        let auth = stored_auth(Some("https://pa.example.com"));
        for other in [
            "https://tokscale.ai",
            "https://evil.example.com",
            "https://pa.example.com:8443",
            "http://localhost:3000",
        ] {
            let err = ensure_token_origin(&auth, other).unwrap_err().to_string();
            assert!(
                err.contains("Refusing to send the saved Tokscale token"),
                "{err}"
            );
            assert!(err.contains("https://pa.example.com"), "{err}");
        }
    }

    #[test]
    fn origin_guard_treats_legacy_credentials_as_the_default_server() {
        let auth = stored_auth(None);
        assert!(ensure_token_origin(&auth, DEFAULT_API_URL).is_ok());
        assert!(ensure_token_origin(&auth, "https://pa.example.com").is_err());
    }

    #[test]
    fn origin_guard_does_not_apply_to_env_tokens() {
        let auth = ApiTokenAuth {
            token: "tt_env".to_string(),
            username: None,
            source: ApiTokenSource::Environment,
            api_url: None,
        };
        assert!(ensure_token_origin(&auth, "https://pa.example.com").is_ok());
    }

    #[test]
    #[serial]
    fn api_url_for_refuses_env_url_that_moves_a_saved_token() {
        let temp_dir = TempDir::new().unwrap();
        let _home = TestEnvGuard::set("HOME", temp_dir.path());
        let _token = TestEnvGuard::set("TOKSCALE_API_TOKEN", "");
        save_credentials(&credentials_for(Some("https://pa.example.com"))).unwrap();
        let auth = resolve_api_token().unwrap();

        {
            let _url = TestEnvGuard::set("TOKSCALE_API_URL", "");
            assert_eq!(api_url_for(&auth).unwrap(), "https://pa.example.com");
        }
        {
            let _url = TestEnvGuard::set("TOKSCALE_API_URL", "https://tokscale.ai");
            assert!(api_url_for(&auth).is_err());
        }
    }

    #[test]
    #[serial]
    fn login_api_url_prefers_flag_then_env_and_ignores_saved_url() {
        let temp_dir = TempDir::new().unwrap();
        let _home = TestEnvGuard::set("HOME", temp_dir.path());
        save_credentials(&credentials_for(Some("https://saved.example.com"))).unwrap();
        let _url = TestEnvGuard::set("TOKSCALE_API_URL", "https://env.example.com");

        assert_eq!(
            login_api_url(Some("https://flag.example.com/")).unwrap(),
            "https://flag.example.com"
        );
        assert_eq!(login_api_url(None).unwrap(), "https://env.example.com");
        assert!(login_api_url(Some("http://flag.example.com")).is_err());
    }

    #[test]
    #[serial]
    fn saved_api_url_round_trips_and_reaches_resolved_token() {
        let temp_dir = TempDir::new().unwrap();
        let _home = TestEnvGuard::set("HOME", temp_dir.path());
        let _token = TestEnvGuard::set("TOKSCALE_API_TOKEN", "");
        save_credentials(&credentials_for(Some("https://pa.example.com"))).unwrap();

        let raw = fs::read_to_string(get_credentials_path().unwrap()).unwrap();
        assert!(
            raw.contains("\"apiUrl\": \"https://pa.example.com\""),
            "{raw}"
        );
        assert_eq!(
            load_credentials().unwrap().api_url.as_deref(),
            Some("https://pa.example.com")
        );
        let auth = resolve_api_token().unwrap();
        assert_eq!(auth.source, ApiTokenSource::StoredCredentials);
        assert_eq!(auth.api_url.as_deref(), Some("https://pa.example.com"));
    }

    #[test]
    fn credentials_without_api_url_still_load() {
        let legacy = r#"{"token":"tt_old","username":"old","createdAt":"2024-01-01T00:00:00Z"}"#;
        let creds: Credentials = serde_json::from_str(legacy).unwrap();
        assert_eq!(creds.api_url, None);
        assert!(!serde_json::to_string(&creds).unwrap().contains("apiUrl"));
    }

    #[test]
    fn test_credentials_serialization() {
        let creds = Credentials {
            token: "test_token_123".to_string(),
            username: "testuser".to_string(),
            avatar_url: Some("https://example.com/avatar.png".to_string()),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            api_url: None,
        };

        let json = serde_json::to_string(&creds).unwrap();
        let deserialized: Credentials = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.token, creds.token);
        assert_eq!(deserialized.username, creds.username);
        assert_eq!(deserialized.avatar_url, creds.avatar_url);
        assert_eq!(deserialized.created_at, creds.created_at);
    }

    #[test]
    fn test_credentials_serialization_without_avatar() {
        let creds = Credentials {
            token: "test_token_456".to_string(),
            username: "testuser2".to_string(),
            avatar_url: None,
            created_at: "2024-01-02T00:00:00Z".to_string(),
            api_url: None,
        };

        let json = serde_json::to_string(&creds).unwrap();
        let deserialized: Credentials = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.token, creds.token);
        assert_eq!(deserialized.username, creds.username);
        assert_eq!(deserialized.avatar_url, None);
        assert_eq!(deserialized.created_at, creds.created_at);

        assert!(!json.contains("avatarUrl"));
    }

    #[test]
    #[serial]
    #[cfg(target_os = "linux")]
    fn test_should_not_auto_open_browser_without_desktop_session() {
        let _display = EnvVarGuard::remove("DISPLAY");
        let _wayland = EnvVarGuard::remove("WAYLAND_DISPLAY");

        assert!(!should_auto_open_browser());
    }

    #[test]
    #[serial]
    #[cfg(target_os = "linux")]
    fn test_should_auto_open_browser_with_display() {
        let _display = EnvVarGuard::set("DISPLAY", ":0");
        let _wayland = EnvVarGuard::remove("WAYLAND_DISPLAY");

        assert!(should_auto_open_browser());
    }

    #[test]
    #[serial]
    #[cfg(target_os = "linux")]
    fn test_should_auto_open_browser_with_wayland_display() {
        let _display = EnvVarGuard::remove("DISPLAY");
        let _wayland = EnvVarGuard::set("WAYLAND_DISPLAY", "wayland-0");

        assert!(should_auto_open_browser());
    }

    #[test]
    #[serial]
    fn test_get_credentials_path() {
        let temp_dir = TempDir::new().unwrap();
        unsafe {
            env::set_var("HOME", temp_dir.path());
        }

        let path = get_credentials_path().unwrap();
        let expected = temp_dir.path().join(".config/tokscale/credentials.json");

        assert_eq!(path, expected);

        unsafe {
            env::remove_var("HOME");
        }
    }

    #[test]
    #[serial]
    fn test_save_credentials() {
        let temp_dir = TempDir::new().unwrap();
        unsafe {
            env::set_var("HOME", temp_dir.path());
        }

        let creds = Credentials {
            token: "save_test_token".to_string(),
            username: "saveuser".to_string(),
            avatar_url: Some("https://example.com/save.png".to_string()),
            created_at: "2024-01-03T00:00:00Z".to_string(),
            api_url: None,
        };

        save_credentials(&creds).unwrap();

        let path = get_credentials_path().unwrap();
        assert!(path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = fs::metadata(&path).unwrap();
            let permissions = metadata.permissions();
            assert_eq!(permissions.mode() & 0o777, 0o600);
        }

        let content = fs::read_to_string(&path).unwrap();
        let loaded: Credentials = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.token, creds.token);
        assert_eq!(loaded.username, creds.username);

        unsafe {
            env::remove_var("HOME");
        }
    }

    #[test]
    #[serial]
    #[cfg(unix)]
    fn saving_credentials_replaces_public_files_and_does_not_follow_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = TempDir::new().unwrap();
        let _home = TestEnvGuard::set("HOME", dir.path());
        let path = get_credentials_path().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old fixture").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let creds = Credentials {
            token: "synthetic-test-token".into(),
            username: "fixture".into(),
            avatar_url: None,
            created_at: "2026-10-09T00:00:00Z".into(),
            api_url: None,
        };
        save_credentials(&creds).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(load_credentials().unwrap().username, "fixture");

        let target = dir.path().join("unrelated-file");
        fs::write(&target, "must remain unchanged").unwrap();
        fs::remove_file(&path).unwrap();
        symlink(&target, &path).unwrap();
        save_credentials(&creds).unwrap();
        assert!(!fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(target).unwrap(), "must remain unchanged");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    #[serial]
    fn test_load_credentials() {
        let temp_dir = TempDir::new().unwrap();
        unsafe {
            env::set_var("HOME", temp_dir.path());
        }

        let creds = Credentials {
            token: "load_test_token".to_string(),
            username: "loaduser".to_string(),
            avatar_url: None,
            created_at: "2024-01-04T00:00:00Z".to_string(),
            api_url: None,
        };

        save_credentials(&creds).unwrap();

        let loaded = load_credentials().unwrap();

        assert_eq!(loaded.token, creds.token);
        assert_eq!(loaded.username, creds.username);
        assert_eq!(loaded.avatar_url, creds.avatar_url);
        assert_eq!(loaded.created_at, creds.created_at);

        unsafe {
            env::remove_var("HOME");
        }
    }

    #[test]
    #[serial]
    fn test_load_credentials_nonexistent() {
        let temp_dir = TempDir::new().unwrap();
        unsafe {
            env::set_var("HOME", temp_dir.path());
        }

        let loaded = load_credentials();
        assert!(loaded.is_none());

        unsafe {
            env::remove_var("HOME");
        }
    }

    #[test]
    #[serial]
    fn test_clear_credentials() {
        let temp_dir = TempDir::new().unwrap();
        unsafe {
            env::set_var("HOME", temp_dir.path());
        }

        let creds = Credentials {
            token: "clear_test_token".to_string(),
            username: "clearuser".to_string(),
            avatar_url: None,
            created_at: "2024-01-05T00:00:00Z".to_string(),
            api_url: None,
        };

        save_credentials(&creds).unwrap();
        let path = get_credentials_path().unwrap();
        assert!(path.exists());

        let cleared = clear_credentials().unwrap();
        assert!(cleared);
        assert!(!path.exists());

        unsafe {
            env::remove_var("HOME");
        }
    }

    #[test]
    #[serial]
    fn test_clear_credentials_nonexistent() {
        let temp_dir = TempDir::new().unwrap();
        unsafe {
            env::set_var("HOME", temp_dir.path());
        }

        let cleared = clear_credentials().unwrap();
        assert!(!cleared);

        unsafe {
            env::remove_var("HOME");
        }
    }

    #[test]
    #[serial]
    fn test_ensure_config_dir() {
        let temp_dir = TempDir::new().unwrap();
        unsafe {
            env::set_var("HOME", temp_dir.path());
        }

        let config_dir = temp_dir.path().join(".config/tokscale");
        assert!(!config_dir.exists());

        ensure_config_dir().unwrap();

        assert!(config_dir.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = fs::metadata(&config_dir).unwrap();
            let permissions = metadata.permissions();
            assert_eq!(permissions.mode() & 0o777, 0o700);
        }

        unsafe {
            env::remove_var("HOME");
        }
    }

    #[test]
    #[serial]
    fn test_save_and_load_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        unsafe {
            env::set_var("HOME", temp_dir.path());
        }

        let original = Credentials {
            token: "roundtrip_token".to_string(),
            username: "roundtripuser".to_string(),
            avatar_url: Some("https://example.com/roundtrip.png".to_string()),
            created_at: "2024-01-06T12:34:56Z".to_string(),
            api_url: None,
        };

        save_credentials(&original).unwrap();
        let loaded = load_credentials().unwrap();

        assert_eq!(loaded.token, original.token);
        assert_eq!(loaded.username, original.username);
        assert_eq!(loaded.avatar_url, original.avatar_url);
        assert_eq!(loaded.created_at, original.created_at);

        unsafe {
            env::remove_var("HOME");
        }
    }

    #[test]
    #[serial]
    fn test_load_api_token_from_env_trims_value() {
        let _token = TestEnvGuard::set("TOKSCALE_API_TOKEN", "  tt_ci_token  ");

        assert_eq!(load_api_token_from_env(), Some("tt_ci_token".to_string()));
    }

    #[test]
    #[serial]
    fn test_load_api_token_from_env_ignores_empty_values() {
        let _token = TestEnvGuard::set("TOKSCALE_API_TOKEN", "   ");

        assert_eq!(load_api_token_from_env(), None);
    }

    #[test]
    #[serial]
    fn test_resolve_api_token_prefers_env_over_saved_credentials() {
        let temp_dir = TempDir::new().unwrap();
        let _home = TestEnvGuard::set("HOME", temp_dir.path());
        let _token = TestEnvGuard::set("TOKSCALE_API_TOKEN", "tt_env_token");

        save_credentials(&Credentials {
            token: "tt_saved_token".to_string(),
            username: "saved-user".to_string(),
            avatar_url: None,
            created_at: "2024-01-01T00:00:00Z".to_string(),
            api_url: None,
        })
        .unwrap();

        let auth = resolve_api_token().unwrap();

        assert_eq!(auth.token, "tt_env_token");
        assert_eq!(auth.username.as_deref(), None);
        assert_eq!(auth.source, ApiTokenSource::Environment);
    }

    #[test]
    fn qr_login_payload_round_trips_through_json() {
        let payload = qr_login_payload("tok_abc123", "alice").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["token"], "tok_abc123");
        assert_eq!(parsed["username"], "alice");
    }

    #[test]
    fn qr_login_payload_escapes_dangerous_username_chars() {
        // The old hand-rolled format!() would have produced invalid JSON for
        // any of these inputs; serde_json must escape them safely.
        for bad in [
            "alice\"; DROP TABLE users;--",
            "alice\\",
            "with\nnewline",
            "tab\there",
            r#"quote"and\backslash"#,
        ] {
            let payload = qr_login_payload("tok", bad).unwrap();
            let parsed: serde_json::Value =
                serde_json::from_str(&payload).expect("must remain valid JSON");
            assert_eq!(
                parsed["username"], bad,
                "round-trip must preserve the original username"
            );
        }
    }

    #[test]
    fn qr_login_payload_escapes_dangerous_token_chars() {
        let payload = qr_login_payload(r#"tok"with"quotes"#, "bob").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["token"], r#"tok"with"quotes"#);
        assert_eq!(parsed["username"], "bob");
    }
}
