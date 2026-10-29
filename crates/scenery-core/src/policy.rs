use anyhow::Result;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Central policy for a single skill as returned by the policy authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub skill_id: String,
    pub status: PolicyStatus,
    /// The approved version to install/run on this channel.
    pub target_version: Option<Version>,
    /// Installed versions strictly below this must be updated before execution.
    pub minimum_allowed_version: Option<Version>,
    /// Human-readable reason shown when status is `Blocked`.
    pub blocked_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PolicyStatus {
    Active,
    Blocked,
}

impl Policy {
    /// Convenience constructor: active policy with no version constraints.
    pub fn default_active(skill_id: &str) -> Self {
        Self {
            skill_id: skill_id.to_string(),
            status: PolicyStatus::Active,
            target_version: None,
            minimum_allowed_version: None,
            blocked_message: None,
        }
    }
}

/// Abstraction over the source of skill policy (registry HTTP, local file, mock).
///
/// The daemon holds an `HttpPolicyClient` (ported in M1). The M0 daemon uses
/// `MockPolicyClient` (allow-all) until the registry sync is ported.
pub trait PolicyClient {
    fn fetch_policy(&self, skill_id: &str) -> Result<Policy>;
}

/// In-process mock for tests and allow-all / offline use.
///
/// Returns a default active policy for any skill not explicitly registered.
/// Use [`MockPolicyClient::with_policy`] to inject specific scenarios in tests.
pub struct MockPolicyClient {
    overrides: HashMap<String, Policy>,
}

impl Default for MockPolicyClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MockPolicyClient {
    pub fn new() -> Self {
        Self {
            overrides: HashMap::new(),
        }
    }

    /// Register a policy that will be returned for `policy.skill_id`.
    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.overrides.insert(policy.skill_id.clone(), policy);
        self
    }
}

impl PolicyClient for MockPolicyClient {
    fn fetch_policy(&self, skill_id: &str) -> Result<Policy> {
        Ok(self
            .overrides
            .get(skill_id)
            .cloned()
            .unwrap_or_else(|| Policy::default_active(skill_id)))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_returns_default_active_for_unregistered_skill() {
        let client = MockPolicyClient::new();
        let policy = client.fetch_policy("unknown-skill").unwrap();
        assert_eq!(policy.status, PolicyStatus::Active);
        assert_eq!(policy.skill_id, "unknown-skill");
    }

    #[test]
    fn mock_returns_registered_policy_override() {
        let blocked = Policy {
            skill_id: "my-skill".to_string(),
            status: PolicyStatus::Blocked,
            target_version: None,
            minimum_allowed_version: None,
            blocked_message: Some("revoked".to_string()),
        };
        let client = MockPolicyClient::new().with_policy(blocked);
        let policy = client.fetch_policy("my-skill").unwrap();
        assert_eq!(policy.status, PolicyStatus::Blocked);
        assert_eq!(policy.blocked_message.as_deref(), Some("revoked"));
    }

    #[test]
    fn mock_default_active_policy_fields() {
        let p = Policy::default_active("test-skill");
        assert_eq!(p.skill_id, "test-skill");
        assert_eq!(p.status, PolicyStatus::Active);
        assert!(p.target_version.is_none());
        assert!(p.minimum_allowed_version.is_none());
        assert!(p.blocked_message.is_none());
    }
}
