#![allow(clippy::unwrap_used)]
use super::*;

fn pending(name: &str, hash: &str) -> PendingServer {
    PendingServer {
        mcp_server_id: format!("id-{name}"),
        installation_id: format!("inst-{name}"),
        name: name.into(),
        approval_hash: hash.into(),
        command: "npx".into(),
        args: vec!["-y".into(), "pkg".into()],
        env_keys: vec!["TOKEN".into()],
        signer_kid: Some("scn-dev-a".into()),
    }
}

#[test]
fn confirm_accepts_only_an_explicit_yes() {
    for (input, want) in [
        ("y\n", true),
        ("YES\n", true),
        ("n\n", false),
        ("\n", false),
        ("maybe\n", false),
        ("", false),
    ] {
        let mut out = Vec::new();
        let got = confirm_with(&mut input.as_bytes(), &mut out, "detail", "Run?").unwrap();
        assert_eq!(got, want, "{input:?}");
        assert!(String::from_utf8(out).unwrap().contains("Run? [y/N]"));
    }
}

#[test]
fn select_by_name_or_hash_prefix_and_ambiguity() {
    let list = vec![
        pending("GitHub", "abcdef1234"),
        pending("Slack", "abcd999999"),
        pending("Jira", "ffee001122"),
    ];
    assert_eq!(select_pending(&list, "github").unwrap().name, "GitHub");
    assert_eq!(select_pending(&list, "ffee").unwrap().name, "Jira");
    assert!(select_pending(&list, "abcd").is_err(), "ambiguous prefix");
    assert!(
        select_pending(&list, "abc").is_err(),
        "too short to be a hash prefix"
    );
    assert!(select_pending(&list, "nope").is_err());
    assert!(select_pending(&list, "").is_err());
}

#[test]
fn describe_shows_command_args_env_keys_signer_and_full_hash() {
    let d = describe(&pending("GitHub", "abcdef1234"));
    for needle in [
        "GitHub",
        "npx",
        "\"-y\" \"pkg\"",
        "TOKEN",
        "scn-dev-a",
        "abcdef1234",
    ] {
        assert!(d.contains(needle), "{needle} missing from:\n{d}");
    }
}
