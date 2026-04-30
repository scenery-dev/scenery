use crate::{
    installer::InstallScope,
    policy::{PolicyClient, PolicyStatus},
    state::AppState,
};
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::{Connection, OptionalExtension};
use semver::Version;
use vectorhawkd_manifest::SkillPackage;

#[derive(Debug, PartialEq)]
pub enum ResolveOutcome {
    /// Skill is installed and policy permits execution.
    Active {
        skill_id: String,
        version: String,
        install_path: String,
        scope: InstallScope,
    },
    /// Policy has blocked this skill (or no valid replacement exists).
    Blocked { skill_id: String, reason: String },
    /// Skill has never been installed locally.
    NotInstalled { skill_id: String },
}

/// Resolve `skill_id` to a runnable path or a block/not-installed reason.
///
/// Resolution order:
/// 1. If `project_root` is `Some`, check `.vectorhawk/skills/{skill_id}/SKILL.md`.
///    If present, load the bundle and return `Active` with `scope = Project(...)` — no policy check.
/// 2. Check user-scope SQLite — not found → `NotInstalled`.
/// 3. Fetch policy — `Blocked` status → `Blocked`.
/// 4. If `minimum_allowed_version` is set and the installed version is below
///    it, execution is blocked until an update installs the target version.
/// 5. Otherwise → `Active` with `scope = User`.
pub fn resolve_skill(
    state: &AppState,
    policy_client: &dyn PolicyClient,
    skill_id: &str,
    project_root: Option<&Utf8Path>,
) -> Result<ResolveOutcome> {
    if let Some(root) = project_root {
        let project_skill_dir = root.join(".vectorhawk").join("skills").join(skill_id);
        if project_skill_dir.join("SKILL.md").exists() {
            let pkg = SkillPackage::load_from_dir(&project_skill_dir).with_context(|| {
                format!("failed to load project-scope skill '{skill_id}' at {project_skill_dir}")
            })?;
            return Ok(ResolveOutcome::Active {
                skill_id: skill_id.to_string(),
                version: pkg.manifest.version.to_string(),
                install_path: project_skill_dir.to_string(),
                scope: InstallScope::Project(Utf8PathBuf::from(root)),
            });
        }
    }

    let conn = Connection::open(&state.db_path)?;

    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT active_version, install_root \
             FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let (active_version_str, install_root) = match row {
        None => {
            return Ok(ResolveOutcome::NotInstalled {
                skill_id: skill_id.to_string(),
            })
        }
        Some(r) => r,
    };

    let policy = policy_client.fetch_policy(skill_id)?;

    if policy.status == PolicyStatus::Blocked {
        return Ok(ResolveOutcome::Blocked {
            skill_id: skill_id.to_string(),
            reason: policy
                .blocked_message
                .unwrap_or_else(|| "This skill is temporarily unavailable.".to_string()),
        });
    }

    if let Some(min_ver) = policy.minimum_allowed_version {
        let installed = Version::parse(&active_version_str).map_err(|e| {
            anyhow::anyhow!(
                "installed version '{}' is not valid semver: {e}",
                active_version_str
            )
        })?;
        if installed < min_ver {
            return Ok(ResolveOutcome::Blocked {
                skill_id: skill_id.to_string(),
                reason: format!(
                    "Installed version {installed} is below the minimum allowed version \
                     {min_ver}. Run `vectorhawk skill install` to update.",
                ),
            });
        }
    }

    Ok(ResolveOutcome::Active {
        skill_id: skill_id.to_string(),
        version: active_version_str,
        install_path: format!("{}/active", install_root),
        scope: InstallScope::User,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        policy::{MockPolicyClient, Policy, PolicyStatus},
        state::AppState,
    };
    use camino::Utf8PathBuf;
    use rusqlite::{params, Connection};
    use semver::Version;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("vh-resolver-tests-{label}-{nanos}"));
        Utf8PathBuf::from_path_buf(path).expect("temporary test path should be utf-8")
    }

    fn seed_installed(conn: &Connection, skill_id: &str, version: &str, install_root: &str) {
        conn.execute(
            "INSERT INTO installed_skills \
             (skill_id, active_version, install_root, channel, current_status) \
             VALUES (?1, ?2, ?3, 'stable', 'active')",
            params![skill_id, version, install_root],
        )
        .expect("seed row should insert");
    }

    #[test]
    fn resolve_returns_active_for_installed_skill_with_permissive_policy() {
        let root = temp_root("active");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");
        let conn = Connection::open(&state.db_path).expect("open db");
        seed_installed(
            &conn,
            "contract-compare",
            "1.0.0",
            "/fake/skills/contract-compare",
        );

        let client = MockPolicyClient::new();
        let outcome = resolve_skill(&state, &client, "contract-compare", None).expect("resolve");

        assert_eq!(
            outcome,
            ResolveOutcome::Active {
                skill_id: "contract-compare".to_string(),
                version: "1.0.0".to_string(),
                install_path: "/fake/skills/contract-compare/active".to_string(),
                scope: InstallScope::User,
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_returns_not_installed_for_unknown_skill() {
        let root = temp_root("missing");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        let client = MockPolicyClient::new();
        let outcome = resolve_skill(&state, &client, "no-such-skill", None).expect("resolve");

        assert_eq!(
            outcome,
            ResolveOutcome::NotInstalled {
                skill_id: "no-such-skill".to_string(),
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_returns_blocked_when_policy_says_blocked() {
        let root = temp_root("policy-blocked");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");
        let conn = Connection::open(&state.db_path).expect("open db");
        seed_installed(
            &conn,
            "contract-compare",
            "1.0.0",
            "/fake/skills/contract-compare",
        );

        let blocked_policy = Policy {
            skill_id: "contract-compare".to_string(),
            status: PolicyStatus::Blocked,
            target_version: None,
            minimum_allowed_version: None,
            blocked_message: Some("Revoked by publisher.".to_string()),
        };
        let client = MockPolicyClient::new().with_policy(blocked_policy);
        let outcome = resolve_skill(&state, &client, "contract-compare", None).expect("resolve");

        assert_eq!(
            outcome,
            ResolveOutcome::Blocked {
                skill_id: "contract-compare".to_string(),
                reason: "Revoked by publisher.".to_string(),
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_returns_blocked_when_installed_version_below_minimum() {
        let root = temp_root("below-minimum");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");
        let conn = Connection::open(&state.db_path).expect("open db");
        seed_installed(
            &conn,
            "contract-compare",
            "1.0.0",
            "/fake/skills/contract-compare",
        );

        let policy = Policy {
            skill_id: "contract-compare".to_string(),
            status: PolicyStatus::Active,
            target_version: Some(Version::parse("1.1.0").expect("semver")),
            minimum_allowed_version: Some(Version::parse("1.1.0").expect("semver")),
            blocked_message: None,
        };
        let client = MockPolicyClient::new().with_policy(policy);
        let outcome = resolve_skill(&state, &client, "contract-compare", None).expect("resolve");

        match outcome {
            ResolveOutcome::Blocked { skill_id, reason } => {
                assert_eq!(skill_id, "contract-compare");
                assert!(
                    reason.contains("1.0.0"),
                    "reason should mention installed version"
                );
                assert!(
                    reason.contains("1.1.0"),
                    "reason should mention minimum version"
                );
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_returns_active_when_installed_version_meets_minimum() {
        let root = temp_root("meets-minimum");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");
        let conn = Connection::open(&state.db_path).expect("open db");
        seed_installed(
            &conn,
            "contract-compare",
            "1.1.0",
            "/fake/skills/contract-compare",
        );

        let policy = Policy {
            skill_id: "contract-compare".to_string(),
            status: PolicyStatus::Active,
            target_version: Some(Version::parse("1.1.0").expect("semver")),
            minimum_allowed_version: Some(Version::parse("1.1.0").expect("semver")),
            blocked_message: None,
        };
        let client = MockPolicyClient::new().with_policy(policy);
        let outcome = resolve_skill(&state, &client, "contract-compare", None).expect("resolve");

        assert_eq!(
            outcome,
            ResolveOutcome::Active {
                skill_id: "contract-compare".to_string(),
                version: "1.1.0".to_string(),
                install_path: "/fake/skills/contract-compare/active".to_string(),
                scope: InstallScope::User,
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
