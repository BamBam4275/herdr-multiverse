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

use std::sync::OnceLock;

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

/// `instant`'s local calendar date and clock, as `zone` has it at the UTC
/// offset in effect at that exact instant — so a card from before a DST
/// change keeps its own, different offset from one minted after it, the
/// same as `date -r <epoch>` (or `astimezone(None)`) with `TZ` set to
/// `zone`. A pure offset lookup once `zone` is parsed: no subprocess, no
/// further file I/O.
pub fn local_datetime_in(
    zone: &tz::TimeZone,
    instant: OffsetDateTime,
) -> Option<time::PrimitiveDateTime> {
    let local_type = zone.find_local_time_type(instant.unix_timestamp()).ok()?;
    let offset = time::UtcOffset::from_whole_seconds(local_type.ut_offset()).ok()?;
    let local = instant.to_offset(offset);
    Some(time::PrimitiveDateTime::new(local.date(), local.time()))
}

fn resolve_ambient_time_zone() -> Option<tz::TimeZone> {
    // Mirrors how `date`/`tzset(3)` pick a zone: an explicit `TZ` (POSIX
    // string, `Region/City`, or empty for UTC) wins; otherwise fall back
    // to the system default (`/etc/localtime`).
    match std::env::var("TZ") {
        Ok(tz) if tz.is_empty() => Some(tz::TimeZone::utc()),
        Ok(tz) => tz::TimeZone::from_posix_tz(&tz).ok(),
        Err(_) => tz::TimeZone::local().ok(),
    }
}

/// The ambient system timezone, parsed from the IANA tzdata at most once
/// per process. A board render touching thousands of cards shares this
/// one parse; only `local_datetime`'s per-instant offset lookup repeats.
fn ambient_time_zone() -> Option<&'static tz::TimeZone> {
    static ZONE: OnceLock<Option<tz::TimeZone>> = OnceLock::new();
    ZONE.get_or_init(resolve_ambient_time_zone).as_ref()
}

/// `local_datetime_in` against the ambient system timezone (`TZ`, or the
/// system default). Returns `None` if the zone cannot be resolved, the
/// same failure mode the old `date`-spawning implementation had when the
/// child process failed.
pub fn local_datetime(instant: OffsetDateTime) -> Option<time::PrimitiveDateTime> {
    local_datetime_in(ambient_time_zone()?, instant)
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

    #[test]
    fn local_datetime_in_a_fixed_offset_zone() {
        let zone = tz::TimeZone::fixed(-5 * 3_600).unwrap();
        let instant = datetime!(2024-01-15 12:00:00 UTC);
        assert_eq!(
            local_datetime_in(&zone, instant),
            Some(datetime!(2024-01-15 07:00:00))
        );
    }

    #[test]
    fn local_datetime_in_utc_is_the_identity() {
        let instant = datetime!(2024-01-15 12:00:00 UTC);
        assert_eq!(
            local_datetime_in(&tz::TimeZone::utc(), instant),
            Some(datetime!(2024-01-15 12:00:00))
        );
    }

    #[test]
    fn local_datetime_in_uses_each_instants_own_dst_offset() {
        // US Eastern as a POSIX rule, not a named zone, so this stays
        // hermetic: no zoneinfo file on disk is involved.
        let zone = tz::TimeZone::from_posix_tz("EST5EDT,M3.2.0,M11.1.0").unwrap();
        let winter = datetime!(2024-01-15 12:00:00 UTC); // EST, UTC-5
        let summer = datetime!(2024-07-15 12:00:00 UTC); // EDT, UTC-4
        assert_eq!(
            local_datetime_in(&zone, winter),
            Some(datetime!(2024-01-15 07:00:00))
        );
        assert_eq!(
            local_datetime_in(&zone, summer),
            Some(datetime!(2024-07-15 08:00:00))
        );
    }
}
