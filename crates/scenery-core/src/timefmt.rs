//! Timestamp helpers built on the `time` crate (replaces `chrono`).
//!
//! Every string produced here is byte-identical to what the old
//! `chrono::Utc::now().to_rfc3339()` / `.format(..)` calls wrote, because
//! those strings are persisted in `state.db`, sent to the backend and sorted
//! lexicographically. In particular RFC 3339 output always ends in `+00:00`
//! (never `Z`) and carries 0, 3, 6 or 9 fractional digits (chrono's
//! `SecondsFormat::AutoSi`).

use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

/// Current UTC time.
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Current Unix time in seconds.
pub fn now_unix() -> i64 {
    now().unix_timestamp()
}

/// Format as chrono's `DateTime<Utc>::to_rfc3339()` did:
/// `2026-11-17T12:34:56.123456+00:00`.
pub fn to_rfc3339(dt: OffsetDateTime) -> String {
    let dt = dt.to_offset(UtcOffset::UTC);
    let nanos = dt.nanosecond();
    let frac = if nanos == 0 {
        String::new()
    } else if nanos % 1_000_000 == 0 {
        format!(".{:03}", nanos / 1_000_000)
    } else if nanos % 1_000 == 0 {
        format!(".{:06}", nanos / 1_000)
    } else {
        format!(".{nanos:09}")
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{frac}+00:00",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

/// `to_rfc3339(now())`.
pub fn now_rfc3339() -> String {
    to_rfc3339(now())
}

/// Parse an RFC 3339 timestamp (any offset, `Z` or `+00:00`).
pub fn parse_rfc3339(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

/// `%Y-%m-%dT%H:%M:%SZ` (second precision, colons).
pub fn stamp_colon(dt: OffsetDateTime) -> String {
    let dt = dt.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

/// `%Y-%m-%dT%H%M%SZ` (filesystem-safe, no colons).
pub fn stamp_compact(dt: OffsetDateTime) -> String {
    let dt = dt.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}{:02}{:02}Z",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

/// `%Y-%m-%d %H:%M:%S UTC` for a Unix timestamp; `None` if out of range.
pub fn format_unix_utc(ts: i64) -> Option<String> {
    let dt = OffsetDateTime::from_unix_timestamp(ts).ok()?;
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    ))
}

#[cfg(test)]
#[path = "timefmt_tests.rs"]
mod tests;
