use crate::state::AppState;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::debug;

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
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("token refresh failed (HTTP {status}): {body}");
        }

        resp.json()
            .context("failed to deserialize refresh response")
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

    /// Determine if the access token needs to be refreshed.
    ///
    /// Decodes the JWT payload without signature validation (we trust the
    /// daemon-stored token) and returns `true` if `exp - now() < 300s`.
    ///
    /// On any parse failure (malformed token, missing exp claim, clock error),
    /// returns `true` conservatively — it is safer to attempt a refresh than
    /// to use a potentially-expired token.
    pub fn needs_refresh(access_token: &str) -> bool {
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

/// Decode the `exp` claim from a JWT without verifying the signature.
///
/// Returns `None` on any parse error. The JWT is assumed to be three
/// base64url-no-padding segments separated by `.`.
fn decode_jwt_exp(token: &str) -> Option<u64> {
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
    let claims: JwtClaims = serde_json::from_slice(&payload_bytes).ok()?;
    Some(claims.exp)
}

// ── Token storage ────────────────────────────────────────────────────────────

pub fn save_tokens(
    state: &AppState,
    registry_url: &str,
    access_token: &str,
    refresh_token: &str,
) -> Result<()> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    conn.execute(
        "INSERT INTO auth_tokens (registry_url, access_token, refresh_token)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(registry_url) DO UPDATE SET
             access_token = excluded.access_token,
             refresh_token = excluded.refresh_token,
             saved_at = CURRENT_TIMESTAMP",
        params![registry_url, access_token, refresh_token],
    )
    .context("failed to save auth tokens")?;
    Ok(())
}

pub fn load_tokens(state: &AppState, registry_url: &str) -> Result<Option<StoredTokens>> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    let result = conn
        .query_row(
            "SELECT access_token, refresh_token FROM auth_tokens WHERE registry_url = ?1",
            [registry_url],
            |row| {
                Ok(StoredTokens {
                    access_token: row.get(0)?,
                    refresh_token: row.get(1)?,
                    registry_url: registry_url.to_string(),
                })
            },
        )
        .optional()?;
    Ok(result)
}

/// Load all stored token rows, returning a vec of (registry_url, access_token, refresh_token).
///
/// Used by the daemon refresh loop to check every registry for near-expiry tokens.
pub fn load_all_tokens(state: &AppState) -> Result<Vec<StoredTokens>> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    let mut stmt = conn
        .prepare("SELECT registry_url, access_token, refresh_token FROM auth_tokens")
        .context("failed to prepare load_all_tokens query")?;
    let rows = stmt
        .query_map([], |row| {
            Ok(StoredTokens {
                registry_url: row.get(0)?,
                access_token: row.get(1)?,
                refresh_token: row.get(2)?,
            })
        })
        .context("failed to query auth_tokens")?;
    let mut tokens = Vec::new();
    for row in rows {
        tokens.push(row.context("failed to read auth_token row")?);
    }
    Ok(tokens)
}

pub fn clear_tokens(state: &AppState, registry_url: &str) -> Result<()> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
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

    #[test]
    fn save_and_load_tokens_roundtrip() {
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
        let root = temp_root("token-clear");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        save_tokens(&state, "http://localhost:8000", "acc", "ref").expect("save tokens");
        clear_tokens(&state, "http://localhost:8000").expect("clear tokens");
        let loaded = load_tokens(&state, "http://localhost:8000").expect("load tokens");
        assert!(loaded.is_none(), "tokens should be cleared");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_all_tokens_returns_all_rows() {
        let root = temp_root("load-all");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        save_tokens(&state, "http://localhost:8000", "acc1", "ref1").expect("save");
        save_tokens(&state, "http://localhost:9000", "acc2", "ref2").expect("save");

        let all = load_all_tokens(&state).expect("load all");
        assert_eq!(all.len(), 2, "should return both token rows");

        let _ = std::fs::remove_dir_all(&root);
    }
}
