#![allow(clippy::unwrap_used)]
use super::*;

#[test]
fn accepts_plain_names() {
    for ok in ["foo", "a-b_c.d", "1.2.3", "Skill-9", "a..b"] {
        validate_component("t", ok).unwrap();
    }
}

#[test]
fn rejects_traversal_and_absolute() {
    for bad in [
        "",
        ".",
        "..",
        "../x",
        "/abs",
        "a/b",
        "a\\b",
        ".hidden",
        "a b",
        "a\0b",
        "é",
        "x:y",
        "~",
        "../../Documents/x",
    ] {
        assert!(validate_component("t", bad).is_err(), "{bad:?} should fail");
    }
    assert!(validate_component("t", &"a".repeat(129)).is_err());
    assert!(validate_component("t", &"a".repeat(128)).is_ok());
}

#[test]
fn private_skill_registry_ids_with_a_tilde_are_one_component() {
    assert!(validate_component("t", "adam~pdf").is_ok());
    assert!(
        validate_component("t", "~pdf").is_err(),
        "leading ~ stays refused"
    );
    assert!(validate_component("t", "a~/b").is_err());
    assert!(safe_join(Path::new("/r/skills"), "adam~pdf").is_ok());
}

#[test]
fn safe_join_stays_under_root() {
    let root = Path::new("/r/skills");
    assert_eq!(safe_join(root, "x").unwrap(), Path::new("/r/skills/x"));
    assert!(safe_join(root, "/etc").is_err());
    assert!(safe_join(root, "..").is_err());
}

#[test]
fn safe_join_rel_requires_normal_components() {
    let root = Path::new("/r/plug");
    assert_eq!(
        safe_join_rel(root, "a/b.txt").unwrap(),
        Path::new("/r/plug/a/b.txt")
    );
    assert!(safe_join_rel(root, "a..b/c").is_ok());
    assert!(safe_join_rel(root, ".claude-plugin/plugin.json").is_ok());
    for bad in [
        "/Users/u/.zshrc",
        "../x",
        "a/../../x",
        "./a",
        "",
        "a\\b",
        "a/./b",
    ] {
        assert!(safe_join_rel(root, bad).is_err(), "{bad:?}");
    }
}

#[test]
fn encode_segment_escapes_slashes() {
    assert_eq!(encode_segment("a/b c"), "a%2Fb%20c");
}

#[test]
fn semver_build_metadata_versions_are_one_component() {
    for v in [
        "0.1.0+e80735de6655",
        "0.1.1+local.abc123",
        "1.0.0-rc.1+build.5",
    ] {
        validate_component("version", v).unwrap();
        let root = std::path::Path::new("/r");
        assert_eq!(safe_join(root, v).unwrap(), root.join(v));
    }
    assert!(validate_component("version", "+x").is_ok());
    assert!(validate_component("version", "0.1.0+a/b").is_err());
    assert!(validate_component("version", "0.1.0 +a").is_err());
    assert_eq!(encode_segment("0.1.0+abc"), "0.1.0%2Babc");
}
