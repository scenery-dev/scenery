#![allow(clippy::unwrap_used)]
use super::*;

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
