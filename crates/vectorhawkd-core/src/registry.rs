use crate::{
    importer::ImportOutcome,
    policy::{Policy, PolicyClient, PolicyStatus},
    state::AppState,
};
use anyhow::{Context, Result};
use camino::Utf8Path;
use rusqlite::{params, Connection, OptionalExtension};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{debug, warn};

// ── Registry wire types ───────────────────────────────────────────────────────

/// Wire format returned by `GET /skills/{id}/policy`.
#[derive(Debug, Deserialize, Serialize)]
struct PolicyApiResponse {
    skill_id: String,
    /// "active" | "blocked"
    status: String,
    channel: Option<String>,
    target_version: Option<String>,
    minimum_allowed_version: Option<String>,
    blocked_message: Option<String>,
    policy_ttl_seconds: Option<u64>,
}

/// Wire format returned by `GET /skills/{id}/versions/{version}`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ArtifactMetadata {
    pub skill_id: String,
    pub version: String,
    pub download_url: String,
    pub sha256: String,
    pub size_bytes: Option<u64>,
}

/// A single skill result from `GET /portal/skills?search=<query>`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SearchResult {
    pub skill_id: String,
    pub name: String,
    pub latest_version: Option<String>,
    pub publisher_name: Option<String>,
    pub description: Option<String>,
}

/// Wire format returned by the search endpoint.
#[derive(Debug, Deserialize)]
struct SearchApiResponse {
    items: Vec<SearchResult>,
}

/// Entry returned per skill by `POST /skills/status`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SkillStatusEntry {
    pub status: String,
    pub latest_version: Option<String>,
}

/// Response from `POST /skills/status`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SkillStatusResponse {
    pub statuses: std::collections::HashMap<String, SkillStatusEntry>,
    #[serde(default)]
    pub unknown: Vec<String>,
}

/// Frontmatter summary returned by `POST /portal/skills/compile`.
#[derive(Debug, Deserialize, Clone)]
pub struct CompileFrontmatterSummary {
    pub name: String,
    pub description: String,
    pub license: String,
    pub vh_version: Option<String>,
    pub vh_publisher: Option<String>,
}

/// A single field-level error from a 422 compile response.
#[derive(Debug, Deserialize, Clone)]
pub struct CompileErrorDetail {
    pub path: String,
    pub message: String,
}

/// Success response from `POST /portal/skills/compile?publish=true`.
#[derive(Debug, Deserialize, Clone)]
pub struct CompilePublishResponse {
    pub content_hash: String,
    pub size_bytes: u64,
    pub frontmatter: CompileFrontmatterSummary,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub compiled_artifact_key: String,
    pub source_artifact_key: String,
    pub download_url: String,
}

/// Skill detail returned by `GET /portal/skills/{skill_id}`.
#[derive(Debug, Deserialize, Clone)]
pub struct SkillDetail {
    pub skill_id: String,
    pub name: String,
    pub latest_version: Option<String>,
    pub publisher_name: Option<String>,
    pub description: Option<String>,
}

// ── Preinstall governance wire types ─────────────────────────────────────────

/// Publisher identity returned inside `PreinstallGovernance`.
#[derive(Debug, Deserialize, Clone)]
pub struct PreinstallPublisher {
    pub id: String,
    pub name: String,
    pub verified: bool,
    pub verified_at: Option<String>,
}

/// Org-scoped policy status for the skill.
#[derive(Debug, Deserialize, Clone)]
pub struct PreinstallPolicy {
    /// "approved" | "pending" | "blocked"
    pub status: String,
    pub org_id: Option<String>,
    pub message: Option<String>,
}

/// A single scope (permission) the skill requests.
#[derive(Debug, Deserialize, Clone)]
pub struct PreinstallScope {
    pub name: String,
    pub description: Option<String>,
    pub risk_level: Option<String>,
}

/// Static-analysis scan result.
#[derive(Debug, Deserialize, Clone)]
pub struct PreinstallScan {
    /// "clean" | "flagged" | "unknown"
    pub verdict: String,
    pub scanner: Option<String>,
    pub scanned_at: Option<String>,
    pub findings_count: Option<u32>,
}

/// Last-audit metadata.
#[derive(Debug, Deserialize, Clone)]
pub struct PreinstallAudit {
    pub last_audited_at: Option<String>,
    pub auditor: Option<String>,
}

/// Aggregated pre-install governance data returned by `GET /skills/{id}/preinstall`.
#[derive(Debug, Deserialize, Clone)]
pub struct PreinstallGovernance {
    pub publisher: PreinstallPublisher,
    pub policy: PreinstallPolicy,
    #[serde(default)]
    pub scopes_requested: Vec<PreinstallScope>,
    pub scan: Option<PreinstallScan>,
    pub audit: Option<PreinstallAudit>,
}

// ── Import preview / submit wire types ───────────────────────────────────────

/// Classification result from `POST /runner/import/preview`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ImportPreview {
    /// "github_url" | "raw_skill_md" | "npx_command" | "mcp_json" | "generic_ref"
    pub input_type: String,
    pub description: String,
    pub raw_input: String,
    pub server_name: Option<String>,
    pub proposed_name: Option<String>,
}

/// Wire response from `POST /runner/import/submit`.
#[derive(Debug, Deserialize)]
struct ImportSubmitResponse {
    /// "skill_scaffolded" | "mcp_server_requested" | "skill_submitted"
    pub outcome_type: String,
    pub bundle_path: Option<String>,
    pub server_name: Option<String>,
    pub submission_id: Option<String>,
    pub status: Option<String>,
}

/// A single approved MCP server entry returned by the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovedServerEntry {
    /// Stable identifier used for tool namespacing (e.g. `"github"`).
    pub server_id: String,
    /// Human-readable display name.
    pub name: String,
    /// Transport variant: `"http"`, `"gateway"`, or `"stdio"`.
    pub transport_type: String,
    /// Base URL for HTTP/gateway backends; `None` for stdio.
    pub url: Option<String>,
    /// Priority for tool budget allocation (higher = served first).
    pub priority: u8,
}

/// Response from the approved-servers endpoint.
#[derive(Debug, Serialize, Deserialize)]
pub struct ApprovedServersResponse {
    pub servers: Vec<ApprovedServerEntry>,
}

/// A single audit event ready for upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEventPayload {
    pub event_type: String,
    pub payload: serde_json::Value,
    pub created_at: i64,
}

// ── RegistryClient (HTTP implementation) ─────────────────────────────────────

/// HTTP client for the VectorHawk registry.
///
/// Handles policy lookup, artifact metadata, package downloads, and audit
/// upload. Has no local state — use [`HttpPolicyClient`] for cached policy.
pub struct RegistryClient {
    pub base_url: String,
    pub http: reqwest::blocking::Client,
    auth_token: Option<String>,
}

impl RegistryClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::blocking::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client should build"),
            auth_token: None,
        }
    }

    /// Set the Bearer auth token for authenticated requests (consuming).
    pub fn with_auth(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    /// Set the auth token on an existing client (non-consuming).
    pub fn set_auth(&mut self, token: impl Into<String>) {
        self.auth_token = Some(token.into());
    }

    /// Fetch the list of approved backend MCP servers for this device.
    pub fn fetch_approved_servers(&self) -> Result<ApprovedServersResponse> {
        let url = format!(
            "{}/api/runner/approved-servers",
            self.base_url.trim_end_matches('/')
        );
        debug!(url, "fetching approved servers");

        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("registry returned HTTP {status} for approved servers: {body}");
        }

        resp.json()
            .context("failed to deserialize approved servers response")
    }

    /// Fetch policy from the registry.
    ///
    /// Returns the parsed `Policy` and the TTL in seconds to use for caching.
    pub fn fetch_policy_remote(&self, skill_id: &str) -> Result<(Policy, u64)> {
        let url = format!(
            "{}/skills/{}/policy",
            self.base_url.trim_end_matches('/'),
            skill_id
        );
        debug!(url, "fetching policy from registry");

        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("registry returned HTTP {status} for policy {skill_id}: {body}");
        }

        let wire: PolicyApiResponse = resp
            .json()
            .context("failed to deserialize policy response")?;

        let ttl = wire.policy_ttl_seconds.unwrap_or(86400);
        let policy = policy_from_wire(wire)?;
        Ok((policy, ttl))
    }

    /// Fetch the current policy for a specific skill (no-cache, single call).
    pub fn fetch_skill_policy(&self, skill_id: &str) -> Result<SkillPolicyResponse> {
        let url = format!(
            "{}/skills/{}/policy",
            self.base_url.trim_end_matches('/'),
            skill_id
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("registry returned HTTP {status} for skill policy {skill_id}: {body}");
        }

        let wire: PolicyApiResponse = resp
            .json()
            .context("failed to deserialize skill policy response")?;

        Ok(SkillPolicyResponse {
            skill_id: wire.skill_id,
            status: wire.status,
            minimum_allowed_version: wire.minimum_allowed_version,
            blocked_message: wire.blocked_message,
        })
    }

    /// Report an audit batch to the registry.
    pub fn upload_audit_batch(&self, events: &[AuditEventPayload]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let url = format!("{}/api/runner/audit", self.base_url.trim_end_matches('/'));
        debug!(url, count = events.len(), "uploading audit batch");

        let mut req = self.http.post(&url).json(events);
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }

        let resp = req
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("audit upload failed (HTTP {status}): {body}");
        }

        Ok(())
    }

    /// Health check — returns `Ok(true)` if the registry is reachable.
    pub fn health_check(&self) -> Result<bool> {
        let url = format!("{}/health", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;
        Ok(resp.status().is_success())
    }

    /// Fetch artifact metadata for a specific skill version.
    pub fn fetch_artifact_metadata(
        &self,
        skill_id: &str,
        version: &str,
    ) -> Result<ArtifactMetadata> {
        let url = format!(
            "{}/skills/{}/versions/{}",
            self.base_url.trim_end_matches('/'),
            skill_id,
            version
        );
        debug!(url, "fetching artifact metadata");

        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!(
                "registry returned HTTP {status} for artifact {skill_id}@{version}: {body}"
            );
        }

        resp.json()
            .context("failed to deserialize artifact metadata")
    }

    /// Download an artifact to `dest`, verifying its SHA-256 hash.
    ///
    /// The file at `dest` will be created (or overwritten). On hash mismatch
    /// the download is discarded and an error is returned.
    pub fn download_artifact(
        &self,
        download_url: &str,
        expected_sha256: &str,
        dest: &Utf8Path,
    ) -> Result<()> {
        debug!(url = download_url, dest = %dest, "downloading artifact");

        let mut resp = self
            .http
            .get(download_url)
            .send()
            .with_context(|| format!("failed to download artifact from {download_url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            anyhow::bail!("artifact download returned HTTP {status}");
        }

        let mut hasher = Sha256::new();
        let mut out =
            std::fs::File::create(dest).with_context(|| format!("failed to create {dest}"))?;

        let mut buf = [0u8; 65536];
        loop {
            let n = resp
                .read(&mut buf)
                .context("error reading download stream")?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .context("error writing download to disk")?;
        }
        drop(out);

        let actual = hex::encode(hasher.finalize());
        if actual != expected_sha256 {
            let _ = std::fs::remove_file(dest);
            anyhow::bail!("artifact hash mismatch: expected {expected_sha256}, got {actual}");
        }

        debug!("artifact hash verified");
        Ok(())
    }

    /// Search the registry for skills matching `query`.
    pub fn search_skills(&self, query: &str) -> Result<Vec<SearchResult>> {
        let url = format!(
            "{}/portal/skills?search={}",
            self.base_url.trim_end_matches('/'),
            urlencoding::encode(query)
        );
        debug!(url, "searching skills");

        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("registry returned HTTP {status} for search: {body}");
        }

        let wire: SearchApiResponse = resp
            .json()
            .context("failed to deserialize search response")?;
        Ok(wire.items)
    }

    /// Fetch skill detail including latest version info.
    pub fn fetch_skill_detail(&self, skill_id: &str) -> Result<SkillDetail> {
        let url = format!(
            "{}/portal/skills/{}",
            self.base_url.trim_end_matches('/'),
            skill_id
        );
        debug!(url, "fetching skill detail");

        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("registry returned HTTP {status} for skill {skill_id}: {body}");
        }

        resp.json()
            .context("failed to deserialize skill detail response")
    }

    /// Check the lifecycle status of a batch of skills.
    ///
    /// Calls `POST /skills/status`. Returns an error if unreachable or non-success.
    /// Callers should handle errors by skipping lifecycle checks.
    pub fn check_skill_status(&self, skill_ids: &[String]) -> Result<SkillStatusResponse> {
        if skill_ids.is_empty() {
            return Ok(SkillStatusResponse {
                statuses: std::collections::HashMap::new(),
                unknown: vec![],
            });
        }

        let url = format!("{}/skills/status", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({ "skill_ids": skill_ids });
        debug!(url, "checking skill lifecycle status");

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("registry returned HTTP {status} for skill status check: {body}");
        }

        resp.json()
            .context("failed to deserialize skill status response")
    }

    /// Upload a SKILL.md source tree to the registry compile endpoint and publish.
    ///
    /// Posts a gzipped tar to `POST /portal/skills/compile` with `publish=true`.
    /// Requires auth token to be set via [`with_auth`].
    pub fn compile_and_publish(
        &self,
        source_tar_gz_bytes: Vec<u8>,
    ) -> Result<CompilePublishResponse> {
        let token = self.auth_token.as_ref().ok_or_else(|| {
            anyhow::anyhow!("not authenticated; run `vectorhawk auth login` first")
        })?;

        let base = self.base_url.trim_end_matches('/');
        let url = format!("{base}/portal/skills/compile");
        debug!(
            url,
            size_bytes = source_tar_gz_bytes.len(),
            "uploading SKILL.md tree"
        );

        let file_part = reqwest::blocking::multipart::Part::bytes(source_tar_gz_bytes)
            .file_name("skill-source.tar.gz")
            .mime_str("application/gzip")
            .context("invalid MIME type for source upload")?;

        let form = reqwest::blocking::multipart::Form::new()
            .part("file", file_part)
            .text("publish", "true");

        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .multipart(form)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        let status = resp.status();

        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            let body = resp.text().unwrap_or_default();
            let errors_msg = parse_compile_errors(&body);
            anyhow::bail!("publish rejected (422): {errors_msg}");
        }

        if status == reqwest::StatusCode::ACCEPTED {
            return resp
                .json()
                .context("failed to deserialize 202 compile response");
        }

        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("compile/publish failed (HTTP {status}): {body}");
        }

        resp.json()
            .context("failed to deserialize compile publish response")
    }

    /// Fetch aggregated pre-install governance data for a skill.
    pub fn fetch_preinstall_governance(&self, skill_id: &str) -> Result<PreinstallGovernance> {
        let url = format!(
            "{}/skills/{}/preinstall",
            self.base_url.trim_end_matches('/'),
            skill_id
        );
        debug!(url, "fetching preinstall governance");

        let mut req = self.http.get(&url);
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }

        let resp = req
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!("skill '{skill_id}' not found in registry");
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!(
                "registry returned HTTP {status} for preinstall governance of '{skill_id}': {body}"
            );
        }

        resp.json()
            .context("failed to deserialize preinstall governance response")
    }

    /// Ask the registry to classify `raw_input` and return an import preview.
    ///
    /// Calls `POST /runner/import/preview` with Bearer auth.
    /// Requires that `auth_token` be set.
    pub fn runner_import_preview(&self, raw_input: &str) -> Result<ImportPreview> {
        let token = self.auth_token.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "not authenticated; run `vectorhawk auth login` before using registry import"
            )
        })?;

        let base = self.base_url.trim_end_matches('/');
        let url = format!("{base}/runner/import/preview");
        debug!(url, "requesting import preview from registry");

        let body = serde_json::json!({ "raw_input": raw_input });

        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        let status = resp.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!(
                "registry returned 401 Unauthorized — your session has expired; \
                 run `vectorhawk auth login` to refresh"
            );
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            anyhow::bail!(
                "registry returned 403 Forbidden — your account may be inactive or \
                 lack permission for import; contact your IT administrator"
            );
        }
        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            let body_text = resp.text().unwrap_or_default();
            anyhow::bail!("registry could not classify input (422): {body_text}");
        }
        if !status.is_success() {
            let body_text = resp.text().unwrap_or_default();
            anyhow::bail!("registry returned HTTP {status} for import preview: {body_text}");
        }

        resp.json()
            .context("failed to deserialize import preview response")
    }

    /// Submit a previously-previewed import to the registry.
    ///
    /// Calls `POST /runner/import/submit` with Bearer auth.
    /// Requires that `auth_token` be set.
    pub fn runner_import_submit(&self, preview: &ImportPreview) -> Result<ImportOutcome> {
        let token = self.auth_token.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "not authenticated; run `vectorhawk auth login` before using registry import"
            )
        })?;

        let base = self.base_url.trim_end_matches('/');
        let url = format!("{base}/runner/import/submit");
        debug!(url, input_type = %preview.input_type, "submitting import to registry");

        let body = serde_json::json!({ "raw_input": preview.raw_input });

        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        let status = resp.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!(
                "registry returned 401 Unauthorized — your session has expired; \
                 run `vectorhawk auth login` to refresh"
            );
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            anyhow::bail!(
                "registry returned 403 Forbidden — your account may be inactive or \
                 lack permission for import; contact your IT administrator"
            );
        }
        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            let body_text = resp.text().unwrap_or_default();
            anyhow::bail!("registry rejected import submission (422): {body_text}");
        }
        if !status.is_success() {
            let body_text = resp.text().unwrap_or_default();
            anyhow::bail!("registry returned HTTP {status} for import submit: {body_text}");
        }

        let wire: ImportSubmitResponse = resp
            .json()
            .context("failed to deserialize import submit response")?;

        map_submit_response_to_outcome(wire)
    }

    /// Upload a batch of unsynced skill ratings to the registry.
    ///
    /// Calls `POST /api/runner/skill-ratings`. Best-effort: callers should log
    /// on error and continue; the rows remain in SQLite for the next tick.
    ///
    /// TODO(registry): add matching POST /api/runner/skill-ratings endpoint.
    pub fn upload_skill_ratings(&self, ratings: &[crate::ratings::LocalRating]) -> Result<()> {
        if ratings.is_empty() {
            return Ok(());
        }

        let url = format!(
            "{}/api/runner/skill-ratings",
            self.base_url.trim_end_matches('/')
        );
        debug!(url, count = ratings.len(), "uploading skill ratings");

        let payload: Vec<serde_json::Value> = ratings
            .iter()
            .map(|r| {
                serde_json::json!({
                    "skill_id":  r.skill_id,
                    "version":   r.version,
                    "rating":    r.rating,
                    "rated_at":  r.rated_at,
                })
            })
            .collect();

        let mut req = self.http.post(&url).json(&payload);
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }

        let resp = req
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("skill ratings upload failed (HTTP {status}): {body}");
        }

        Ok(())
    }

    /// Upload execution statistics for all tracked skills to the registry.
    ///
    /// Calls `POST /api/runner/execution-stats`. Best-effort: callers should log
    /// on error and continue.
    ///
    /// TODO(registry): add matching POST /api/runner/execution-stats endpoint.
    pub fn upload_execution_stats(&self, stats: &[crate::ratings::ExecutionStats]) -> Result<()> {
        if stats.is_empty() {
            return Ok(());
        }

        let url = format!(
            "{}/api/runner/execution-stats",
            self.base_url.trim_end_matches('/')
        );
        debug!(url, count = stats.len(), "uploading execution stats");

        let mut req = self.http.post(&url).json(stats);
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }

        let resp = req
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("execution stats upload failed (HTTP {status}): {body}");
        }

        Ok(())
    }
}

// ── Compatibility: SkillPolicyResponse used by tests / M0 consumers ──────────

/// Policy for a single skill as returned by the raw policy endpoint.
#[derive(Debug, Serialize, Deserialize)]
pub struct SkillPolicyResponse {
    pub skill_id: String,
    /// `"active"` or `"blocked"`.
    pub status: String,
    pub minimum_allowed_version: Option<String>,
    pub blocked_message: Option<String>,
}

// ── MockRegistryClient ────────────────────────────────────────────────────────

/// A registry client that returns canned responses. Used in tests and by any
/// code path that doesn't have a live registry connection.
pub struct MockRegistryClient {
    pub base_url: String,
}

impl MockRegistryClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }

    /// Convenience delegates matching the trait surface so tests can use either.
    pub fn fetch_approved_servers(&self) -> Result<ApprovedServersResponse> {
        Ok(ApprovedServersResponse { servers: vec![] })
    }

    pub fn fetch_skill_policy(&self, skill_id: &str) -> Result<SkillPolicyResponse> {
        Ok(SkillPolicyResponse {
            skill_id: skill_id.to_string(),
            status: "active".to_string(),
            minimum_allowed_version: None,
            blocked_message: None,
        })
    }

    pub fn upload_audit_batch(&self, _events: &[AuditEventPayload]) -> Result<()> {
        Ok(())
    }

    pub fn health_check(&self) -> Result<bool> {
        Ok(false)
    }

    pub fn fetch_skill_detail(&self, skill_id: &str) -> Result<SkillDetail> {
        Ok(SkillDetail {
            skill_id: skill_id.to_string(),
            name: skill_id.to_string(),
            latest_version: None,
            publisher_name: None,
            description: None,
        })
    }

    pub fn fetch_artifact_metadata(
        &self,
        skill_id: &str,
        version: &str,
    ) -> Result<ArtifactMetadata> {
        anyhow::bail!(
            "MockRegistryClient.fetch_artifact_metadata not implemented for {skill_id}@{version}"
        )
    }

    pub fn download_artifact(
        &self,
        download_url: &str,
        _expected_sha256: &str,
        _dest: &Utf8Path,
    ) -> Result<()> {
        anyhow::bail!("MockRegistryClient.download_artifact not implemented for {download_url}")
    }

    pub fn check_skill_status(&self, _skill_ids: &[String]) -> Result<SkillStatusResponse> {
        Ok(SkillStatusResponse {
            statuses: std::collections::HashMap::new(),
            unknown: vec![],
        })
    }

    pub fn upload_skill_ratings(&self, _ratings: &[crate::ratings::LocalRating]) -> Result<()> {
        Ok(())
    }

    pub fn upload_execution_stats(&self, _stats: &[crate::ratings::ExecutionStats]) -> Result<()> {
        Ok(())
    }
}

// ── HttpPolicyClient ──────────────────────────────────────────────────────────

/// A `PolicyClient` that fetches from the registry and caches results in
/// the local SQLite `policy_cache` table.
///
/// On network failure it falls back to the cached policy if one exists,
/// implementing the spec's 7-day offline grace window.
pub struct HttpPolicyClient {
    registry: RegistryClient,
    db_path: camino::Utf8PathBuf,
}

impl HttpPolicyClient {
    pub fn new(registry: RegistryClient, state: &AppState) -> Self {
        Self {
            registry,
            db_path: state.db_path.clone(),
        }
    }

    /// Delete the cached policy for `skill_id` so the next `fetch_policy` call
    /// re-fetches from the registry. Best-effort: callers should ignore errors.
    pub fn invalidate(&self, skill_id: &str) -> Result<()> {
        let conn = Connection::open(&self.db_path)?;
        conn.execute(
            "DELETE FROM policy_cache WHERE skill_id = ?1",
            rusqlite::params![skill_id],
        )?;
        Ok(())
    }
}

impl PolicyClient for HttpPolicyClient {
    fn fetch_policy(&self, skill_id: &str) -> Result<Policy> {
        let now = unix_now();
        let conn = Connection::open(&self.db_path).context("failed to open state DB")?;

        // Always try registry first so policy changes take effect immediately.
        match self.registry.fetch_policy_remote(skill_id) {
            Ok((policy, ttl)) => {
                let wire = policy_to_wire(&policy, ttl);
                let json = serde_json::to_string(&wire).context("failed to serialize policy")?;
                let expires_at = now + ttl;

                conn.execute(
                    "INSERT INTO policy_cache (skill_id, policy_json, expires_at, fetched_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(skill_id) DO UPDATE SET
                         policy_json = excluded.policy_json,
                         expires_at  = excluded.expires_at,
                         fetched_at  = excluded.fetched_at",
                    params![skill_id, json, expires_at as i64, now as i64],
                )
                .context("failed to write policy cache")?;

                Ok(policy)
            }
            Err(fetch_err) => {
                // Fallback: use cached policy within 7-day offline grace window.
                let cached = conn
                    .query_row(
                        "SELECT policy_json, fetched_at FROM policy_cache WHERE skill_id = ?1",
                        [skill_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()?;

                if let Some((json, fetched_at)) = cached {
                    const GRACE_SECONDS: u64 = 7 * 86400;
                    let within_grace = now < fetched_at as u64 + GRACE_SECONDS;

                    if within_grace {
                        warn!(
                            skill_id,
                            error = %fetch_err,
                            "policy fetch failed; using stale cache within 7-day grace window"
                        );
                        let wire: PolicyApiResponse = serde_json::from_str(&json)
                            .context("failed to deserialize stale cached policy")?;
                        return policy_from_wire(wire);
                    }
                }

                Err(fetch_err.context(format!("failed to fetch policy for '{skill_id}'")))
            }
        }
    }
}

// ── Helper functions ──────────────────────────────────────────────────────────

fn policy_from_wire(wire: PolicyApiResponse) -> Result<Policy> {
    let status = match wire.status.as_str() {
        "active" => PolicyStatus::Active,
        "blocked" => PolicyStatus::Blocked,
        other => anyhow::bail!("unknown policy status '{other}'"),
    };

    let target_version = wire
        .target_version
        .as_deref()
        .map(Version::parse)
        .transpose()
        .with_context(|| format!("invalid target_version in policy for '{}'", wire.skill_id))?;

    let minimum_allowed_version = wire
        .minimum_allowed_version
        .as_deref()
        .map(Version::parse)
        .transpose()
        .with_context(|| {
            format!(
                "invalid minimum_allowed_version in policy for '{}'",
                wire.skill_id
            )
        })?;

    Ok(Policy {
        skill_id: wire.skill_id,
        status,
        target_version,
        minimum_allowed_version,
        blocked_message: wire.blocked_message,
    })
}

fn policy_to_wire(policy: &Policy, ttl: u64) -> PolicyApiResponse {
    PolicyApiResponse {
        skill_id: policy.skill_id.clone(),
        status: match policy.status {
            PolicyStatus::Active => "active".to_string(),
            PolicyStatus::Blocked => "blocked".to_string(),
        },
        channel: None,
        target_version: policy.target_version.as_ref().map(|v| v.to_string()),
        minimum_allowed_version: policy
            .minimum_allowed_version
            .as_ref()
            .map(|v| v.to_string()),
        blocked_message: policy.blocked_message.clone(),
        policy_ttl_seconds: Some(ttl),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_secs()
}

fn map_submit_response_to_outcome(wire: ImportSubmitResponse) -> Result<ImportOutcome> {
    match wire.outcome_type.as_str() {
        "skill_scaffolded" => {
            let path = wire
                .bundle_path
                .ok_or_else(|| anyhow::anyhow!("skill_scaffolded outcome missing bundle_path"))?;
            Ok(ImportOutcome::SkillScaffolded {
                bundle: camino::Utf8PathBuf::from(path),
            })
        }
        "mcp_server_requested" => {
            let server_name = wire.server_name.ok_or_else(|| {
                anyhow::anyhow!("mcp_server_requested outcome missing server_name")
            })?;
            Ok(ImportOutcome::McpServerRequested {
                server_name,
                status: wire.status.unwrap_or_else(|| "pending".to_string()),
            })
        }
        "skill_submitted" => {
            let submission_id = wire
                .submission_id
                .ok_or_else(|| anyhow::anyhow!("skill_submitted outcome missing submission_id"))?;
            Ok(ImportOutcome::SkillSubmitted {
                submission_id,
                status: wire.status.unwrap_or_else(|| "pending".to_string()),
            })
        }
        other => anyhow::bail!("unknown import outcome type from registry: '{other}'"),
    }
}

/// Parse a 422 compile response body into a human-readable error string.
fn parse_compile_errors(body: &str) -> String {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };

    let detail = &val["detail"];
    let top_msg = detail["message"].as_str().unwrap_or("validation failed");

    let errors = detail["errors"].as_array();
    let Some(errors) = errors else {
        return detail.as_str().unwrap_or(body).to_string();
    };

    if errors.is_empty() {
        return top_msg.to_string();
    }

    let mut lines = vec![top_msg.to_string()];
    for e in errors {
        let path = e["path"].as_str().unwrap_or("<root>");
        let msg = e["message"].as_str().unwrap_or("unknown error");
        lines.push(format!("  {path}: {msg}"));
    }
    lines.join("\n")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use camino::Utf8PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("vh-tests-registry-{label}-{nanos}")),
        )
        .unwrap()
    }

    #[test]
    fn mock_registry_returns_empty_approved_servers() {
        let client = MockRegistryClient::new("http://localhost:8000");
        let resp = client.fetch_approved_servers().unwrap();
        assert!(resp.servers.is_empty());
    }

    #[test]
    fn mock_registry_returns_active_skill_policy() {
        let client = MockRegistryClient::new("http://localhost:8000");
        let policy = client.fetch_skill_policy("test-skill").unwrap();
        assert_eq!(policy.skill_id, "test-skill");
        assert_eq!(policy.status, "active");
    }

    #[test]
    fn mock_registry_upload_audit_batch_succeeds() {
        let client = MockRegistryClient::new("http://localhost:8000");
        let events = vec![AuditEventPayload {
            event_type: "tool_called".to_string(),
            payload: serde_json::json!({"tool": "github__create_issue"}),
            created_at: 1_700_000_000,
        }];
        assert!(client.upload_audit_batch(&events).is_ok());
    }

    #[test]
    fn mock_registry_health_check_returns_false() {
        let client = MockRegistryClient::new("http://localhost:8000");
        assert!(!client.health_check().unwrap());
    }

    // ── RegistryClient: fetch_policy_remote ────────────────────────────────

    #[test]
    fn fetch_policy_remote_parses_active_policy() {
        use mockito::Server;
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/skills/my-skill/policy")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "skill_id": "my-skill",
                    "status": "active",
                    "target_version": "1.2.0",
                    "minimum_allowed_version": "1.0.0",
                    "policy_ttl_seconds": 3600
                }"#,
            )
            .create();

        let client = RegistryClient::new(server.url());
        let (policy, ttl) = client.fetch_policy_remote("my-skill").unwrap();

        assert_eq!(policy.skill_id, "my-skill");
        assert_eq!(policy.status, PolicyStatus::Active);
        assert_eq!(
            policy.target_version,
            Some(Version::parse("1.2.0").unwrap())
        );
        assert_eq!(
            policy.minimum_allowed_version,
            Some(Version::parse("1.0.0").unwrap())
        );
        assert_eq!(ttl, 3600);
        mock.assert();
    }

    #[test]
    fn fetch_policy_remote_parses_blocked_policy() {
        use mockito::Server;
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/skills/bad-skill/policy")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "skill_id": "bad-skill",
                    "status": "blocked",
                    "blocked_message": "security vulnerability"
                }"#,
            )
            .create();

        let client = RegistryClient::new(server.url());
        let (policy, ttl) = client.fetch_policy_remote("bad-skill").unwrap();

        assert_eq!(policy.status, PolicyStatus::Blocked);
        assert_eq!(
            policy.blocked_message.as_deref(),
            Some("security vulnerability")
        );
        assert_eq!(ttl, 86400); // default TTL
        mock.assert();
    }

    #[test]
    fn fetch_policy_remote_errors_on_non_200() {
        use mockito::Server;
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/skills/ghost-skill/policy")
            .with_status(404)
            .with_body("not found")
            .create();

        let client = RegistryClient::new(server.url());
        let result = client.fetch_policy_remote("ghost-skill");
        assert!(result.is_err());
        mock.assert();
    }

    #[test]
    fn health_check_returns_true_on_200() {
        use mockito::Server;
        let mut server = Server::new();
        let mock = server.mock("GET", "/health").with_status(200).create();

        let client = RegistryClient::new(server.url());
        assert!(client.health_check().unwrap());
        mock.assert();
    }

    #[test]
    fn search_skills_parses_items() {
        use mockito::Server;
        let mut server = Server::new();
        let mock = server
            .mock("GET", mockito::Matcher::Regex(r"/portal/skills".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"items":[{"skill_id":"contract-compare","name":"Contract Compare","latest_version":"0.2.0","publisher_name":null,"description":null}]}"#)
            .create();

        let client = RegistryClient::new(server.url());
        let results = client.search_skills("contract").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].skill_id, "contract-compare");
        mock.assert();
    }

    // ── HttpPolicyClient: 7-day offline grace ─────────────────────────────────

    #[test]
    fn http_policy_client_caches_fetched_policy() {
        use mockito::Server;
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/skills/cached-skill/policy")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"skill_id":"cached-skill","status":"active","policy_ttl_seconds":3600}"#)
            .create();

        let root = temp_root("http-policy-cache");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let registry = RegistryClient::new(server.url());
        let policy_client = HttpPolicyClient::new(registry, &state);

        let policy = policy_client.fetch_policy("cached-skill").unwrap();
        assert_eq!(policy.status, PolicyStatus::Active);

        // Verify the cache row was written.
        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM policy_cache WHERE skill_id = 'cached-skill'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "policy should be cached in SQLite");

        mock.assert();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn http_policy_client_uses_cache_when_registry_unreachable() {
        let root = temp_root("http-policy-grace");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        // Pre-seed the cache with a fresh entry.
        let now = unix_now();
        let wire = PolicyApiResponse {
            skill_id: "stale-skill".to_string(),
            status: "active".to_string(),
            channel: None,
            target_version: None,
            minimum_allowed_version: None,
            blocked_message: None,
            policy_ttl_seconds: Some(3600),
        };
        let json = serde_json::to_string(&wire).unwrap();
        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        conn.execute(
            "INSERT INTO policy_cache (skill_id, policy_json, expires_at, fetched_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["stale-skill", json, now as i64 + 3600, now as i64],
        )
        .unwrap();
        drop(conn);

        // Point at a non-listening port so the registry fetch fails.
        let registry = RegistryClient::new("http://127.0.0.1:1");
        let policy_client = HttpPolicyClient::new(registry, &state);

        let policy = policy_client.fetch_policy("stale-skill").unwrap();
        assert_eq!(
            policy.status,
            PolicyStatus::Active,
            "should use cached policy when registry unreachable"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn http_policy_client_rejects_cache_older_than_7_days() {
        let root = temp_root("http-policy-expired");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        // Seed cache with a fetched_at more than 7 days ago.
        let now = unix_now();
        let ancient = now - (8 * 86400); // 8 days ago
        let wire = PolicyApiResponse {
            skill_id: "old-skill".to_string(),
            status: "active".to_string(),
            channel: None,
            target_version: None,
            minimum_allowed_version: None,
            blocked_message: None,
            policy_ttl_seconds: Some(3600),
        };
        let json = serde_json::to_string(&wire).unwrap();
        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        conn.execute(
            "INSERT INTO policy_cache (skill_id, policy_json, expires_at, fetched_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["old-skill", json, ancient as i64 + 3600, ancient as i64],
        )
        .unwrap();
        drop(conn);

        // Registry unreachable.
        let registry = RegistryClient::new("http://127.0.0.1:1");
        let policy_client = HttpPolicyClient::new(registry, &state);

        let result = policy_client.fetch_policy("old-skill");
        assert!(
            result.is_err(),
            "should fail when cache is older than 7-day grace window"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
