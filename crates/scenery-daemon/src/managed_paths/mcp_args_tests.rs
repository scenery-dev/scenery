#![allow(clippy::unwrap_used)]

use super::*;

fn golden() -> Vec<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(include_str!("mcp_args_golden.json")).unwrap();
    v["vectors"].as_array().unwrap().clone()
}

fn strings(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

/// What the runner does to a config entry's `command` + `args`.
fn run(command: &str, args: &[String]) -> (String, Vec<String>) {
    let (exe, mut extra) = split_command(command);
    extra.extend(args.iter().cloned());
    let base = std::path::Path::new(&exe)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&exe)
        .to_string();
    let out = redact_args(&base, &extra);
    (base, out)
}

#[test]
fn golden_vectors_match_exactly() {
    let vectors = golden();
    assert!(vectors.len() >= 15);
    for v in vectors {
        let (base, got) = run(v["command"].as_str().unwrap(), &strings(&v["args"]));
        assert_eq!(got, strings(&v["expected"]), "vector {:?}", v["name"]);
        if let Some(want) = v.get("expected_command").and_then(|c| c.as_str()) {
            assert_eq!(base, want, "vector {:?} expected_command", v["name"]);
        }
    }
}

#[test]
fn no_golden_expected_value_contains_a_secret_input() {
    for v in golden() {
        for secret in [
            "opaque", "tok@", "TOKEN=x", "sk-ant", "SECRET", "pw", "ghp_abc", "=abc",
        ] {
            // Inputs whose expected output legitimately keeps the word are not secrets.
            let expected = strings(&v["expected"]).join(" ");
            assert!(
                !expected.contains(secret) || secret == "pw" && expected.contains("pwd"),
                "vector {:?} leaks {secret}: {expected}",
                v["name"]
            );
        }
    }
}

#[test]
fn output_has_one_element_per_input_element() {
    let args: Vec<String> = ["-y", "pkg", "--token", "t", "-pX", "a b"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // `--token t` is two inputs and two outputs; `-pX` and `a b` one each.
    assert_eq!(redact_args("npx", &args).len(), args.len());
}

#[test]
fn p_is_not_a_benign_flag() {
    assert!(!BENIGN_FLAGS.contains(&"-p"));
    assert_eq!(
        redact_args("mysql", &["-p".into(), "hunter2".into()]),
        vec!["-p", REDACTED]
    );
}

#[test]
fn a_command_string_is_split_and_redacted_like_args() {
    let (exe, extra) = split_command("npx -y pkg --api-key sk-ant-abcdefghijklmnopqrstuvwx");
    assert_eq!(exe, "npx");
    assert_eq!(extra.len(), 4);
    let (exe, extra) = split_command("/usr/local/bin/node");
    assert_eq!((exe.as_str(), extra.len()), ("/usr/local/bin/node", 0));
}

#[test]
fn package_refs_split_for_the_registry_fetch() {
    let p = package_of("npx", "@modelcontextprotocol/server-everything@1.2.3").unwrap();
    assert_eq!(
        (p.ecosystem.as_str(), p.name.as_str(), p.version.as_deref()),
        (
            "npm",
            "@modelcontextprotocol/server-everything",
            Some("1.2.3")
        )
    );
    let p = package_of("npx", "@scope/unpinned").unwrap();
    assert_eq!((p.name.as_str(), p.version), ("@scope/unpinned", None));
    let p = package_of("uvx", "mcp-server-git==0.6.2").unwrap();
    assert_eq!(
        (p.ecosystem.as_str(), p.name.as_str(), p.version.as_deref()),
        ("pypi", "mcp-server-git", Some("0.6.2"))
    );
    let p = package_of("docker", "ghcr.io/org/img:1.4").unwrap();
    assert_eq!(
        (p.ecosystem.as_str(), p.name.as_str(), p.version.as_deref()),
        ("docker", "ghcr.io/org/img", Some("1.4"))
    );
    let p = package_of("docker", "localhost:5000/img").unwrap();
    assert_eq!((p.name.as_str(), p.version), ("localhost:5000/img", None));
    assert!(package_of("node", "x").is_none());
}
