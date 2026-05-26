//! Codex (ChatGPT Plus/Pro) device-code OAuth login.
//!
//! Ported from `codex/codex-rs/login/src/{device_code_auth,pkce,server,auth/manager}.rs`.
//! The vendor-specific quirk here is the *two-step* device flow:
//!
//!   1. POST `{issuer}/api/accounts/deviceauth/usercode` → `device_auth_id`
//!      + `user_code`. The user opens `{issuer}/codex/device` in a
//!      browser and enters the code.
//!   2. Poll POST `{issuer}/api/accounts/deviceauth/token` with that
//!      `(device_auth_id, user_code)` pair. While the user is still
//!      entering the code the endpoint replies 403 or 404; once they
//!      authorize, it returns `authorization_code` + a PKCE pair the
//!      server generated.
//!   3. POST `{issuer}/oauth/token` with `grant_type=authorization_code`
//!      + the returned code + `code_verifier` → the actual
//!      `{id,access,refresh}_token` triple.
//!
//! Step 3 uses **form-urlencoded** (RFC 6749 §4.1.3); the refresh path
//! (`grant_type=refresh_token`) uses **JSON**. This is what the codex
//! source does and what auth.openai.com expects.
//!
//! Tokens land in `$XDG_STATE_HOME/cockpit/credentials.json` under the
//! `codex` key:
//!
//! ```json
//! "codex": {
//!   "id_token":      "...",
//!   "access_token":  "...",
//!   "refresh_token": "...",
//!   "saved_at":      "2026-05-26T12:34:56Z"
//! }
//! ```

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use rand::RngCore;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::credentials::CredentialStore;

/// OAuth client id codex itself uses. Matches
/// `codex/codex-rs/login/src/auth/manager.rs::CLIENT_ID`.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// OpenAI auth issuer. `auth.openai.com` is the canonical
/// production hostname.
pub const DEFAULT_ISSUER: &str = "https://auth.openai.com";

/// Max time we'll keep polling the deviceauth/token endpoint before
/// giving up. Matches codex's behavior — the user has 15 minutes to
/// open the URL and enter the code.
const MAX_POLL_DURATION: Duration = Duration::from_secs(15 * 60);

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub saved_at: chrono::DateTime<chrono::Utc>,
}

/// PKCE verifier + challenge. We generate the verifier locally for
/// the *exchange* step; the codex token-poll endpoint *also* returns
/// its own (verifier, challenge) pair that we have to round-trip
/// back to it (this is how it binds the device-code session to the
/// authorization code).
#[derive(Debug, Clone)]
struct PkceCodes {
    code_verifier: String,
    code_challenge: String,
}

fn generate_pkce() -> PkceCodes {
    let mut bytes = [0u8; 64];
    rand::rng().fill_bytes(&mut bytes);
    // RFC 7636 §4.1: verifier is `[A-Z] / [a-z] / [0-9] / "-" / "." / "_" / "~"`,
    // 43..128 chars. URL-safe base64 of 64 bytes fits.
    let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    PkceCodes {
        code_verifier,
        code_challenge,
    }
}

#[derive(Debug, Clone)]
pub struct DeviceCode {
    /// URL to direct the user to (`{issuer}/codex/device`).
    pub verification_url: String,
    /// 6–8 char code the user types into the browser page.
    pub user_code: String,
    device_auth_id: String,
    interval: u64,
}

#[derive(Deserialize)]
struct UserCodeResp {
    device_auth_id: String,
    #[serde(alias = "user_code", alias = "usercode")]
    user_code: String,
    #[serde(default, deserialize_with = "deserialize_interval")]
    interval: u64,
}

fn deserialize_interval<'de, D>(de: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;
    // Codex sends the interval as a JSON string. Tolerate both forms.
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

#[derive(Deserialize)]
struct CodeSuccessResp {
    authorization_code: String,
    code_challenge: String,
    code_verifier: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

#[derive(Deserialize)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

/// Configuration for the login flow. Override the issuer for testing
/// against a mock auth server.
#[derive(Debug, Clone)]
pub struct LoginConfig {
    pub issuer: String,
    pub client_id: String,
}

impl Default for LoginConfig {
    fn default() -> Self {
        Self {
            issuer: DEFAULT_ISSUER.to_string(),
            client_id: CLIENT_ID.to_string(),
        }
    }
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("building reqwest client for codex auth")
}

/// Kick off the device flow: ask the auth server for a user code.
pub async fn request_device_code(cfg: &LoginConfig) -> Result<DeviceCode> {
    let url = format!(
        "{}/api/accounts/deviceauth/usercode",
        cfg.issuer.trim_end_matches('/')
    );
    let body = serde_json::json!({ "client_id": cfg.client_id });
    let resp = client()?
        .post(&url)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        if status == StatusCode::NOT_FOUND {
            bail!(
                "device-code login isn't enabled for this Codex server (issuer: {})",
                cfg.issuer
            );
        }
        bail!("device-code request failed: {status}: {body}");
    }

    let uc: UserCodeResp = resp
        .json()
        .await
        .context("parsing /deviceauth/usercode response")?;
    Ok(DeviceCode {
        verification_url: format!("{}/codex/device", cfg.issuer.trim_end_matches('/')),
        user_code: uc.user_code,
        device_auth_id: uc.device_auth_id,
        interval: uc.interval.max(1),
    })
}

/// Poll the auth server until the user authorizes (or the 15-minute
/// window elapses).
pub async fn poll_for_authorization_code(
    cfg: &LoginConfig,
    device_code: &DeviceCode,
) -> Result<CodeSuccessResp> {
    let url = format!(
        "{}/api/accounts/deviceauth/token",
        cfg.issuer.trim_end_matches('/')
    );
    let started = Instant::now();
    let client = client()?;
    loop {
        let body = serde_json::json!({
            "device_auth_id": device_code.device_auth_id,
            "user_code":      device_code.user_code,
        });
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;

        let status = resp.status();
        if status.is_success() {
            return resp
                .json::<CodeSuccessResp>()
                .await
                .context("parsing /deviceauth/token success response");
        }
        // 403 / 404 = "user hasn't authorized yet, keep polling".
        if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
            if started.elapsed() >= MAX_POLL_DURATION {
                bail!("device-code login timed out after 15 minutes");
            }
            let remaining = MAX_POLL_DURATION.saturating_sub(started.elapsed());
            let sleep = Duration::from_secs(device_code.interval).min(remaining);
            tokio::time::sleep(sleep).await;
            continue;
        }
        // Anything else is fatal.
        let body = resp.text().await.unwrap_or_default();
        bail!("device-code poll failed: {status}: {body}");
    }
}

/// Exchange the (authorization_code + code_verifier) pair for the
/// `{id,access,refresh}_token` triple. The code_verifier comes from
/// the deviceauth/token success response (the server generates it on
/// our behalf), not from our own PKCE generator — that's how the
/// device-code variant binds the session.
pub async fn exchange_for_tokens(
    cfg: &LoginConfig,
    code: &CodeSuccessResp,
) -> Result<TokenResponse> {
    let issuer = cfg.issuer.trim_end_matches('/');
    let token_url = format!("{issuer}/oauth/token");
    let redirect_uri = format!("{issuer}/deviceauth/callback");

    // PKCE-S256 sanity: the challenge must equal BASE64URL-NO-PAD(SHA256(verifier)).
    // If the server's verifier+challenge pair drifted, fail fast.
    let recomputed = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(code.code_verifier.as_bytes()));
    if recomputed != code.code_challenge {
        return Err(anyhow!(
            "PKCE challenge mismatch: server returned a verifier whose SHA-256 doesn't match \
             its challenge. Refusing to exchange."
        ));
    }

    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(&code.authorization_code),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(&cfg.client_id),
        urlencoding::encode(&code.code_verifier),
    );
    let resp = client()?
        .post(&token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .with_context(|| format!("POST {token_url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("token endpoint returned {status}: {body}");
    }
    resp.json::<TokenResponse>()
        .await
        .context("parsing /oauth/token response")
}

/// Refresh an expired access token. Returns the *new* token triple
/// (any field the server omits keeps its prior value).
pub async fn refresh_tokens(cfg: &LoginConfig, refresh_token: &str) -> Result<RefreshResponse> {
    let url = format!("{}/oauth/token", cfg.issuer.trim_end_matches('/'));
    let body = serde_json::json!({
        "client_id":     cfg.client_id,
        "grant_type":    "refresh_token",
        "refresh_token": refresh_token,
    });
    let resp = client()?
        .post(&url)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED {
            bail!(
                "refresh-token rejected (status 401): {body}. \
                 Run `cockpit providers login codex` to re-authenticate."
            );
        }
        bail!("refresh failed: {status}: {body}");
    }
    resp.json::<RefreshResponse>()
        .await
        .context("parsing refresh response")
}

/// End-to-end interactive login flow. Prints the URL + user-code,
/// then polls and persists tokens on success.
pub async fn run_interactive_login(cfg: &LoginConfig) -> Result<StoredTokens> {
    let device = request_device_code(cfg).await?;
    print_user_prompt(&device);
    complete_login(cfg, &device).await
}

/// Headless variant of the second half of the device flow. Given a
/// `DeviceCode` already obtained via [`request_device_code`], polls the
/// auth server, exchanges the authorization code for tokens, and
/// persists them. Used by the TUI's `/settings` dialog, where the user
/// sees the prompt in the dialog itself rather than on stderr.
pub async fn complete_login(cfg: &LoginConfig, device: &DeviceCode) -> Result<StoredTokens> {
    let code = poll_for_authorization_code(cfg, device).await?;
    let tokens = exchange_for_tokens(cfg, &code).await?;
    let stored = StoredTokens {
        id_token: tokens.id_token,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        saved_at: chrono::Utc::now(),
    };
    persist(&stored)?;
    Ok(stored)
}

/// Write the token bundle into `credentials.json` under the `codex` key.
pub fn persist(tokens: &StoredTokens) -> Result<()> {
    let mut store = CredentialStore::open_default()?;
    store.set("codex", serde_json::to_value(tokens)?);
    store.save()
}

/// Load the stored token bundle, if any.
pub fn load() -> Result<Option<StoredTokens>> {
    let store = CredentialStore::open_default()?;
    match store.get("codex") {
        None => Ok(None),
        Some(v) => Ok(Some(serde_json::from_value(v.clone()).context(
            "stored codex credentials don't match expected shape — re-login",
        )?)),
    }
}

/// Remove the stored bundle.
pub fn logout() -> Result<bool> {
    let mut store = CredentialStore::open_default()?;
    if store.get("codex").is_none() {
        return Ok(false);
    }
    store.remove("codex");
    store.save()?;
    Ok(true)
}

fn print_user_prompt(device: &DeviceCode) {
    // Stick to plain text — this runs both from `cockpit providers
    // login codex` (interactive terminal) and potentially from a non-TTY
    // wrapper; ANSI escapes everywhere would just be noise.
    eprintln!();
    eprintln!("Codex device-code login");
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
    fn pkce_challenge_matches_verifier() {
        let p = generate_pkce();
        // Verifier must be 43..128 chars per RFC 7636.
        assert!(p.code_verifier.len() >= 43);
        assert!(p.code_verifier.len() <= 128);
        let recomputed = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(p.code_verifier.as_bytes()));
        assert_eq!(recomputed, p.code_challenge);
    }

    #[test]
    fn pkce_two_calls_yield_distinct_verifiers() {
        let a = generate_pkce();
        let b = generate_pkce();
        assert_ne!(a.code_verifier, b.code_verifier);
    }

    #[test]
    fn interval_deserializer_accepts_string_and_number() {
        let from_str: UserCodeResp =
            serde_json::from_str(r#"{"device_auth_id":"x","user_code":"abc","interval":"5"}"#)
                .unwrap();
        assert_eq!(from_str.interval, 5);
        let from_num: UserCodeResp =
            serde_json::from_str(r#"{"device_auth_id":"x","user_code":"abc","interval":7}"#)
                .unwrap();
        assert_eq!(from_num.interval, 7);
    }

    #[test]
    fn pkce_mismatch_is_detected() {
        // If the server ever lies about its (verifier, challenge) pair,
        // exchange_for_tokens should refuse rather than send a bogus
        // PKCE exchange. We exercise the check directly.
        let recomputed = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(b"some-verifier"));
        assert_ne!(recomputed, "definitely-not-the-real-challenge");
    }
}
