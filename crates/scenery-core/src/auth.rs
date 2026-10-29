use crate::secret_file;
use crate::state::AppState;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info};

// ── Wire types ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default = "default_token_type")]
    pub token_type: String,
}

fn default_token_type() -> String {
    "bearer".to_string()
}

#[derive(Debug, Serialize)]
struct RefreshRequest {
    refresh_token: String,
}

/// Outcome of a token refresh attempt. Callers should treat `AuthFailed` as
/// terminal-until-reauth (drive an exponential backoff and surface a "needs
/// re-auth" state) but treat `Transport` and `ServerError` as transient and
/// retry on the normal interval.
#[derive(Debug)]
pub enum RefreshError {
    /// Server rejected the refresh token (401/403). The refresh token is
    /// almost certainly dead — back off and tell the user to re-login.
    AuthFailed { status: u16, body: String },
    /// 5xx or other non-auth HTTP failure — retry on the normal cadence.
    ServerError { status: u16, body: String },
    /// Could not reach the server (network down, DNS, TLS, etc.).
    Transport { source: anyhow::Error },
    /// Got a 2xx but the body wasn't a valid TokenResponse.
    Decode(anyhow::Error),
}

impl RefreshError {
    /// True when the refresh token is dead and a backoff should be applied.
    pub fn is_auth_failure(&self) -> bool {
        matches!(self, RefreshError::AuthFailed { .. })
    }
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshError::AuthFailed { status, body } => {
                write!(
                    f,
                    "token refresh rejected by server (HTTP {status}): {body}"
                )
            }
            RefreshError::ServerError { status, body } => {
                write!(f, "token refresh server error (HTTP {status}): {body}")
            }
            RefreshError::Transport { source } => write!(f, "{source:#}"),
            RefreshError::Decode(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for RefreshError {}

#[derive(Debug, Deserialize)]
pub struct UserInfo {
    pub id: String,
    pub email: String,
    pub display_name: String,
}

// ── OAuth flow types ─────────────────────────────────────────────────────────

/// The information needed to drive a browser-based OAuth login.
///
/// The caller opens `auth_url` in the user's browser. The daemon's OAuth
/// callback listener receives the redirect and the CLI exchanges the code.
#[derive(Debug)]
pub struct OAuthInitiation {
    /// URL to open in the user's browser.
    pub auth_url: String,
    /// PKCE code verifier — stored by the caller until the callback arrives.
    /// 32 random bytes encoded as base64url-no-padding (43 chars).
    pub code_verifier: String,
    /// PKCE code challenge — SHA-256(code_verifier) encoded as base64url-no-padding.
    pub code_challenge: String,
    /// State parameter for CSRF protection.
    pub state: String,
}

// ── JWT claims (minimal — only exp is read) ──────────────────────────────────

/// Minimal JWT claims struct used for expiry checking.
/// Only the `exp` claim is decoded; other fields are ignored.
#[derive(Debug, Deserialize)]
struct JwtClaims {
    exp: u64,
}

// ── Stored tokens ────────────────────────────────────────────────────────────

pub struct StoredTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub registry_url: String,
    /// Consecutive 401/403 responses from /portal/auth/refresh. Resets to 0
    /// on a successful refresh.
    pub refresh_failures: u32,
    /// Unix timestamp (seconds) before which the daemon will not attempt
    /// another refresh. NULL/0 means "no backoff in effect".
    pub next_refresh_attempt_at: Option<i64>,
    /// Most recent refresh status: "ok", "auth_failed", "server_error",
    /// "transport_error". Used by the daemon's status surface so the CLI
    /// can show "needs re-auth" instead of "active" when appropriate.
    pub last_refresh_status: Option<String>,
}

// ── PKCE helpers ─────────────────────────────────────────────────────────────

/// Generate a PKCE code verifier: 32 cryptographically random bytes encoded
/// as base64url-no-padding, yielding exactly 43 characters.
///
/// Per RFC 7636 §4.1, verifiers must be 43–128 characters from the
/// unreserved character set `[A-Z a-z 0-9 - . _ ~]`. Base64url-no-padding
/// over 32 bytes satisfies this requirement.
fn generate_code_verifier() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS RNG must be available");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Derive the PKCE code challenge from a verifier.
///
/// Per RFC 7636 §4.2, `code_challenge = BASE64URL(SHA256(ASCII(code_verifier)))`.
pub fn derive_code_challenge(code_verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    URL_SAFE_NO_PAD.encode(digest)
}

// ── AuthClient ───────────────────────────────────────────────────────────────

pub struct AuthClient {
    base_url: String,
    http: reqwest::blocking::Client,
}

impl AuthClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client should build"),
        }
    }

    /// Initiate an OAuth browser-based login flow with a specific redirect URI.
    ///
    /// Generates a proper PKCE code_verifier (32 random bytes, base64url encoded)
    /// and derives the code_challenge. Builds the authorization URL targeting
    /// `/portal/auth/cli/authorize`.
    pub fn initiate_oauth_flow_with_redirect(&self, redirect_uri: &str) -> Result<OAuthInitiation> {
        let state = uuid::Uuid::new_v4().to_string();
        let code_verifier = generate_code_verifier();
        let code_challenge = derive_code_challenge(&code_verifier);

        let base = self.base_url.trim_end_matches('/');
        let encoded_redirect = urlencoding::encode(redirect_uri);
        let auth_url = format!(
            "{base}/portal/auth/cli/authorize\
             ?response_type=code\
             &client_id=runner\
             &redirect_uri={encoded_redirect}\
             &code_challenge={code_challenge}\
             &code_challenge_method=S256\
             &state={state}"
        );

        debug!(auth_url, "OAuth PKCE flow initiated — open URL in browser");

        Ok(OAuthInitiation {
            auth_url,
            code_verifier,
            code_challenge,
            state,
        })
    }

    /// Initiate an OAuth browser-based login flow (legacy — no redirect URI).
    ///
    /// Kept for backwards compatibility with tests that do not need a
    /// daemon-provided redirect URI. Builds a URL targeting the CLI authorize
    /// endpoint with PKCE but without a `redirect_uri` parameter.
    pub fn initiate_oauth_flow(&self) -> Result<OAuthInitiation> {
        let state = uuid::Uuid::new_v4().to_string();
        let code_verifier = generate_code_verifier();
        let code_challenge = derive_code_challenge(&code_verifier);

        let base = self.base_url.trim_end_matches('/');
        let auth_url = format!(
            "{base}/portal/auth/cli/authorize\
             ?response_type=code\
             &client_id=runner\
             &code_challenge={code_challenge}\
             &code_challenge_method=S256\
             &state={state}"
        );

        debug!(
            auth_url,
            "OAuth flow initiated (no redirect_uri) — open URL in browser"
        );

        Ok(OAuthInitiation {
            auth_url,
            code_verifier,
            code_challenge,
            state,
        })
    }

    /// Exchange an OAuth authorization code for tokens via PKCE.
    ///
    /// POSTs form-urlencoded to `/portal/auth/cli/token` with the code,
    /// code_verifier, grant_type, and client_id.
    pub fn exchange_oauth_code(&self, code: &str, code_verifier: &str) -> Result<TokenResponse> {
        let url = format!(
            "{}/portal/auth/cli/token",
            self.base_url.trim_end_matches('/')
        );
        debug!(url, "exchanging OAuth code for tokens via PKCE");

        let resp = self
            .http
            .post(&url)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("code_verifier", code_verifier),
                ("client_id", "runner"),
            ])
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().unwrap_or_default();
            anyhow::bail!("OAuth token exchange failed (HTTP {status}): {body_text}");
        }

        resp.json()
            .context("failed to deserialize OAuth token response")
    }

    /// Refresh an access token using a refresh token.
    pub fn refresh(&self, refresh_token: &str) -> Result<TokenResponse> {
        match self.refresh_detailed(refresh_token) {
            Ok(t) => Ok(t),
            Err(RefreshError::AuthFailed { status, body }) => {
                anyhow::bail!("token refresh failed (HTTP {status}): {body}")
            }
            Err(RefreshError::Transport { source }) => Err(source),
            Err(RefreshError::ServerError { status, body }) => {
                anyhow::bail!("token refresh failed (HTTP {status}): {body}")
            }
            Err(RefreshError::Decode(e)) => Err(e),
        }
    }

    /// Refresh an access token, distinguishing auth failures (401/403) from
    /// transport / server errors. Callers use this to drive backoff: auth
    /// failures escalate the retry interval because the refresh token is
    /// almost certainly dead until the user re-authenticates; transport
    /// failures retry on the normal cadence.
    pub fn refresh_detailed(
        &self,
        refresh_token: &str,
    ) -> std::result::Result<TokenResponse, RefreshError> {
        let url = format!(
            "{}/portal/auth/refresh",
            self.base_url.trim_end_matches('/')
        );
        debug!(url, "refreshing token");

        let resp = self
            .http
            .post(&url)
            .json(&RefreshRequest {
                refresh_token: refresh_token.to_string(),
            })
            .send()
            .map_err(|e| RefreshError::Transport {
                source: anyhow::Error::new(e).context(format!("failed to reach registry at {url}")),
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(RefreshError::AuthFailed {
                    status: status.as_u16(),
                    body,
                });
            }
            return Err(RefreshError::ServerError {
                status: status.as_u16(),
                body,
            });
        }

        resp.json().map_err(|e| {
            RefreshError::Decode(
                anyhow::Error::new(e).context("failed to deserialize refresh response"),
            )
        })
    }

    /// Fetch the currently authenticated user's info.
    pub fn me(&self, access_token: &str) -> Result<UserInfo> {
        let url = format!("{}/portal/auth/me", self.base_url.trim_end_matches('/'));

        let resp = self
            .http
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("auth check failed (HTTP {status}): {body}");
        }

        resp.json().context("failed to deserialize user info")
    }

    /// Revoke the sign-in session `refresh_token` belongs to, server side:
    /// `POST /portal/auth/revoke`. `scope` is `"family"` (this session) or
    /// `"device"` (every session bound to its device). The backend answers
    /// 200 even for a dead token; any failure here is the caller's to treat
    /// as best-effort, since the local tokens are cleared regardless.
    pub fn revoke(&self, refresh_token: &str, scope: &str) -> Result<()> {
        let url = format!("{}/portal/auth/revoke", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "refresh_token": refresh_token, "scope": scope }))
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("session revoke failed (HTTP {status}): {body}");
        }
        Ok(())
    }

    /// Offboard this device: `POST /api/devices/{device_id}/deprovision`.
    /// Revokes the device's sessions and brokered gateway grants and records
    /// the offboarding in IT's audit log, keeping the device row (unlike
    /// `DELETE /devices/{id}`). Repeat calls succeed.
    pub fn deprovision_device(&self, access_token: &str, device_id: &str) -> Result<()> {
        let url = format!(
            "{}/api/devices/{device_id}/deprovision",
            self.base_url.trim_end_matches('/')
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(access_token)
            .json(&serde_json::json!({ "reason": "vectorhawk uninstall" }))
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("device deprovision failed (HTTP {status}): {body}");
        }
        Ok(())
    }

    /// Determine if the access token needs to be refreshed.
    ///
    /// Decodes the JWT payload without signature validation (we trust the
    /// daemon-stored token) and returns `true` if `exp - now() < 300s`.
    ///
    /// On any parse failure (malformed token, missing exp claim, clock error),
    /// returns `true` conservatively — it is safer to attempt a refresh than
    /// to use a potentially-expired token.
    pub fn needs_refresh(access_token: &str) -> bool {
        // PATs are long-lived credentials revoked via the portal — never expire via JWT mechanism.
        if access_token.starts_with("vh_pat_") {
            return false;
        }
        match decode_jwt_exp(access_token) {
            Some(exp) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                exp.saturating_sub(now) < 300
            }
            None => true,
        }
    }
}

/// Decode the raw JSON payload (second segment) of a JWT without verifying
/// the signature. Returns `None` on any parse error. The JWT is assumed to
/// be three segments separated by `.`; the payload segment may be
/// base64url-no-padding or standard-base64-padded.
///
/// This is intentionally unauthenticated: callers only ever use it to read
/// claims out of a token *this daemon itself already holds* (its own stored
/// portal access token), never to trust claims from an untrusted caller.
/// Untrusted bearer tokens (the MCP HTTP resource-server path) go through
/// full ES256/JWKS verification instead — see `scenery-daemon::mcp_http`.
fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = token.splitn(3, '.').collect();
    if parts.len() != 3 {
        return None;
    }
    // JWT payload may use standard base64 padding or no-padding.
    // Try URL_SAFE_NO_PAD first, then fall back to standard.
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(parts[1]))
        .ok()?;
    serde_json::from_slice(&payload_bytes).ok()
}

/// Decode the `exp` claim from a JWT without verifying the signature.
fn decode_jwt_exp(token: &str) -> Option<u64> {
    let claims: JwtClaims = serde_json::from_value(decode_jwt_payload(token)?).ok()?;
    Some(claims.exp)
}

/// Decode the `sub` claim from a JWT without verifying the signature.
///
/// Used only to read the user id out of this daemon's own stored portal
/// access token (an HS256 JWT this daemon trusts because it is the one that
/// saved it via [`save_tokens`]) — not for validating untrusted input.
pub fn decode_jwt_sub(token: &str) -> Option<String> {
    decode_jwt_payload(token)?
        .get("sub")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

// ── Token storage ────────────────────────────────────────────────────────────
//
// Token *secrets* (access_token, refresh_token — a PAT is stored as the
// access token) live in the OS keychain when one is usable (macOS Keychain,
// Windows Credential Manager) and otherwise in an owner-only file in the
// secrets store (`crate::secret_file`): always on Linux, and on headless or
// keychain-disabled macOS. Never in `state.db` — older builds kept them in
// the `auth_tokens` columns, and `move_legacy_secrets` moves them out.
//
// Backoff state (`refresh_failures`, `next_refresh_attempt_at`,
// `last_refresh_status`) ALWAYS lives in SQLite. It's not secret, it's
// structured, and the keychain isn't a great fit for non-secret metadata.
// We always insert a backoff-state row, with the secret columns empty, so
// `load_all_tokens` has something to iterate over.

#[cfg(any(target_os = "macos", target_os = "windows"))]
const KEYRING_SERVICE: &str = "com.vectorhawk.agent";

/// Stable account name for a given registry URL inside the OS keychain.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn keyring_account(registry_url: &str) -> String {
    // The OS keychain is keyed by (service, account). We pick a stable,
    // human-readable account string so users can find the entry if they
    // ever inspect their keychain.
    format!("registry::{registry_url}")
}

/// Escape hatch — when `VECTORHAWK_DISABLE_KEYCHAIN=1` is set the OS keychain
/// is treated as if it were unavailable, forcing the secrets-file fallback. Used
/// by the test suite (so tests don't pollute the real macOS Keychain) and
/// by users on shared boxes who prefer to keep tokens out of system stores.
/// Public so `vectorhawk uninstall`'s keychain sweep honours it too.
pub fn keychain_disabled() -> bool {
    std::env::var("VECTORHAWK_DISABLE_KEYCHAIN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[derive(Serialize, Deserialize)]
struct KeychainBlob<'a> {
    access_token: &'a str,
    refresh_token: &'a str,
}

#[derive(Serialize, Deserialize)]
struct OwnedKeychainBlob {
    access_token: String,
    refresh_token: String,
}

/// Result of probing the OS keychain for an entry.
//
// On Linux the `keyring` crate is not linked and `keychain_get` always returns
// `Unavailable`, so `Found`/`NotFound` are never constructed there. Scope the
// dead-code allowance to non-macOS/Windows so those platforms stay strict.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
enum KeychainProbe {
    /// Keychain returned a value successfully.
    Found(OwnedKeychainBlob),
    /// Keychain is available but has no entry for this registry.
    NotFound,
    /// Keychain is unavailable on this system (headless, no D-Bus, etc.).
    Unavailable,
}

// On macOS and Windows we link the `keyring` crate and use the OS-native
// keystore. On Linux the crate is intentionally not pulled in (would force
// a libdbus dep on the binary) — every call below returns Unavailable so
// the secrets file is used: 0600 in a 0700 directory, which matches what
// `gh`, `gcloud`, `kubectl` and Claude Code do on Linux.

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn keychain_get(registry_url: &str) -> KeychainProbe {
    if keychain_disabled() {
        return KeychainProbe::Unavailable;
    }
    let entry = match keyring::Entry::new(KEYRING_SERVICE, &keyring_account(registry_url)) {
        Ok(e) => e,
        Err(e) => {
            debug!(error = %e, "keychain entry constructor failed — treating as unavailable");
            return KeychainProbe::Unavailable;
        }
    };
    match entry.get_password() {
        Ok(json) => match serde_json::from_str::<OwnedKeychainBlob>(&json) {
            Ok(blob) => KeychainProbe::Found(blob),
            Err(e) => {
                debug!(error = %e, "keychain entry present but not JSON we wrote — treating as not found");
                KeychainProbe::NotFound
            }
        },
        Err(keyring::Error::NoEntry) => KeychainProbe::NotFound,
        Err(e) => {
            debug!(error = %e, "keychain get_password failed — falling back to the secrets file");
            KeychainProbe::Unavailable
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn keychain_get(_registry_url: &str) -> KeychainProbe {
    KeychainProbe::Unavailable
}

/// Try to write the secrets to the OS keychain. Returns Ok(true) on success,
/// Ok(false) when the keychain isn't usable (caller should fall back to
/// the secrets file), or Err on a programming error.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn keychain_set(registry_url: &str, access_token: &str, refresh_token: &str) -> Result<bool> {
    if keychain_disabled() {
        return Ok(false);
    }
    let entry = match keyring::Entry::new(KEYRING_SERVICE, &keyring_account(registry_url)) {
        Ok(e) => e,
        Err(e) => {
            debug!(error = %e, "keychain entry constructor failed; will store in the secrets file");
            return Ok(false);
        }
    };
    let blob = serde_json::to_string(&KeychainBlob {
        access_token,
        refresh_token,
    })
    .context("failed to serialize keychain blob")?;
    match entry.set_password(&blob) {
        Ok(()) => Ok(true),
        Err(e) => {
            debug!(error = %e, "keychain set_password failed; will store in the secrets file");
            Ok(false)
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn keychain_set(_registry_url: &str, _access_token: &str, _refresh_token: &str) -> Result<bool> {
    Ok(false)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn keychain_delete(registry_url: &str) {
    if keychain_disabled() {
        return;
    }
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &keyring_account(registry_url)) {
        let _ = entry.delete_credential();
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn keychain_delete(_registry_url: &str) {}

/// Secrets-store file name for a registry's tokens. Hashed, so any URL makes
/// a safe file name.
fn token_secret_name(registry_url: &str) -> String {
    let digest = hex::encode(Sha256::digest(registry_url.as_bytes()));
    format!("token-{}.json", &digest[..32])
}

/// A registry's secrets: the keychain's copy when it has one, else the
/// secrets file's.
fn secrets_get(state: &AppState, registry_url: &str) -> Result<Option<OwnedKeychainBlob>> {
    if let KeychainProbe::Found(blob) = keychain_get(registry_url) {
        return Ok(Some(blob));
    }
    let json = secret_file::read(
        state.root_dir.as_std_path(),
        &token_secret_name(registry_url),
    )
    .context("failed to read auth tokens from the secrets store")?;
    json.map(|j| serde_json::from_str(&j).context("auth tokens file is not valid JSON"))
        .transpose()
}

/// Store a registry's secrets in the keychain, or the secrets file when the
/// keychain isn't usable.
fn secrets_set(
    state: &AppState,
    registry_url: &str,
    access_token: &str,
    refresh_token: &str,
) -> Result<()> {
    let root = state.root_dir.as_std_path();
    let name = token_secret_name(registry_url);
    if keychain_set(registry_url, access_token, refresh_token)? {
        // A file copy from a time the keychain was unusable is now stale.
        let _ = secret_file::remove(root, &name);
        return Ok(());
    }
    let blob = serde_json::to_string(&KeychainBlob {
        access_token,
        refresh_token,
    })
    .context("failed to serialize auth tokens")?;
    secret_file::write(root, &name, &blob)
        .context("failed to write auth tokens to the secrets store")
}

/// Write secrets to the keychain (when available) or the secrets file, and
/// always update the SQLite backoff-state row so callers can iterate.
///
/// On a successful save the backoff counters reset — the new tokens are
/// presumed good until proven otherwise.
pub fn save_tokens(
    state: &AppState,
    registry_url: &str,
    access_token: &str,
    refresh_token: &str,
) -> Result<()> {
    secrets_set(state, registry_url, access_token, refresh_token)?;

    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    conn.execute(
        "INSERT INTO auth_tokens (
             registry_url, access_token, refresh_token,
             refresh_failures, next_refresh_attempt_at, last_refresh_status
         )
         VALUES (?1, '', '', 0, NULL, 'ok')
         ON CONFLICT(registry_url) DO UPDATE SET
             access_token = '',
             refresh_token = '',
             refresh_failures = 0,
             next_refresh_attempt_at = NULL,
             last_refresh_status = 'ok',
             saved_at = CURRENT_TIMESTAMP",
        params![registry_url],
    )
    .context("failed to save auth tokens")?;
    Ok(())
}

/// Record a refresh attempt's outcome on the auth_tokens row. Used by the
/// daemon refresh loop to drive backoff.
pub fn record_refresh_failure(
    state: &AppState,
    registry_url: &str,
    status: &str,
    next_attempt_at: Option<i64>,
) -> Result<()> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    conn.execute(
        "UPDATE auth_tokens
         SET refresh_failures = refresh_failures + 1,
             next_refresh_attempt_at = ?1,
             last_refresh_status = ?2
         WHERE registry_url = ?3",
        params![next_attempt_at, status, registry_url],
    )
    .context("failed to record refresh failure")?;
    Ok(())
}

/// Row shape returned by the SQLite half of `load_tokens`.
/// `(legacy access_token, legacy refresh_token, refresh_failures,
///   next_refresh_attempt_at, last_refresh_status)`. The token columns are
/// empty except on a database `move_legacy_secrets` hasn't processed yet.
type SqliteAuthRow = (String, String, u32, Option<i64>, Option<String>);

/// Load tokens for a single registry: secrets from the keychain or secrets
/// file, backoff state from SQLite.
pub fn load_tokens(state: &AppState, registry_url: &str) -> Result<Option<StoredTokens>> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    let sqlite_row: Option<SqliteAuthRow> = conn
        .query_row(
            "SELECT access_token, refresh_token,
                    COALESCE(refresh_failures, 0),
                    next_refresh_attempt_at,
                    last_refresh_status
             FROM auth_tokens WHERE registry_url = ?1",
            [registry_url],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get::<_, i64>(2)? as u32,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    drop(conn);

    let (access_token, refresh_token) = match secrets_get(state, registry_url)? {
        Some(blob) => (blob.access_token, blob.refresh_token),
        // Not moved out of state.db yet (the move retries on every start).
        None => match &sqlite_row {
            Some(row) if !row.0.is_empty() && !row.1.is_empty() => (row.0.clone(), row.1.clone()),
            _ => return Ok(None),
        },
    };

    let meta = sqlite_row.unwrap_or((String::new(), String::new(), 0, None, None));
    Ok(Some(StoredTokens {
        access_token,
        refresh_token,
        registry_url: registry_url.to_string(),
        refresh_failures: meta.2,
        next_refresh_attempt_at: meta.3,
        last_refresh_status: meta.4,
    }))
}

/// Move every secret older builds kept in `state.db` out of it: auth tokens
/// to the keychain or secrets file, the H3 local key to the secrets file.
/// Then blank the columns and VACUUM, so no copy survives on a free page —
/// every token refresh used to rewrite the row and leave the previous tokens
/// behind. Idempotent; `AppState::bootstrap_in` runs it until it succeeds
/// once.
pub fn move_legacy_secrets(state: &AppState) -> Result<()> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    let mut stmt = conn.prepare(
        "SELECT registry_url, access_token, refresh_token FROM auth_tokens
         WHERE access_token != '' AND refresh_token != ''",
    )?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);

    for (registry_url, access_token, refresh_token) in rows {
        if secrets_get(state, &registry_url)?.is_none() {
            secrets_set(state, &registry_url, &access_token, &refresh_token)?;
        }
        info!(registry_url, "moved auth tokens out of state.db");
    }
    state.move_legacy_local_mcp_key()?;

    conn.execute(
        "UPDATE auth_tokens SET access_token = '', refresh_token = ''
         WHERE access_token != '' OR refresh_token != ''",
        [],
    )
    .context("failed to blank legacy token columns")?;
    conn.execute_batch("VACUUM")
        .context("failed to vacuum state.db")?;
    Ok(())
}

/// Load all stored token rows. Used by the daemon refresh loop to check
/// every registry for near-expiry tokens. Walks the SQLite backoff-state
/// table and resolves each row's secrets via `load_tokens`.
pub fn load_all_tokens(state: &AppState) -> Result<Vec<StoredTokens>> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    let mut stmt = conn
        .prepare("SELECT registry_url FROM auth_tokens")
        .context("failed to prepare load_all_tokens query")?;
    let urls: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("failed to query auth_tokens")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to collect auth_token rows")?;
    drop(stmt);
    drop(conn);

    let mut tokens = Vec::new();
    for url in urls {
        if let Some(t) = load_tokens(state, &url)? {
            tokens.push(t);
        }
    }
    Ok(tokens)
}

/// Clear tokens for a single registry. Removes the keychain entry, the
/// secrets file and the SQLite row.
pub fn clear_tokens(state: &AppState, registry_url: &str) -> Result<()> {
    keychain_delete(registry_url);
    secret_file::remove(
        state.root_dir.as_std_path(),
        &token_secret_name(registry_url),
    )
    .context("failed to delete auth tokens from the secrets store")?;
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    // Zero the deleted row's bytes instead of leaving them on a free page.
    conn.execute_batch("PRAGMA secure_delete = ON;")
        .context("failed to enable secure_delete")?;
    conn.execute(
        "DELETE FROM auth_tokens WHERE registry_url = ?1",
        [registry_url],
    )
    .context("failed to clear auth tokens")?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use camino::Utf8PathBuf;
    use mockito::Server;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("vh-auth-tests-{label}-{nanos}")),
        )
        .expect("temp path should be utf-8")
    }

    /// Build a minimal JWT with a given `exp` claim (no signature — for testing only).
    fn make_jwt_with_exp(exp: u64) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"HS256\",\"typ\":\"JWT\"}");
        let payload =
            URL_SAFE_NO_PAD.encode(format!("{{\"sub\":\"u1\",\"exp\":{exp}}}").as_bytes());
        // Signature segment is arbitrary — we don't validate it.
        format!("{header}.{payload}.fakesig")
    }

    // ── RFC 7636 test vector ──────────────────────────────────────────────────

    /// RFC 7636 Appendix B test vector.
    ///
    /// verifier:  dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk
    /// challenge: E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM
    #[test]
    fn code_challenge_matches_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected_challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        let computed = derive_code_challenge(verifier);
        assert_eq!(
            computed, expected_challenge,
            "code_challenge derivation must match RFC 7636 Appendix B test vector"
        );
    }

    // ── needs_refresh tests ───────────────────────────────────────────────────

    #[test]
    fn needs_refresh_returns_true_within_5_min_of_expiry() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        // Expires in 4 minutes 59 seconds — should trigger refresh.
        let exp = now + 299;
        let token = make_jwt_with_exp(exp);
        assert!(
            AuthClient::needs_refresh(&token),
            "token expiring in <300s should need refresh"
        );
    }

    #[test]
    fn needs_refresh_returns_false_when_expiry_is_far() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        // Expires in 10 minutes — should NOT trigger refresh.
        let exp = now + 600;
        let token = make_jwt_with_exp(exp);
        assert!(
            !AuthClient::needs_refresh(&token),
            "token expiring in >300s should not need refresh"
        );
    }

    #[test]
    fn needs_refresh_returns_true_for_already_expired() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let exp = now.saturating_sub(60);
        let token = make_jwt_with_exp(exp);
        assert!(
            AuthClient::needs_refresh(&token),
            "already-expired token should need refresh"
        );
    }

    #[test]
    fn needs_refresh_returns_true_for_malformed_jwt() {
        assert!(
            AuthClient::needs_refresh("not.a.jwt"),
            "malformed JWT should conservatively need refresh"
        );
        assert!(
            AuthClient::needs_refresh(""),
            "empty string should conservatively need refresh"
        );
        assert!(
            AuthClient::needs_refresh("only-one-segment"),
            "single-segment token should conservatively need refresh"
        );
    }

    #[test]
    fn decode_jwt_sub_returns_sub_claim() {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"HS256\",\"typ\":\"JWT\"}");
        let payload = URL_SAFE_NO_PAD.encode(b"{\"sub\":\"user-123\",\"exp\":9999999999}");
        let token = format!("{header}.{payload}.fakesig");
        assert_eq!(decode_jwt_sub(&token), Some("user-123".to_string()));
    }

    #[test]
    fn decode_jwt_sub_returns_none_for_malformed_or_missing() {
        assert_eq!(decode_jwt_sub("not.a.jwt"), None);
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"HS256\"}");
        let payload = URL_SAFE_NO_PAD.encode(b"{\"exp\":1}");
        let token = format!("{header}.{payload}.fakesig");
        assert_eq!(decode_jwt_sub(&token), None, "payload has no sub claim");
    }

    #[test]
    fn needs_refresh_returns_true_for_missing_exp_claim() {
        // Payload has no `exp` field.
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"HS256\"}");
        let payload = URL_SAFE_NO_PAD.encode(b"{\"sub\":\"u1\"}");
        let token = format!("{header}.{payload}.fakesig");
        assert!(
            AuthClient::needs_refresh(&token),
            "JWT without exp claim should conservatively need refresh"
        );
    }

    // ── exchange_oauth_code tests ─────────────────────────────────────────────

    #[test]
    fn exchange_oauth_code_posts_form_to_cli_token_endpoint() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/portal/auth/cli/token")
            .match_header(
                "content-type",
                mockito::Matcher::Regex("application/x-www-form-urlencoded".to_string()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"access_token":"new_acc","refresh_token":"new_ref","token_type":"bearer"}"#,
            )
            .create();

        let client = AuthClient::new(server.url());
        let resp = client
            .exchange_oauth_code("test-code", "test-verifier")
            .expect("exchange should succeed");
        assert_eq!(resp.access_token, "new_acc");
        assert_eq!(resp.refresh_token, "new_ref");
        mock.assert();
    }

    #[test]
    fn exchange_oauth_code_fails_on_non_200() {
        let mut server = Server::new();
        let _mock = server
            .mock("POST", "/portal/auth/cli/token")
            .with_status(400)
            .with_body(r#"{"error":"invalid_grant"}"#)
            .create();

        let client = AuthClient::new(server.url());
        let result = client.exchange_oauth_code("bad-code", "verifier");
        assert!(result.is_err(), "non-200 response should return Err");
    }

    // ── initiate_oauth_flow_with_redirect tests ───────────────────────────────

    #[test]
    fn initiate_oauth_flow_with_redirect_builds_correct_url() {
        let client = AuthClient::new("https://app.vectorhawk.ai");
        let redirect = "http://127.0.0.1:39127/oauth/cli/callback";
        let init = client
            .initiate_oauth_flow_with_redirect(redirect)
            .expect("should initiate");

        assert!(
            init.auth_url.contains("/portal/auth/cli/authorize"),
            "URL must target /portal/auth/cli/authorize: {}",
            init.auth_url
        );
        assert!(
            init.auth_url.contains("code_challenge_method=S256"),
            "URL must include S256 method: {}",
            init.auth_url
        );
        assert!(
            init.auth_url.contains(&init.state),
            "URL must include state parameter"
        );
        assert!(
            init.auth_url.contains(&init.code_challenge),
            "URL must include code_challenge"
        );
        // code_verifier must not appear in the URL (only challenge goes to server).
        assert!(
            !init.auth_url.contains(&init.code_verifier),
            "URL must NOT expose code_verifier"
        );
    }

    #[test]
    fn code_verifier_and_challenge_are_correct_length() {
        let client = AuthClient::new("https://app.vectorhawk.ai");
        let init = client
            .initiate_oauth_flow_with_redirect("http://127.0.0.1:39127/oauth/cli/callback")
            .expect("should initiate");
        // base64url-no-padding of 32 bytes = 43 chars.
        assert_eq!(
            init.code_verifier.len(),
            43,
            "code_verifier must be 43 chars"
        );
        assert_eq!(
            init.code_challenge.len(),
            43,
            "code_challenge must be 43 chars"
        );
    }

    #[test]
    fn code_challenge_is_sha256_of_verifier() {
        let client = AuthClient::new("https://app.vectorhawk.ai");
        let init = client
            .initiate_oauth_flow_with_redirect("http://127.0.0.1:39127/oauth/cli/callback")
            .expect("should initiate");
        let expected = derive_code_challenge(&init.code_verifier);
        assert_eq!(
            init.code_challenge, expected,
            "code_challenge must equal derive_code_challenge(code_verifier)"
        );
    }

    // ── Legacy initiate_oauth_flow tests ─────────────────────────────────────

    #[test]
    fn oauth_initiation_produces_auth_url_with_state() {
        let client = AuthClient::new("https://app.vectorhawk.ai");
        let init = client.initiate_oauth_flow().expect("should initiate");
        assert!(
            init.auth_url.contains("authorize"),
            "auth_url should contain 'authorize': {}",
            init.auth_url
        );
        assert!(
            init.auth_url.contains(&init.state),
            "auth_url should contain the state parameter"
        );
        assert!(
            !init.code_verifier.is_empty(),
            "code_verifier should not be empty"
        );
    }

    // ── Refresh + me tests ────────────────────────────────────────────────────

    #[test]
    fn refresh_returns_new_tokens() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/portal/auth/refresh")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"access_token":"new_acc","refresh_token":"new_ref","token_type":"bearer"}"#,
            )
            .create();

        let client = AuthClient::new(server.url());
        let resp = client.refresh("old_ref").expect("refresh should succeed");
        assert_eq!(resp.access_token, "new_acc");
        mock.assert();
    }

    #[test]
    fn me_returns_user_info() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/portal/auth/me")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id":"u1","email":"test@example.com","display_name":"Test User"}"#)
            .create();

        let client = AuthClient::new(server.url());
        let info = client.me("tok123").expect("me should succeed");
        assert_eq!(info.email, "test@example.com");
        mock.assert();
    }

    // ── Token storage tests ───────────────────────────────────────────────────
    //
    // These tests run with VECTORHAWK_DISABLE_KEYCHAIN=1 so they exercise the
    // SQLite fallback path without touching the real macOS Keychain.
    //
    // `load_tokens` calls `keychain_get` unconditionally on *every* call —
    // not just when a caller happens to have saved something — so this is
    // not an opt-in nicety: any test anywhere in this crate's test binary
    // that reaches `load_tokens`/`save_tokens`/`clear_tokens` touches the
    // real OS keychain unless the flag is set first. A past incident (a
    // keychain access popup firing on a developer's actual Mac during `cargo
    // test`) showed that per-test opt-in guards are not sufficient — it's
    // too easy to add a new test that forgets one. `disable_real_keychain_for_tests`
    // below is the actual backstop: it runs once, automatically, before any
    // `#[test]` in this binary even starts, via `ctor`. The per-test
    // `KeychainOff` guard below is kept for local clarity/documentation at
    // each call site and because it's still useful for the mutex-serialized
    // "concurrent set/clear" story if `ctor` is ever dropped, but the ctor
    // function is what actually provides the guarantee.
    #[ctor::ctor]
    fn disable_real_keychain_for_tests() {
        std::env::set_var("VECTORHAWK_DISABLE_KEYCHAIN", "1");
    }

    /// Regression guard for the safety net itself: if `#[ctor]` support
    /// ever breaks (stripped by a linker config, crate removed, etc.) this
    /// fails loudly instead of silently letting every other test fall back
    /// to touching the real keychain.
    #[test]
    fn real_keychain_is_disabled_for_this_test_binary() {
        assert_eq!(
            std::env::var("VECTORHAWK_DISABLE_KEYCHAIN").as_deref(),
            Ok("1"),
            "disable_real_keychain_for_tests' #[ctor] must run before any test in this binary"
        );
    }

    /// RAII guard that (re-)asserts `VECTORHAWK_DISABLE_KEYCHAIN=1` for the
    /// duration of a test. Holds a global mutex so concurrent tests can't
    /// interleave their set/clear of the process-global env var.
    ///
    /// Drop deliberately re-sets the flag rather than removing it:
    /// `disable_real_keychain_for_tests` above sets it once for the whole
    /// process and nothing in this binary should ever be allowed to turn it
    /// back off, even transiently, while other threads may be mid-test.
    struct KeychainOff {
        _g: std::sync::MutexGuard<'static, ()>,
    }
    static KEYCHAIN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    impl KeychainOff {
        fn enable() -> Self {
            let _g = KEYCHAIN_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("VECTORHAWK_DISABLE_KEYCHAIN", "1");
            KeychainOff { _g }
        }
    }
    impl Drop for KeychainOff {
        fn drop(&mut self) {
            std::env::set_var("VECTORHAWK_DISABLE_KEYCHAIN", "1");
        }
    }

    #[test]
    fn revoke_posts_the_refresh_token_and_scope() {
        let mut server = Server::new();
        let m = server
            .mock("POST", "/portal/auth/revoke")
            .match_body(mockito::Matcher::Json(serde_json::json!({
                "refresh_token": "rt-1",
                "scope": "family"
            })))
            .with_status(200)
            .with_body(r#"{"revoked":true,"families_revoked":1,"device_id":null}"#)
            .create();
        AuthClient::new(server.url())
            .revoke("rt-1", "family")
            .unwrap();
        m.assert();
    }

    #[test]
    fn deprovision_device_posts_with_bearer_and_reports_failure() {
        let mut server = Server::new();
        let ok = server
            .mock("POST", "/api/devices/dev-1/deprovision")
            .match_header("authorization", "Bearer acc-1")
            .with_status(200)
            .with_body("{}")
            .create();
        let client = AuthClient::new(server.url());
        client.deprovision_device("acc-1", "dev-1").unwrap();
        ok.assert();

        server
            .mock("POST", "/api/devices/dev-2/deprovision")
            .with_status(404)
            .create();
        assert!(client.deprovision_device("acc-1", "dev-2").is_err());
    }

    #[test]
    fn save_and_load_tokens_roundtrip() {
        let _guard = KeychainOff::enable();
        let root = temp_root("token-roundtrip");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        save_tokens(&state, "http://localhost:8000", "acc", "ref").expect("save tokens");
        let loaded = load_tokens(&state, "http://localhost:8000")
            .expect("load tokens")
            .expect("tokens should exist");
        assert_eq!(loaded.access_token, "acc");
        assert_eq!(loaded.refresh_token, "ref");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_tokens_removes_entry() {
        let _guard = KeychainOff::enable();
        let root = temp_root("token-clear");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        save_tokens(&state, "http://localhost:8000", "acc", "ref").expect("save tokens");
        clear_tokens(&state, "http://localhost:8000").expect("clear tokens");
        let loaded = load_tokens(&state, "http://localhost:8000").expect("load tokens");
        assert!(loaded.is_none(), "tokens should be cleared");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_tokens_leaves_no_token_bytes_in_the_db_file() {
        let _guard = KeychainOff::enable();
        let root = temp_root("token-scrub");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        save_tokens(
            &state,
            "http://localhost:8000",
            "ACCESS-SENTINEL-7f3a",
            "REFRESH-SENTINEL-7f3a",
        )
        .expect("save tokens");
        clear_tokens(&state, "http://localhost:8000").expect("clear tokens");

        let bytes = std::fs::read(&state.db_path).expect("read db");
        let needle = b"SENTINEL-7f3a";
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle),
            "a cleared token must not survive on a free page"
        );
    }

    #[test]
    fn load_all_tokens_returns_all_rows() {
        let _guard = KeychainOff::enable();
        let root = temp_root("load-all");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        save_tokens(&state, "http://localhost:8000", "acc1", "ref1").expect("save");
        save_tokens(&state, "http://localhost:9000", "acc2", "ref2").expect("save");

        let all = load_all_tokens(&state).expect("load all");
        assert_eq!(all.len(), 2, "should return both token rows");

        let _ = std::fs::remove_dir_all(&root);
    }

    fn db_contains(state: &AppState, needle: &[u8]) -> bool {
        let bytes = std::fs::read(&state.db_path).expect("read db");
        bytes.windows(needle.len()).any(|w| w == needle)
    }

    /// With the keychain unusable (Linux, headless macOS) the secrets go to
    /// an owner-only file and never into state.db.
    #[test]
    fn save_writes_to_the_secrets_file_not_sqlite_when_keychain_disabled() {
        let _guard = KeychainOff::enable();
        let root = temp_root("file-fallback");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        save_tokens(
            &state,
            "http://localhost:8000",
            "FILE-ACC-91c2",
            "FILE-REF-91c2",
        )
        .expect("save");

        assert!(!db_contains(&state, b"91c2"), "no token bytes in state.db");
        let file = secret_file::read(
            state.root_dir.as_std_path(),
            &token_secret_name("http://localhost:8000"),
        )
        .expect("read secrets file")
        .expect("secrets file written");
        assert!(file.contains("FILE-ACC-91c2") && file.contains("FILE-REF-91c2"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A state.db from an older build — tokens in the auth_tokens columns,
    /// the H3 key in sync_state, stale copies of earlier tokens on free
    /// pages — comes out of the next bootstrap with no secret bytes left in
    /// it and every secret still readable.
    #[test]
    fn bootstrap_moves_legacy_secrets_out_of_state_db() {
        let _guard = KeychainOff::enable();
        let root = temp_root("legacy-move");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");
        {
            let conn = Connection::open(&state.db_path).unwrap();
            // An older build's refresh: a token row rewritten in place,
            // leaving the earlier token on a free page.
            conn.execute(
                "INSERT INTO auth_tokens (registry_url, access_token, refresh_token)
                 VALUES ('https://r.example', 'OLDACC-5e8d', 'OLDREF-5e8d')",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE auth_tokens SET access_token = 'NEWACC-5e8d', refresh_token = 'NEWREF-5e8d'",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sync_state (key, value) VALUES ('mcp_local_key', 'LOCALKEY-5e8d')",
                [],
            )
            .unwrap();
            conn.execute(
                "DELETE FROM sync_state WHERE key = 'legacy_secrets_moved'",
                [],
            )
            .unwrap();
        }
        assert!(db_contains(&state, b"5e8d"), "fixture must plant the bytes");

        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap again");

        assert!(
            !db_contains(&state, b"5e8d"),
            "no secret bytes left in state.db"
        );
        let tokens = load_tokens(&state, "https://r.example")
            .unwrap()
            .expect("tokens still there");
        assert_eq!(tokens.access_token, "NEWACC-5e8d");
        assert_eq!(tokens.refresh_token, "NEWREF-5e8d");
        assert_eq!(state.local_mcp_key().unwrap(), "LOCALKEY-5e8d");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_tokens_removes_the_secrets_file() {
        let _guard = KeychainOff::enable();
        let root = temp_root("clear-file");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        save_tokens(&state, "http://localhost:8000", "acc", "ref").expect("save");
        clear_tokens(&state, "http://localhost:8000").expect("clear");
        assert_eq!(
            secret_file::read(
                state.root_dir.as_std_path(),
                &token_secret_name("http://localhost:8000"),
            )
            .unwrap(),
            None
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Backoff state (refresh_failures, next_refresh_attempt_at,
    /// last_refresh_status) lives in SQLite even when secrets are in the
    /// keychain. A successful save must reset these.
    #[test]
    fn save_resets_backoff_state() {
        let _guard = KeychainOff::enable();
        let root = temp_root("backoff-reset");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        save_tokens(&state, "http://localhost:8000", "acc", "ref").expect("save");
        record_refresh_failure(&state, "http://localhost:8000", "auth_failed", Some(99999))
            .expect("record failure");

        // A subsequent save (e.g. user re-login) must wipe the backoff.
        save_tokens(&state, "http://localhost:8000", "acc2", "ref2").expect("save");
        let loaded = load_tokens(&state, "http://localhost:8000")
            .expect("load")
            .expect("present");
        assert_eq!(loaded.refresh_failures, 0);
        assert_eq!(loaded.next_refresh_attempt_at, None);
        assert_eq!(loaded.last_refresh_status.as_deref(), Some("ok"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
