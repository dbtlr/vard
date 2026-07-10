//! RFC 3339 UTC rendering and the small time-expression grammar shared by the
//! host CLI's `--at` / `--since` flags.
//!
//! vard pulls in no date crate, so the calendar math is a hand-rolled pair of
//! Howard Hinnant's `days_from_civil` / `civil_from_days` algorithms — small,
//! well-known, and exact for the Gregorian proleptic calendar. This module
//! lives in `vard-core` (not the binary) so every host — the CLI today, the
//! status surface VRD-18 needs tomorrow — renders timestamps identically.
//!
//! # `--at` grammar
//!
//! Deliberately narrow (see [`parse_at`]): a humane duration counted back from
//! now (`2h`, `3d`, reusing [`parse_duration`](crate::parse_duration)), or an
//! absolute UTC date. The absolute form is `YYYY-MM-DDTHH:MM` (a `T` separator,
//! so no shell quoting is needed), also accepting a bare `YYYY-MM-DD` and the
//! space-separated `YYYY-MM-DD HH:MM` (which a shell needs quoted). A bare date
//! with no time-of-day means the **end** of that day (`23:59:59Z`) — the
//! natural "state as of that day". Calendar-invalid dates (Feb 29 on a
//! non-leap year, Apr 31) and years outside `1970..=9999` are rejected with a
//! clear error; anything fancier — `yesterday 3pm`, `last tuesday` — is
//! rejected with a message listing what *is* supported.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The instant `duration` before `now`, clamped at the Unix epoch so a duration
/// longer than `now`'s age never underflows. The one owner of the
/// "duration counted back from now" semantics shared by `--at` and `--since`,
/// so both flags agree to the second.
pub fn duration_back_from_now(duration: Duration, now: SystemTime) -> SystemTime {
    let since_epoch = now.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let back = since_epoch.checked_sub(duration).unwrap_or(Duration::ZERO);
    UNIX_EPOCH + back
}

/// Parses a `restore --at` expression into the instant it names, relative to
/// `now`. Accepts a duration back from `now` or an absolute UTC date; returns a
/// human-readable error (listing the supported forms) for anything else.
pub fn parse_at(expr: &str, now: SystemTime) -> Result<SystemTime, String> {
    let trimmed = expr.trim();

    // A humane duration ("2h", "3d", "1h30m") means "that long ago".
    if let Ok(duration) = crate::parse_duration(trimmed) {
        return Ok(duration_back_from_now(duration, now));
    }

    // An absolute UTC date, with or without a time-of-day.
    match parse_absolute_utc(trimmed) {
        Some(result) => result,
        None => Err(format!(
            "unsupported --at value {expr:?}; use a duration back from now like \"2h\" or \"3d\", \
             or an absolute UTC date \"YYYY-MM-DDThh:mm\" (a bare \"YYYY-MM-DD\" means the end of \
             that day; the space form \"YYYY-MM-DD hh:mm\" also works but needs shell quoting)"
        )),
    }
}

/// Parses an absolute UTC date, optionally with a time-of-day (`T` or space
/// separator). Returns `None` when the input is not date-shaped at all (so the
/// caller can fall through to its generic guidance), `Some(Err)` when it *is*
/// date-shaped but invalid (an out-of-range year, a calendar-impossible day, a
/// bad time), and `Some(Ok)` for a valid instant. A bare date with no
/// time-of-day resolves to the **end** of that day (`23:59:59Z`).
fn parse_absolute_utc(s: &str) -> Option<Result<SystemTime, String>> {
    let (date, time) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };

    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = parse_two_digit(date_parts.next()?)?;
    let day: i64 = parse_two_digit(date_parts.next()?)?;
    if date_parts.next().is_some() {
        // A fourth dash-separated field: not a date we recognize.
        return None;
    }

    // From here the input is date-shaped, so every rejection is an explicit,
    // date-specific error rather than the caller's generic guidance.
    if !(1970..=9999).contains(&year) {
        return Some(Err(format!(
            "year {year} in {s:?} is out of range; --at supports 1970 through 9999"
        )));
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Some(Err(format!("{s:?} is not a valid calendar date")));
    }
    // Round-trip through the calendar to reject impossible days (Feb 29 on a
    // non-leap year, Apr 31): the day only exists if it maps back unchanged.
    let days = days_from_civil(year, month, day);
    if civil_from_days(days) != (year, month, day) {
        return Some(Err(format!("{s:?} is not a valid calendar date")));
    }

    // Given a time-of-day, honor it; a bare date means the end of the day.
    let (hour, minute, second) = match time {
        None => (23, 59, 59),
        Some(t) => match parse_time_of_day(t) {
            Some(hms) => hms,
            None => {
                return Some(Err(format!(
                    "{t:?} in {s:?} is not a valid time of day; use hh:mm or hh:mm:ss (24-hour UTC)"
                )));
            }
        },
    };

    // year >= 1970 keeps this non-negative and well inside u64.
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    Some(Ok(UNIX_EPOCH + Duration::from_secs(secs as u64)))
}

/// Parses a `hh:mm` or `hh:mm:ss` 24-hour time-of-day. Returns `None` for any
/// malformed or out-of-range value.
fn parse_time_of_day(t: &str) -> Option<(i64, i64, i64)> {
    let mut tp = t.split(':');
    let hour = parse_two_digit(tp.next()?)?;
    let minute = parse_two_digit(tp.next()?)?;
    let second = match tp.next() {
        Some(sec) => parse_two_digit(sec)?,
        None => 0,
    };
    if tp.next().is_some() {
        return None;
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=59).contains(&second) {
        return None;
    }
    Some((hour, minute, second))
}

/// Parses a one- or two-digit non-negative field (accepting `3` as well as
/// `03`).
fn parse_two_digit(s: &str) -> Option<i64> {
    if s.is_empty() || s.len() > 2 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Formats a snapshot timestamp as an RFC 3339 UTC string
/// (`2026-07-09T14:03:22Z`) — machine-parseable and human-readable in one
/// field, so the records and JSON forms agree.
pub fn format_rfc3339_utc(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Days since 1970-01-01 for a Gregorian `y-m-d` (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The Gregorian `(year, month, day)` for a count of days since 1970-01-01
/// (Howard Hinnant's algorithm; the inverse of [`days_from_civil`]).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_duration_counts_back_from_now() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let at = parse_at("2h", now).unwrap();
        assert_eq!(at, now - Duration::from_secs(7_200));
    }

    #[test]
    fn relative_duration_saturates_at_epoch() {
        let now = UNIX_EPOCH + Duration::from_secs(60);
        // 3 days before a 60-second-old epoch clamps to the epoch, not a panic.
        assert_eq!(parse_at("3d", now).unwrap(), UNIX_EPOCH);
    }

    #[test]
    fn bare_absolute_date_is_end_of_day() {
        // A bare date is "state as of that day", so it resolves to the day's
        // final second — a snapshot taken any time that day is at or before it.
        let at = parse_at("2026-07-09", SystemTime::now()).unwrap();
        assert_eq!(format_rfc3339_utc(at), "2026-07-09T23:59:59Z");
    }

    #[test]
    fn absolute_date_with_time_space_form() {
        let at = parse_at("2026-07-09 14:03", SystemTime::now()).unwrap();
        assert_eq!(format_rfc3339_utc(at), "2026-07-09T14:03:00Z");
    }

    #[test]
    fn absolute_date_with_time_t_form() {
        // The T separator needs no shell quoting.
        let at = parse_at("2026-07-09T14:03", SystemTime::now()).unwrap();
        assert_eq!(format_rfc3339_utc(at), "2026-07-09T14:03:00Z");
    }

    #[test]
    fn absolute_date_with_seconds() {
        let at = parse_at("2026-07-09T14:03:22", SystemTime::now()).unwrap();
        assert_eq!(format_rfc3339_utc(at), "2026-07-09T14:03:22Z");
    }

    #[test]
    fn natural_language_is_rejected_with_guidance() {
        let err = parse_at("yesterday 3pm", SystemTime::now()).unwrap_err();
        assert!(err.contains("YYYY-MM-DD"), "got: {err}");
        assert!(err.contains("2h"), "got: {err}");
    }

    #[test]
    fn malformed_dates_are_rejected() {
        let now = SystemTime::now();
        assert!(parse_at("2026-13-01", now).is_err(), "month 13");
        assert!(parse_at("2026-07-32", now).is_err(), "day 32");
        assert!(parse_at("2026-07-09 25:00", now).is_err(), "hour 25");
        assert!(parse_at("not-a-date", now).is_err());
    }

    #[test]
    fn calendar_invalid_days_are_rejected() {
        let now = SystemTime::now();
        // Feb 29 on a non-leap year does not exist.
        let err = parse_at("2026-02-29", now).unwrap_err();
        assert!(err.contains("not a valid calendar date"), "got: {err}");
        // April has 30 days.
        assert!(parse_at("2026-04-31", now).is_err(), "Apr 31");
        // A leap year still accepts Feb 29.
        assert!(parse_at("2024-02-29", now).is_ok(), "2024 is a leap year");
    }

    #[test]
    fn out_of_range_year_is_rejected_without_overflow() {
        let now = SystemTime::now();
        // A gigantic year must error cleanly, never panic on the seconds math.
        let err = parse_at("999999999999-01-01", now).unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
        // A year before the epoch is rejected too.
        assert!(parse_at("1969-12-31", now).is_err(), "pre-epoch year");
    }

    #[test]
    fn rfc3339_round_trips_through_civil_math() {
        // A known instant: 2000-01-01T00:00:00Z is 946684800 seconds.
        let t = UNIX_EPOCH + Duration::from_secs(946_684_800);
        assert_eq!(format_rfc3339_utc(t), "2000-01-01T00:00:00Z");
    }

    #[test]
    fn duration_back_from_now_clamps_at_epoch() {
        let now = UNIX_EPOCH + Duration::from_secs(100);
        assert_eq!(
            duration_back_from_now(Duration::from_secs(50), now),
            UNIX_EPOCH + Duration::from_secs(50)
        );
        assert_eq!(
            duration_back_from_now(Duration::from_secs(1_000), now),
            UNIX_EPOCH
        );
    }
}
