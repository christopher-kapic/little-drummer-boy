//! GitHub Copilot device-code OAuth login.
//!
//! Copilot rides on top of a regular GitHub OAuth device flow. Two
//! network round-trips materialize a usable Copilot API key:
//!
//!   1. POST `{host}/login/device/code` (form-urlencoded
//!      `client_id` + `scope=read:user`) → `{ device_code, user_code,
//!      verification_uri, interval, expires_in }`. The user opens
//!      `verification_uri` in a browser and types the `user_code`.
//!   2. Poll POST `{host}/login/oauth/access_token` with form-urlencoded
//!      `client_id`, `device_code`, and
//!      `grant_type=urn:ietf:params:oauth:grant-type:device_code`.
//!      While the user hasn't authorized, the body is JSON
//!      `{ "error": "authorization_pending" }` at HTTP 200; on
//!      `slow_down` we widen the poll interval by 5s. Success returns
//!      `{ "access_token": "gho_...", ... }`.
//!   3. GET `https://api.github.com/copilot_internal/v2/token` with
//!      `Authorization: token <gh_access_token>` swaps the GitHub
//!      OAuth token for a short-lived Copilot API key
//!      (`tid=…;exp=…;sku=…`, ~30 min lifetime). That key is what the
//!      `api.githubcopilot.com` upstream expects on the wire.
//!
//! No PKCE: GitHub's device flow doesn't use it.
//!
//! Tokens land in `$XDG_STATE_HOME/cockpit/credentials.json` under the
//! `copilot` key:
//!
//! ```json
//! "copilot": {
//!   "gh_access_token":     "gho_...",
//!   "copilot_token":       "tid=...;exp=...;sku=...",
//!   "copilot_expires_at":  "2026-05-26T13:04:56Z",
//!   "saved_at":            "2026-05-26T12:34:56Z"
//! }
//! ```

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::credentials::CredentialStore;

/// OAuth client id used by every open-source Copilot client (neovim,
/// helix, etc.). It's GitHub's published Copilot client.
pub const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// GitHub OAuth host. Override for ghe.com / Enterprise Server.
pub const DEFAULT_HOST: &str = "https://github.com";

/// Copilot's internal token-exchange endpoint. The upstream that
/// actually serves chat completions is `api.githubcopilot.com`.
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";

/// Max time we'll keep polling before giving up. Mirrors codex.rs.
const MAX_POLL_DURATION: Duration = Duration::from_secs(15 * 60);

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// User-Agent / Editor-Version values sent on the Copilot token swap.
/// Copilot's internal endpoint rejects requests without an editor header.
fn editor_version() -> String {
    format!("cockpit-cli/{}", env!("CARGO_PKG_VERSION"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTokens {
    pub gh_access_token: String,
    pub copilot_token: String,
    pub copilot_expires_at: chrono::DateTime<chrono::Utc>,
    pub saved_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct DeviceCode {
    /// URL to direct the user to (e.g. `https://github.com/login/device`).
    pub verification_url: String,
    /// 8-char hyphenated code the user types into the browser page.
    pub user_code: String,
    device_code: String,
    interval: u64,
}

#[derive(Deserialize)]
struct DeviceCodeResp {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default, deserialize_with = "deserialize_interval")]
    interval: u64,
}

fn deserialize_interval<'de, D>(de: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;
    // GitHub returns the interval as a JSON number, but tolerate strings
    // for parity with codex.rs and to survive proxies that re-serialize.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        S(String),
        N(u64),
    }
    match Either::deserialize(de)? {
        Either::S(s) => s.trim().parse::<u64>().map_err(de::Error::custom),
        Either::N(n) => Ok(n),
    }
}

/// Successful OAuth access-token response.
#[derive(Deserialize)]
struct AccessTokenResp {
    access_token: String,
    #[allow(dead_code)]
    #[serde(default)]
    token_type: String,
    #[allow(dead_code)]
    #[serde(default)]
    scope: String,
}

/// Pending / slow-down / error envelope. GitHub serves these at HTTP 200.
#[derive(Deserialize)]
struct PollErrorResp {
    error: String,
    #[allow(dead_code)]
    #[serde(default)]
    error_description: Option<String>,
}

/// Response from the copilot_internal/v2/token swap.
#[derive(Debug, Clone, Deserialize)]
pub struct CopilotTokenResp {
    pub token: String,
    pub expires_at: i64,
    #[serde(default)]
    pub refresh_in: Option<i64>,
}

/// Configuration for the login flow. Override the host for GitHub
/// Enterprise Server (`https://ghe.example.com`).
#[derive(Debug, Clone)]
pub struct LoginConfig {
    pub host: String,
    pub client_id: String,
}

impl Default for LoginConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            client_id: CLIENT_ID.to_string(),
        }
    }
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("building reqwest client for copilot auth")
}

/// Kick off the device flow: ask GitHub for a user code.
pub async fn request_device_code(cfg: &LoginConfig) -> Result<DeviceCode> {
    let url = format!("{}/login/device/code", cfg.host.trim_end_matches('/'));
    let body = format!(
        "client_id={}&scope={}",
        urlencoding::encode(&cfg.client_id),
        urlencoding::encode("read:user"),
    );
    let resp = client()?
        .post(&url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("device-code request failed: {status}: {body}");
    }
    let dc: DeviceCodeResp = resp
        .json()
        .await
        .context("parsing /login/device/code response")?;
    Ok(DeviceCode {
        verification_url: dc.verification_uri,
        user_code: dc.user_code,
        device_code: dc.device_code,
        interval: dc.interval.max(1),
    })
}

/// Poll GitHub until the user authorizes (or the 15-minute window
/// elapses). Returns the GitHub OAuth access token (`gho_...`).
pub async fn poll_for_token(cfg: &LoginConfig, device: &DeviceCode) -> Result<String> {
    let url = format!(
        "{}/login/oauth/access_token",
        cfg.host.trim_end_matches('/')
    );
    let started = Instant::now();
    let mut interval = device.interval;
    let client = client()?;
    loop {
        let body = format!(
            "client_id={}&device_code={}&grant_type={}",
            urlencoding::encode(&cfg.client_id),
            urlencoding::encode(&device.device_code),
            urlencoding::encode("urn:ietf:params:oauth:grant-type:device_code"),
        );
        let resp = client
            .post(&url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("token poll failed: {status}: {body}");
        }

        // GitHub returns both success and pending/slow_down/error at 200.
        // Read the body once, then try to parse it as either shape.
        let raw = resp.text().await.context("reading token poll body")?;
        if let Ok(tok) = serde_json::from_str::<AccessTokenResp>(&raw) {
            return Ok(tok.access_token);
        }
        let err: PollErrorResp = serde_json::from_str(&raw)
            .with_context(|| format!("parsing token poll response: {raw}"))?;
        match err.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => interval = interval.saturating_add(5),
            "expired_token"
            | "access_denied"
            | "unsupported_grant_type"
            | "incorrect_client_credentials"
            | "incorrect_device_code"
            | "device_flow_disabled" => {
                bail!("device-code login failed: {}", err.error);
            }
            other => bail!("device-code login failed: {other}"),
        }
        if started.elapsed() >= MAX_POLL_DURATION {
            bail!("device-code login timed out after 15 minutes");
        }
        let remaining = MAX_POLL_DURATION.saturating_sub(started.elapsed());
        let sleep = Duration::from_secs(interval).min(remaining);
        tokio::time::sleep(sleep).await;
    }
}

/// Swap a GitHub OAuth access token for a short-lived Copilot API key.
/// The returned `token` is what `api.githubcopilot.com` accepts on the
/// `Authorization: Bearer ...` header.
pub async fn fetch_copilot_token(gh_access_token: &str) -> Result<CopilotTokenResp> {
    let ver = editor_version();
    let resp = client()?
        .get(COPILOT_TOKEN_URL)
        .header("Authorization", format!("token {gh_access_token}"))
        .header("Accept", "application/json")
        .header("Editor-Version", &ver)
        .header("Editor-Plugin-Version", &ver)
        .header("User-Agent", &ver)
        .send()
        .await
        .with_context(|| format!("GET {COPILOT_TOKEN_URL}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED {
            bail!(
                "Copilot token swap rejected (401): {body}. \
                 The GitHub account may not have an active Copilot subscription."
            );
        }
        bail!("copilot token swap failed: {status}: {body}");
    }
    resp.json::<CopilotTokenResp>()
        .await
        .context("parsing /copilot_internal/v2/token response")
}

/// End-to-end interactive login flow. Prints the URL + user-code,
/// then polls, swaps, and persists tokens on success.
pub async fn run_interactive_login(cfg: &LoginConfig) -> Result<StoredTokens> {
    let device = request_device_code(cfg).await?;
    print_user_prompt(&device);
    complete_login(cfg, &device).await
}

/// Headless variant of the second half of the device flow. Given a
/// `DeviceCode` already obtained via [`request_device_code`], polls
/// GitHub, swaps for a Copilot token, and persists both. Used by the
/// TUI's `/settings` dialog, where the user sees the prompt in the
/// dialog itself rather than on stderr.
pub async fn complete_login(cfg: &LoginConfig, device: &DeviceCode) -> Result<StoredTokens> {
    let gh = poll_for_token(cfg, device).await?;
    let cop = fetch_copilot_token(&gh).await?;
    let expires_at = chrono::DateTime::<chrono::Utc>::from_timestamp(cop.expires_at, 0)
        .context("copilot token expires_at out of range")?;
    let stored = StoredTokens {
        gh_access_token: gh,
        copilot_token: cop.token,
        copilot_expires_at: expires_at,
        saved_at: chrono::Utc::now(),
    };
    persist(&stored)?;
    Ok(stored)
}

/// Write the token bundle into `credentials.json` under the `copilot` key.
pub fn persist(tokens: &StoredTokens) -> Result<()> {
    let mut store = CredentialStore::open_default()?;
    store.set("copilot", serde_json::to_value(tokens)?);
    store.save()
}

/// Load the stored token bundle, if any.
pub fn load() -> Result<Option<StoredTokens>> {
    let store = CredentialStore::open_default()?;
    match store.get("copilot") {
        None => Ok(None),
        Some(v) => Ok(Some(serde_json::from_value(v.clone()).context(
            "stored copilot credentials don't match expected shape — re-login",
        )?)),
    }
}

/// Remove the stored bundle.
pub fn logout() -> Result<bool> {
    let mut store = CredentialStore::open_default()?;
    if store.get("copilot").is_none() {
        return Ok(false);
    }
    store.remove("copilot");
    store.save()?;
    Ok(true)
}

fn print_user_prompt(device: &DeviceCode) {
    // Stick to plain text — this runs both from `cockpit providers
    // login copilot` (interactive terminal) and potentially from a
    // non-TTY wrapper; ANSI escapes everywhere would just be noise.
    eprintln!();
    eprintln!("GitHub Copilot device-code login");
    eprintln!();
    eprintln!("  1. Open this URL in a browser and sign in:");
    eprintln!("       {}", device.verification_url);
    eprintln!();
    eprintln!("  2. Enter this code (expires in 15 minutes):");
    eprintln!("       {}", device.user_code);
    eprintln!();
    eprintln!("Waiting for authorization…");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_deserializer_accepts_string_and_number() {
        let from_str: DeviceCodeResp = serde_json::from_str(
            r#"{"device_code":"d","user_code":"abc","verification_uri":"https://github.com/login/device","interval":"5"}"#,
        )
        .unwrap();
        assert_eq!(from_str.interval, 5);
        let from_num: DeviceCodeResp = serde_json::from_str(
            r#"{"device_code":"d","user_code":"abc","verification_uri":"https://github.com/login/device","interval":7}"#,
        )
        .unwrap();
        assert_eq!(from_num.interval, 7);
    }

    #[test]
    fn device_code_response_parses() {
        let raw = r#"{
            "device_code": "abc123",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5
        }"#;
        let dc: DeviceCodeResp = serde_json::from_str(raw).unwrap();
        assert_eq!(dc.device_code, "abc123");
        assert_eq!(dc.user_code, "WDJB-MJHT");
        assert_eq!(dc.verification_uri, "https://github.com/login/device");
        assert_eq!(dc.interval, 5);
    }

    #[test]
    fn access_token_response_parses() {
        let raw = r#"{
            "access_token": "gho_abc",
            "token_type": "bearer",
            "scope": "read:user"
        }"#;
        let tok: AccessTokenResp = serde_json::from_str(raw).unwrap();
        assert_eq!(tok.access_token, "gho_abc");
        assert_eq!(tok.token_type, "bearer");
        assert_eq!(tok.scope, "read:user");
    }

    #[test]
    fn poll_error_response_parses() {
        let raw = r#"{"error":"authorization_pending","error_description":"…"}"#;
        let e: PollErrorResp = serde_json::from_str(raw).unwrap();
        assert_eq!(e.error, "authorization_pending");
    }

    #[test]
    fn copilot_token_response_parses() {
        let raw = r#"{
            "token": "tid=abc;exp=123456",
            "expires_at": 1764140000,
            "refresh_in": 1500
        }"#;
        let r: CopilotTokenResp = serde_json::from_str(raw).unwrap();
        assert_eq!(r.token, "tid=abc;exp=123456");
        assert_eq!(r.expires_at, 1764140000);
        assert_eq!(r.refresh_in, Some(1500));
    }

    #[test]
    fn login_config_defaults_to_public_github() {
        let cfg = LoginConfig::default();
        assert_eq!(cfg.host, "https://github.com");
        assert_eq!(cfg.client_id, "Iv1.b507a08c87ecfe98");
    }

    #[test]
    fn editor_version_contains_crate_version() {
        let v = editor_version();
        assert!(v.starts_with("cockpit-cli/"));
    }
}
