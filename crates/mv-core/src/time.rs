//! UTC timestamp formatting: writers emit `+00:00` offsets at second
//! precision; readers accept `Z` and other offset forms.
//!
//! Every writer in the Python original stamps records with
//! `datetime.now(timezone.utc).isoformat(timespec="seconds")`
//! (`accounts.py:2559-2561`, `:3270`), which prints e.g.
//! `2026-11-02T12:00:00+00:00` — never a trailing `Z`. Readers go through
//! `datetime.fromisoformat`, which (since the interpreter version this
//! fleet runs) accepts both `Z` and numeric offsets. This module keeps that
//! asymmetry on purpose: always write `+00:00`, always accept either on
//! read, so a Rust writer's own output round-trips through a Python reader
//! (and vice versa) without either side needing to special-case the other.

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// The current instant, in UTC.
pub fn now_utc() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Format `instant` the way every writer in this fleet stamps a record:
/// UTC, second precision, always a numeric `+00:00` offset — never `Z`.
///
/// Mirrors `datetime.now(timezone.utc).isoformat(timespec="seconds")`.
/// `instant` is converted to UTC first, so a caller handed a non-UTC
/// `OffsetDateTime` still gets the fleet's one true format rather than
/// silently stamping its own offset.
pub fn format_seconds_utc(instant: OffsetDateTime) -> String {
    let utc = instant.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
    )
}

/// `format_seconds_utc(now_utc())` — the one-call form every writer wants.
pub fn now_stamp() -> String {
    format_seconds_utc(now_utc())
}

/// A timestamp that is not a valid ISO-8601 / RFC-3339 datetime.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a valid ISO-8601 timestamp")]
pub struct TimestampParseError(pub String);

/// Parse a timestamp a Python writer may have produced: a numeric offset
/// (`+00:00`, `-05:00`, …) as every writer in this fleet emits, or a
/// trailing `Z`, which `datetime.fromisoformat` also accepts on read and
/// which some other tool or a future Rust writer may still produce.
///
/// RFC 3339 already specifies both forms as equivalent spellings of an
/// offset, so no bespoke `Z`-stripping is needed here — `time`'s RFC-3339
/// parser accepts either directly.
pub fn parse_flexible(value: &str) -> Result<OffsetDateTime, TimestampParseError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| TimestampParseError(value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn formats_with_a_numeric_offset_never_z() {
        let instant = datetime!(2026-11-02 12:00:00 UTC);
        assert_eq!(format_seconds_utc(instant), "2026-11-02T12:00:00+00:00");
    }

    #[test]
    fn formats_a_non_utc_instant_converted_to_utc_first() {
        let offset = time::UtcOffset::from_hms(-5, 0, 0).unwrap();
        let instant = datetime!(2026-11-02 07:00:00).assume_offset(offset);
        assert_eq!(format_seconds_utc(instant), "2026-11-02T12:00:00+00:00");
    }

    #[test]
    fn truncates_sub_second_precision_like_timespec_seconds() {
        let instant = datetime!(2026-11-02 12:00:00.999_999 UTC);
        assert_eq!(format_seconds_utc(instant), "2026-11-02T12:00:00+00:00");
    }

    #[test]
    fn parses_a_numeric_offset_written_by_this_fleet() {
        let parsed = parse_flexible("2026-11-02T12:00:00+00:00").unwrap();
        assert_eq!(parsed, datetime!(2026-11-02 12:00:00 UTC));
    }

    #[test]
    fn parses_a_trailing_z_the_same_as_a_numeric_offset() {
        let parsed = parse_flexible("2026-11-02T12:00:00Z").unwrap();
        assert_eq!(parsed, datetime!(2026-11-02 12:00:00 UTC));
    }

    #[test]
    fn parses_a_non_utc_numeric_offset() {
        let parsed = parse_flexible("2026-11-02T07:00:00-05:00").unwrap();
        assert_eq!(parsed, datetime!(2026-11-02 12:00:00 UTC));
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        assert!(parse_flexible("not a timestamp").is_err());
        assert!(parse_flexible("").is_err());
    }
}
