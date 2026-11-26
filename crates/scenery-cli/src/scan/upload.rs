//! Upload of a scan report: `GET /scan/session`, `POST /scan/reports`.
//!
//! The org scan token only ever lives in an `Authorization` header built per
//! request. It is never logged, printed, put in an error or saved.

use super::report::ScanReport;
use serde::Deserialize;
use std::fmt;
use std::time::Duration;

/// Retry delays after the first failed attempt (connect errors and 5xx only).
pub const DEFAULT_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

pub enum ScanAuth {
    /// Org scan token (`scn_scan_...`).
    Token(String),
    /// A signed-in user's access token (`scenery scan --once --login`).
    #[allow(dead_code)] // constructed by the `--login` flow (R5)
    Bearer(String),
}

impl ScanAuth {
    pub fn header(&self) -> String {
        match self {
            ScanAuth::Token(t) | ScanAuth::Bearer(t) => format!("Bearer {t}"),
        }
    }
}

// Never derive Debug on the auth type: it would print the token.
impl fmt::Debug for ScanAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ScanAuth(..)")
    }
}

#[derive(Debug, Deserialize)]
pub struct ScanSession {
    pub org_name: String,
    #[allow(dead_code)]
    pub attribution: String,
    #[allow(dead_code)]
    pub user_email: Option<String>,
    pub auto_submit_mcp: bool,
    pub max_items: usize,
}

#[derive(Debug, Deserialize)]
pub struct Rejected {
    pub kind: String,
    pub slug: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct ScanReceipt {
    #[allow(dead_code)]
    pub device_id: String,
    pub accepted: u64,
    pub deduped: u64,
    pub rejected: Vec<Rejected>,
    #[allow(dead_code)]
    pub mcp_submitted: u64,
    pub inventory_url: String,
}

#[derive(Debug)]
pub enum UploadError {
    Auth(String),
    TooOld,
    RunnerPresent,
    Limit(String),
    Network(String),
    Server(u16),
}

impl fmt::Display for UploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UploadError::Auth(code) => match code.as_str() {
                "scan_token_revoked" => write!(f, "The scan token was revoked by your IT admin."),
                "scan_token_expired" => write!(f, "The scan token has expired."),
                "scan_token_invalid" => write!(f, "The scan token was not recognised."),
                _ => write!(f, "The server refused the credentials ({code})."),
            },
            UploadError::TooOld => write!(
                f,
                "This scanner is too old for your Scenery server. Install the latest scenery."
            ),
            UploadError::RunnerPresent => write!(
                f,
                "The Scenery runner is already installed on this machine, so no scan is needed."
            ),
            UploadError::Limit(why) => write!(f, "{why}"),
            UploadError::Network(why) => write!(f, "Could not reach the server: {why}."),
            UploadError::Server(n) => write!(f, "The server answered with an error (HTTP {n})."),
        }
    }
}

/// Error code from `{"detail":{"code":..}}`, falling back to top-level `code`.
fn error_code(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let code = v.pointer("/detail/code").or_else(|| v.get("code"))?;
    code.as_str().map(str::to_string)
}

fn classify(status: u16, body: &[u8]) -> UploadError {
    let code = error_code(body);
    match (status, code.as_deref()) {
        (426, _) => UploadError::TooOld,
        (409, Some("runner_present")) => UploadError::RunnerPresent,
        (401, Some(c)) => UploadError::Auth(c.to_string()),
        (413, _) => UploadError::Limit("The report is too large for the server.".to_string()),
        (422, Some("too_many_items")) => {
            UploadError::Limit("The report has more items than the server accepts.".to_string())
        }
        (n, _) => UploadError::Server(n),
    }
}

fn url(base: &str, path: &str) -> String {
    format!("{}/api/scan/{path}", base.trim_end_matches('/'))
}

pub fn client() -> Result<reqwest::Client, UploadError> {
    scenery_core::http::async_client_builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| UploadError::Network(e.without_url().to_string()))
}

async fn send_once(
    req: reqwest::RequestBuilder,
    auth: &ScanAuth,
) -> Result<(u16, Vec<u8>), reqwest::Error> {
    let resp = req
        .header(reqwest::header::AUTHORIZATION, auth.header())
        .send()
        .await?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await?.to_vec();
    Ok((status, body))
}

pub async fn session(
    client: &reqwest::Client,
    base: &str,
    auth: &ScanAuth,
) -> Result<ScanSession, UploadError> {
    let req = client.get(url(base, "session"));
    let (status, body) = send_once(req, auth)
        .await
        .map_err(|e| UploadError::Network(e.without_url().to_string()))?;
    if status != 200 {
        return Err(classify(status, &body));
    }
    serde_json::from_slice(&body).map_err(|_| UploadError::Server(status))
}

pub async fn post_report(
    client: &reqwest::Client,
    base: &str,
    auth: &ScanAuth,
    report: &ScanReport,
    backoff: &[Duration],
) -> Result<ScanReceipt, UploadError> {
    let body = serde_json::to_vec(report).map_err(|_| UploadError::Server(0))?;
    post_body(client, base, auth, body, backoff).await
}

/// Post an already-serialized report (used as is by `--from-file`).
pub async fn post_body(
    client: &reqwest::Client,
    base: &str,
    auth: &ScanAuth,
    body: Vec<u8>,
    backoff: &[Duration],
) -> Result<ScanReceipt, UploadError> {
    let mut attempt = 0;
    loop {
        let req = client
            .post(url(base, "reports"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.clone());
        let retry = match send_once(req, auth).await {
            Ok((200, resp)) => {
                return serde_json::from_slice(&resp).map_err(|_| UploadError::Server(200));
            }
            Ok((status, resp)) if status >= 500 => (classify(status, &resp), true),
            Ok((status, resp)) => (classify(status, &resp), false),
            Err(e) => {
                let connect = e.is_connect();
                (UploadError::Network(e.without_url().to_string()), connect)
            }
        };
        let (err, retryable) = retry;
        match backoff.get(attempt) {
            Some(delay) if retryable => {
                tokio::time::sleep(*delay).await;
                attempt += 1;
            }
            _ => return Err(err),
        }
    }
}

#[cfg(test)]
#[path = "upload_tests.rs"]
mod upload_tests;
