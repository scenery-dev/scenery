//! Enterprise managed-deployment config (`managed.json`).
//!
//! When a `managed.json` file exists at a known location and contains
//! `"managed": true`, the runner is in *managed mode*: governance messaging
//! gets stronger, optional org-specific copy is shown, and (later) features
//! like `allow_user_installs` may be enforced. When no managed.json exists
//! or `managed=false`, the runner is in individual-developer mode and
//! `load_managed_config()` returns `None`.

use camino::Utf8PathBuf;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ManagedConfig {
    pub managed: bool,
    pub org: Option<String>,
    pub registry_url: Option<String>,
    pub api_key: Option<String>,
    #[serde(default = "default_true")]
    pub allow_user_installs: bool,
    /// Custom governance message from Corp IT. Replaces the default if set.
    pub governance_message: Option<String>,
    /// Whether to show governance messaging at all. Defaults to true.
    #[serde(default = "default_true")]
    pub governance_message_enabled: bool,
}

fn default_true() -> bool {
    true
}

fn try_load(path: &Utf8PathBuf) -> Option<ManagedConfig> {
    let contents = std::fs::read_to_string(path).ok()?;
    let config: ManagedConfig = serde_json::from_str(&contents).ok()?;
    if !config.managed {
        return None;
    }
    Some(config)
}

/// Load managed deployment config.
///
/// Search order (first hit wins):
/// 1. `/etc/vectorhawk/managed.json` — IT-managed system override (Linux/macOS).
/// 2. `<state.root_dir>/managed.json` — per-user app data dir
///    (`~/Library/Application Support/VectorHawk/managed.json` on macOS,
///    `~/.config/vectorhawk/managed.json` on Linux).
///
/// Returns `None` if no file exists, the JSON is malformed, or the file
/// has `"managed": false`.
pub fn load_managed_config(state: &crate::state::AppState) -> Option<ManagedConfig> {
    let system_path = Utf8PathBuf::from("/etc/vectorhawk/managed.json");
    if let Some(config) = try_load(&system_path) {
        return Some(config);
    }

    let app_path = state.root_dir.join("managed.json");
    try_load(&app_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("vh-managed-test-{name}-{nanos}")),
        )
        .unwrap()
    }

    #[test]
    fn loads_managed_config_from_app_data_dir() {
        let root = temp_root("load");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        let config_json = r#"{
            "managed": true,
            "org": "acme",
            "registry_url": "https://registry.acme.com",
            "api_key": "secret"
        }"#;
        fs::write(state.root_dir.join("managed.json"), config_json).unwrap();

        let config = load_managed_config(&state).expect("should load");
        assert_eq!(config.org.as_deref(), Some("acme"));
        assert_eq!(
            config.registry_url.as_deref(),
            Some("https://registry.acme.com")
        );
        assert_eq!(config.api_key.as_deref(), Some("secret"));
        assert!(config.allow_user_installs);
        assert!(config.governance_message_enabled);
        assert!(config.governance_message.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn managed_false_returns_none() {
        let root = temp_root("false");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        fs::write(
            state.root_dir.join("managed.json"),
            r#"{"managed": false, "org": "acme"}"#,
        )
        .unwrap();

        assert!(load_managed_config(&state).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_file_returns_none() {
        let root = temp_root("missing");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        assert!(load_managed_config(&state).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_json_returns_none() {
        let root = temp_root("invalid");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        fs::write(state.root_dir.join("managed.json"), "not valid json {{{").unwrap();
        assert!(load_managed_config(&state).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn governance_message_fields_load() {
        let root = temp_root("govmsg");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        let config_json = r#"{
            "managed": true,
            "org": "Acme Corp",
            "governance_message": "Contact security@acme.com for tool requests.",
            "governance_message_enabled": true
        }"#;
        fs::write(state.root_dir.join("managed.json"), config_json).unwrap();

        let config = load_managed_config(&state).unwrap();
        assert_eq!(config.org.as_deref(), Some("Acme Corp"));
        assert_eq!(
            config.governance_message.as_deref(),
            Some("Contact security@acme.com for tool requests.")
        );
        assert!(config.governance_message_enabled);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn governance_message_disabled_explicitly() {
        let root = temp_root("govdisabled");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        fs::write(
            state.root_dir.join("managed.json"),
            r#"{"managed": true, "governance_message_enabled": false}"#,
        )
        .unwrap();

        let config = load_managed_config(&state).unwrap();
        assert!(!config.governance_message_enabled);

        let _ = fs::remove_dir_all(&root);
    }
}
