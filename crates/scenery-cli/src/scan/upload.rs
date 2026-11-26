//! Upload of a scan report: `GET /scan/session`, `POST /scan/reports`.
//!
//! The org scan token only ever lives in an `Authorization` header built per
//! request. It is never logged, printed, put in an error or saved.

use super::report::ScanReport;
use reqwest::header::HeaderValue;
use serde::Deserialize;
use std::fmt;
use std::time::Duration;

/// Retry delays after the first failed attempt (connect errors and 5xx only).
pub const DEFAULT_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

pub enum ScanAuth {
    /// Org scan token (`scn_scan_...`).
    Token(String),
    /// A signed-in user's access token (`scenery scan --once --login`).
    Bearer(String),
}

impl ScanAuth {
    /// The `Authorization` value, marked sensitive so hyper and reqwest
    /// never print it (Debug output, HPACK indexing).
    pub fn header_value(&self) -> Result<HeaderValue, UploadError> {
        let token = match self {
            ScanAuth::Token(t) | ScanAuth::Bearer(t) => t,
        };
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| UploadError::BadCredential)?;
        value.set_sensitive(true);
        Ok(value)
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
    /// The credential has characters an HTTP header cannot carry.
    BadCredential,
    TooOld,
    RunnerPresent,
    /// 409 `device_claimed`: a token report cannot update a device a user owns.
    DeviceClaimed,
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
            UploadError::BadCredential => {
                write!(f, "The credential contains characters that cannot be sent.")
            }
            UploadError::TooOld => write!(
                f,
                "This scanner is too old for your Scenery server. Install the latest scenery."
            ),
            UploadError::RunnerPresent => write!(
                f,
                "The Scenery runner is already installed on this machine, so no scan is needed."
            ),
            UploadError::DeviceClaimed => write!(
                f,
                "This machine is already linked to a Scenery user, so a scan token cannot \
                 update it. Sign in as that user with --login instead"
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
        (409, Some("device_claimed")) => UploadError::DeviceClaimed,
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

/// Where and as whom a report goes.
pub struct Uplink<'a> {
    pub client: &'a reqwest::Client,
    pub base: &'a str,
    pub auth: &'a ScanAuth,
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
    auth: &HeaderValue,
) -> Result<(u16, Vec<u8>), reqwest::Error> {
    let resp = req
        .header(reqwest::header::AUTHORIZATION, auth.clone())
        .send()
        .await?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await?.to_vec();
    Ok((status, body))
}

pub async fn session(link: &Uplink<'_>) -> Result<ScanSession, UploadError> {
    let header = link.auth.header_value()?;
    let req = link.client.get(url(link.base, "session"));
    let (status, body) = send_once(req, &header)
        .await
        .map_err(|e| UploadError::Network(e.without_url().to_string()))?;
    if status != 200 {
        return Err(classify(status, &body));
    }
    serde_json::from_slice(&body).map_err(|_| UploadError::Server(status))
}

pub async fn post_report(
    link: &Uplink<'_>,
    report: &ScanReport,
    backoff: &[Duration],
) -> Result<ScanReceipt, UploadError> {
    let body = serde_json::to_vec(report).map_err(|_| UploadError::Server(0))?;
    post_body(link, body, backoff).await
}

/// Post an already-serialized report (used as is by `--from-file`).
pub async fn post_body(
    link: &Uplink<'_>,
    body: Vec<u8>,
    backoff: &[Duration],
) -> Result<ScanReceipt, UploadError> {
    let header = link.auth.header_value()?;
    let mut attempt = 0;
    loop {
        let req = link
            .client
            .post(url(link.base, "reports"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.clone());
        let retry = match send_once(req, &header).await {
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
