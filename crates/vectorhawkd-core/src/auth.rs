use crate::state::AppState;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
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
/// callback listener (M3) will receive the redirect and exchange the code
/// for tokens. For M1 this struct is returned but the callback exchange is a
/// placeholder — the daemon's fixed-port listener is a planned M3 feature.
#[derive(Debug)]
pub struct OAuthInitiation {
    /// URL to open in the user's browser.
    pub auth_url: String,
    /// PKCE code verifier — stored by the caller until the callback arrives.
    pub code_verifier: String,
    /// State parameter for CSRF protection.
    pub state: String,
}

// ── Stored tokens ────────────────────────────────────────────────────────────

pub struct StoredTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub registry_url: String,
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

    /// Initiate an OAuth browser-based login flow.
    ///
    /// Returns an [`OAuthInitiation`] whose `auth_url` should be opened in the
    /// user's browser. The daemon's fixed-port callback listener (M3) receives
    /// the redirect and completes the exchange.
    ///
    /// Per project convention login must use an OAuth browser flow — not a
    /// terminal password prompt. This method generates the authorization URL
    /// and PKCE parameters. Full PKCE callback handling lands in M3.
    pub fn initiate_oauth_flow(&self) -> Result<OAuthInitiation> {
        // Generate a simple PKCE code verifier and state.
        // M3 will replace this with a proper PKCE implementation including the
        // SHA-256 code challenge and fixed-port redirect URI.
        let state = uuid::Uuid::new_v4().to_string();
        let code_verifier = uuid::Uuid::new_v4().to_string();

        let base = self.base_url.trim_end_matches('/');
        let auth_url = format!(
            "{base}/portal/auth/oauth/authorize?response_type=code&client_id=runner&state={state}"
        );

        debug!(auth_url, "OAuth flow initiated — open URL in browser");

        Ok(OAuthInitiation {
            auth_url,
            code_verifier,
            state,
        })
    }

    /// Exchange an OAuth authorization code for tokens.
    ///
    /// Called by the daemon's callback listener once the browser redirects
    /// back with the `code` parameter. For M3 this will include PKCE
    /// verification; for M1 it is a direct code exchange.
    pub fn exchange_oauth_code(&self, code: &str, _code_verifier: &str) -> Result<TokenResponse> {
        let url = format!(
            "{}/portal/auth/oauth/token",
            self.base_url.trim_end_matches('/')
        );
        debug!(url, "exchanging OAuth code for tokens");

        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "code": code,
            "client_id": "runner",
        });

        let resp = self
            .http
            .post(&url)
            .json(&body)
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

    #[test]
    fn oauth_initiation_produces_auth_url_with_state() {
        let mut server = Server::new();
        // The server is not called during initiation — just a URL is built.
        let _unused = server.url(); // ensure server boots
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
}
