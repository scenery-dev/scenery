#![allow(clippy::unwrap_used)]
use super::*;

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
fn verified_stdio_is_allowed_with_no_approval() {
    let r = row(
        r#"{"command":"npx","args":["-y","pkg"],"env":{"TOKEN":"x"}}"#,
        None,
    );
    assert!(stdio_definition(&r).is_some());
    assert_eq!(check(&r), Gate::Allowed);
}

#[test]
fn denylisted_env_is_refused() {
    for key in [
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "NODE_OPTIONS",
        "PATH",
    ] {
        let r = row(
            &format!(r#"{{"command":"npx","args":[],"env":{{"{key}":"/x"}}}}"#),
            None,
        );
        assert_eq!(
            check(&r),
            Gate::Refused(format!("env key {key} not allowed"))
        );
    }
}

#[test]
fn http_and_gateway_rows_are_allowed() {
    assert_eq!(
        check(&row(r#"{"url":"https://x.example/mcp"}"#, None)),
        Gate::Allowed
    );
    assert_eq!(
        check(&row(
            r#"{"command":"npx","args":[]}"#,
            Some("https://app.scenery.dev/gw/1")
        )),
        Gate::Allowed
    );
}
