#![allow(clippy::unwrap_used)]
use super::*;
use camino::Utf8PathBuf;

fn state() -> (tempfile::TempDir, AppState) {
    let d = tempfile::tempdir().unwrap();
    let st = AppState::bootstrap_in(Utf8PathBuf::from_path_buf(d.path().to_path_buf()).unwrap())
        .unwrap();
    (d, st)
}

fn row(cfg: &str, gateway: Option<&str>) -> McpInstallRow {
    McpInstallRow {
        mcp_server_id: "id-1".into(),
        installation_id: "inst-1".into(),
        mcp_server_name: "Srv".into(),
        package_source: "npm".into(),
        version_pin: None,
        server_config: Some(cfg.into()),
        auth_type: "none".into(),
        gateway_server_id: None,
        gateway_url: gateway.map(str::to_string),
    }
}

#[test]
fn unapproved_stdio_is_pending_then_allowed_after_approval() {
    let (_d, st) = state();
    let r = row(r#"{"command":"npx","args":["-y","pkg"],"env":{"TOKEN":"x"}}"#, None);
    let def = stdio_definition(&r).unwrap();
    assert_eq!(check(&st, &r), Gate::Pending(def.approval_hash()));
    approvals::insert_approval(&st, &def, "Srv").unwrap();
    assert_eq!(check(&st, &r), Gate::Allowed);
}

#[test]
fn args_change_reprompts_env_value_change_does_not() {
    let (_d, st) = state();
    let r1 = row(r#"{"command":"npx","args":["-y","pkg"],"env":{"TOKEN":"a"}}"#, None);
    approvals::insert_approval(&st, &stdio_definition(&r1).unwrap(), "Srv").unwrap();
    let same_keys = row(r#"{"command":"npx","args":["-y","pkg"],"env":{"TOKEN":"b"}}"#, None);
    assert_eq!(check(&st, &same_keys), Gate::Allowed);
    let new_args = row(r#"{"command":"npx","args":["-y","pkg","--evil"],"env":{"TOKEN":"a"}}"#, None);
    assert!(matches!(check(&st, &new_args), Gate::Pending(_)));
    let new_env_key = row(r#"{"command":"npx","args":["-y","pkg"],"env":{"TOKEN":"a","X":"1"}}"#, None);
    assert!(matches!(check(&st, &new_env_key), Gate::Pending(_)));
}

#[test]
fn denylisted_env_is_refused_even_if_approved() {
    let (_d, st) = state();
    let r = row(r#"{"command":"npx","args":[],"env":{"LD_PRELOAD":"/x.so"}}"#, None);
    assert_eq!(check(&st, &r), Gate::Refused("env key LD_PRELOAD not allowed".into()));
}

#[test]
fn http_and_gateway_rows_do_not_prompt() {
    let (_d, st) = state();
    assert_eq!(check(&st, &row(r#"{"url":"https://x.example/mcp"}"#, None)), Gate::Allowed);
    assert_eq!(
        check(&st, &row(r#"{"command":"npx","args":[]}"#, Some("https://app.scenery.dev/gw/1"))),
        Gate::Allowed
    );
}
