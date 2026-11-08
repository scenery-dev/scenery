#![allow(clippy::unwrap_used)]
use super::*;
use camino::Utf8PathBuf;

fn def(args: &[&str], env: &[&str]) -> StdioDefinition {
    StdioDefinition::new(
        "npx".into(),
        args.iter().map(|s| s.to_string()).collect(),
        env.iter().map(|s| s.to_string()).collect(),
    )
}

#[test]
fn hash_is_stable_golden() {
    // Pins the serialization: v, command, args, env_keys in key order.
    let d = def(&["-y", "pkg"], &["B", "A"]);
    assert_eq!(
        d.approval_hash(),
        hex::encode(Sha256::digest(
            br#"{"args":["-y","pkg"],"command":"npx","env_keys":["A","B"],"v":1}"#
        ))
    );
    assert_eq!(d.approval_hash(), def(&["-y", "pkg"], &["A", "B", "A"]).approval_hash());
}

#[test]
fn args_or_env_key_change_changes_hash_but_value_does_not() {
    let a = def(&["-y", "pkg"], &["TOKEN"]);
    assert_ne!(a.approval_hash(), def(&["-y", "pkg2"], &["TOKEN"]).approval_hash());
    assert_ne!(a.approval_hash(), def(&["-y", "pkg"], &["TOKEN", "X"]).approval_hash());
    // Env values never enter the hash: derive from configs differing only in value.
    let c1 = serde_json::json!({"command":"npx","args":["-y","pkg"],"env":{"TOKEN":"one"}});
    let c2 = serde_json::json!({"command":"npx","args":["-y","pkg"],"env":{"TOKEN":"two"}});
    assert_eq!(
        definition_from_config(&c1).unwrap().approval_hash(),
        definition_from_config(&c2).unwrap().approval_hash()
    );
}

#[test]
fn env_denylist() {
    for bad in [
        "LD_PRELOAD", "LD_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES", "PATH", "NODE_OPTIONS",
        "NODE_PATH", "BASH_ENV", "ENV", "PYTHONSTARTUP", "PYTHONPATH", "PERL5OPT", "RUBYOPT",
        "RUBYLIB", "JAVA_TOOL_OPTIONS", "path", "Ld_Preload",
    ] {
        assert!(env_key_denied(bad), "{bad}");
    }
    for ok in ["GITHUB_TOKEN", "HOME", "API_KEY", "ENVIRONMENT", "MY_PATH", "PATHEXT_X"] {
        assert!(!env_key_denied(ok), "{ok}");
    }
    let keys = vec!["OK".to_string(), "LD_PRELOAD".to_string()];
    assert_eq!(first_denied_env_key(&keys).as_deref(), Some("LD_PRELOAD"));
}

#[test]
fn approvals_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let st = AppState::bootstrap_in(Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap())
        .unwrap();
    let d = def(&["-y", "pkg"], &["T"]);
    assert!(!is_approved(&st, &d.approval_hash()).unwrap());
    insert_approval(&st, &d, "srv").unwrap();
    assert!(is_approved(&st, &d.approval_hash()).unwrap());
    let l = list_approvals(&st).unwrap();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].args, d.args);
    assert!(delete_approval(&st, &d.approval_hash()).unwrap());
    assert!(!is_approved(&st, &d.approval_hash()).unwrap());
}
