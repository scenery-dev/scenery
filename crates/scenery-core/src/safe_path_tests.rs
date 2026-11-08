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
        "", ".", "..", "../x", "/abs", "a/b", "a\\b", ".hidden", "a b", "a\0b", "é", "x:y",
        "~", "../../Documents/x",
    ] {
        assert!(validate_component("t", bad).is_err(), "{bad:?} should fail");
    }
    assert!(validate_component("t", &"a".repeat(129)).is_err());
    assert!(validate_component("t", &"a".repeat(128)).is_ok());
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
    for bad in ["/Users/u/.zshrc", "../x", "a/../../x", "./a", "", "a\\b", "a/./b"] {
        assert!(safe_join_rel(root, bad).is_err(), "{bad:?}");
    }
}

#[test]
fn encode_segment_escapes_slashes() {
    assert_eq!(encode_segment("a/b c"), "a%2Fb%20c");
}
