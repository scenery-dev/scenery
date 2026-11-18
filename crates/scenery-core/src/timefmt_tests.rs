#![allow(clippy::unwrap_used)]

use super::*;
use time::Duration;

fn at(secs: i64, nanos: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).unwrap() + Duration::nanoseconds(nanos)
}

#[test]
fn rfc3339_matches_chrono_auto_si_layout() {
    // 2026-11-17T12:34:56Z
    let s = 1_794_918_896;
    assert_eq!(to_rfc3339(at(s, 0)), "2026-11-17T12:34:56+00:00");
    assert_eq!(
        to_rfc3339(at(s, 120_000_000)),
        "2026-11-17T12:34:56.120+00:00"
    );
    assert_eq!(
        to_rfc3339(at(s, 123_456_000)),
        "2026-11-17T12:34:56.123456+00:00"
    );
    assert_eq!(
        to_rfc3339(at(s, 123_456_789)),
        "2026-11-17T12:34:56.123456789+00:00"
    );
    assert_eq!(
        to_rfc3339(at(s, 5_000_000)),
        "2026-11-17T12:34:56.005+00:00"
    );
}

#[test]
fn rfc3339_round_trips_and_parses_z_and_offsets() {
    let t = at(1_794_918_896, 123_456_000);
    assert_eq!(parse_rfc3339(&to_rfc3339(t)).unwrap(), t);
    assert_eq!(
        parse_rfc3339("2026-11-17T12:34:56Z").unwrap(),
        at(1_794_918_896, 0)
    );
    assert_eq!(
        parse_rfc3339("2026-11-17T14:34:56+02:00").unwrap(),
        at(1_794_918_896, 0)
    );
    assert!(parse_rfc3339("not a date").is_none());
}

#[test]
fn rfc3339_strings_sort_chronologically() {
    let a = to_rfc3339(at(1_794_918_896, 0));
    let b = to_rfc3339(at(1_794_918_897, 0));
    assert!(a < b);
}

#[test]
fn stamps_match_strftime_layouts() {
    let t = at(1_794_918_896, 999);
    assert_eq!(stamp_colon(t), "2026-11-17T12:34:56Z");
    assert_eq!(stamp_compact(t), "2026-11-17T123456Z");
}

#[test]
fn unix_formatting() {
    assert_eq!(format_unix_utc(0).unwrap(), "1970-01-01 00:00:00 UTC");
    assert_eq!(
        format_unix_utc(1_794_918_896).unwrap(),
        "2026-11-17 12:34:56 UTC"
    );
}

#[test]
fn now_helpers_are_consistent() {
    assert!(parse_rfc3339(&now_rfc3339()).is_some());
    assert!((now_unix() - now().unix_timestamp()).abs() <= 1);
}
