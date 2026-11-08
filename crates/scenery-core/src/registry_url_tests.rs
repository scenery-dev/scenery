#![allow(clippy::unwrap_used)]
use super::*;

#[test]
fn https_origin_ok() {
    assert_eq!(
        validate_origin("https://app.scenery.dev").unwrap(),
        "https://app.scenery.dev"
    );
    assert_eq!(
        validate_origin("https://app.scenery.dev/").unwrap(),
        "https://app.scenery.dev"
    );
}

#[test]
fn rejects_paths_userinfo_and_other_schemes() {
    assert_eq!(
        validate("https://a.dev/x").unwrap_err(),
        RegistryUrlError::HasPathOrQuery
    );
    assert_eq!(
        validate("https://a.dev/?q=1").unwrap_err(),
        RegistryUrlError::HasPathOrQuery
    );
    assert_eq!(
        validate("https://u:p@a.dev").unwrap_err(),
        RegistryUrlError::HasUserinfo
    );
    assert!(matches!(
        validate("ftp://a.dev").unwrap_err(),
        RegistryUrlError::NotHttps(_)
    ));
    assert!(matches!(
        validate("not a url").unwrap_err(),
        RegistryUrlError::Parse(_)
    ));
}

#[test]
fn http_only_for_loopback() {
    // Remote http is never allowed, even in debug builds.
    assert_eq!(
        validate("http://evil.example").unwrap_err(),
        RegistryUrlError::InsecureNotAllowed
    );
    assert_eq!(
        validate("http://app.scenery.dev.evil.example").unwrap_err(),
        RegistryUrlError::InsecureNotAllowed
    );
    // Loopback http is allowed in debug builds (tests run as debug).
    assert!(validate("http://127.0.0.1:9").is_ok());
    assert!(validate("http://localhost:8000").is_ok());
}

#[test]
fn same_origin_is_exact() {
    assert!(same_origin("https://a.dev/x", "https://a.dev/y"));
    assert!(!same_origin("https://a.dev", "https://a.dev.evil.example"));
    assert!(!same_origin("https://a.dev", "http://a.dev"));
    assert!(!same_origin("https://a.dev", "https://a.dev:8443"));
    assert!(!same_origin("garbage", "https://a.dev"));
}
