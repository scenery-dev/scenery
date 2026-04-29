use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Trait for interacting with the VectorHawk registry API.
///
/// The real HTTP implementation (`HttpRegistryClient`) is wired in M1 when the
/// full registry sync lands. For M0, `MockRegistryClient` returns canned
/// responses so the rest of the codebase can compile and test against the trait.
pub trait RegistryClient: Send + Sync {
    /// Fetch the list of approved backend MCP servers for this device.
    fn fetch_approved_servers(&self) -> Result<ApprovedServersResponse>;

    /// Fetch the current policy for a specific skill.
    fn fetch_skill_policy(&self, skill_id: &str) -> Result<SkillPolicyResponse>;

    /// Report an audit batch to the registry. Returns `Ok(())` on success or
    /// if the registry is unreachable (best-effort, non-blocking for M0).
    fn upload_audit_batch(&self, events: &[AuditEventPayload]) -> Result<()>;

    /// Health check — returns `Ok(true)` if the registry is reachable.
    fn health_check(&self) -> Result<bool>;
}

// ── Wire types ────────────────────────────────────────────────────────────────

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

/// Policy for a single skill as returned by the registry.
#[derive(Debug, Serialize, Deserialize)]
pub struct SkillPolicyResponse {
    pub skill_id: String,
    /// `"active"` or `"blocked"`.
    pub status: String,
    pub minimum_allowed_version: Option<String>,
    pub blocked_message: Option<String>,
}

/// A single audit event ready for upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEventPayload {
    pub event_type: String,
    pub payload: serde_json::Value,
    pub created_at: i64,
}

// ── Mock implementation ───────────────────────────────────────────────────────

/// A registry client that returns canned responses. Used in tests and by the
/// M0 daemon (which doesn't yet have a live registry connection).
pub struct MockRegistryClient {
    /// Base URL stored only for identification in logs.
    pub base_url: String,
}

impl MockRegistryClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl RegistryClient for MockRegistryClient {
    fn fetch_approved_servers(&self) -> Result<ApprovedServersResponse> {
        Ok(ApprovedServersResponse { servers: vec![] })
    }

    fn fetch_skill_policy(&self, skill_id: &str) -> Result<SkillPolicyResponse> {
        Ok(SkillPolicyResponse {
            skill_id: skill_id.to_string(),
            status: "active".to_string(),
            minimum_allowed_version: None,
            blocked_message: None,
        })
    }

    fn upload_audit_batch(&self, _events: &[AuditEventPayload]) -> Result<()> {
        Ok(())
    }

    fn health_check(&self) -> Result<bool> {
        Ok(false)
    }
}

// ── HTTP implementation stub ──────────────────────────────────────────────────

/// HTTP registry client. Full implementation arrives in M1 (registry sync,
/// policy cache, 7-day offline grace). For M0 this struct exists so M1 can
/// fill in method bodies without changing the public interface.
pub struct HttpRegistryClient {
    pub base_url: String,
    client: reqwest::blocking::Client,
}

impl HttpRegistryClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(15))
            // Use rustls for portable TLS without system library deps.
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            base_url: base_url.into(),
            client,
        })
    }
}

impl RegistryClient for HttpRegistryClient {
    fn fetch_approved_servers(&self) -> Result<ApprovedServersResponse> {
        // TODO(M1): implement real HTTP fetch
        anyhow::bail!("HttpRegistryClient.fetch_approved_servers not yet implemented (M1)")
    }

    fn fetch_skill_policy(&self, _skill_id: &str) -> Result<SkillPolicyResponse> {
        // TODO(M1): implement real HTTP fetch with policy cache
        anyhow::bail!("HttpRegistryClient.fetch_skill_policy not yet implemented (M1)")
    }

    fn upload_audit_batch(&self, _events: &[AuditEventPayload]) -> Result<()> {
        // TODO(M1): implement batch upload
        Ok(())
    }

    fn health_check(&self) -> Result<bool> {
        let url = format!("{}/health", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .send()
            .context("registry health check request failed")?;
        Ok(resp.status().is_success())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
}
