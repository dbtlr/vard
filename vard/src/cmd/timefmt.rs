//! Modest, honest time handling for the snapshot commands: rendering a
//! snapshot's timestamp and parsing `restore --at`.
//!
//! vard pulls in no date crate, so the calendar math is a hand-rolled pair of
//! Howard Hinnant's `days_from_civil` / `civil_from_days` algorithms — small,
//! well-known, and exact for the Gregorian proleptic calendar.
//!
//! # `--at` grammar
//!
//! Deliberately narrow (see [`parse_at`]): a humane duration counted back from
//! now (`2h`, `3d`, reusing [`vard_core::parse_duration`]), or an absolute UTC
//! date `YYYY-MM-DD` optionally with ` HH:MM`. Anything fancier — `yesterday
//! 3pm`, `last tuesday` — is rejected with a message listing what *is*
//! supported, rather than shipping a half-working natural-language parser.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Parses a `restore --at` expression into the instant it names, relative to
/// `now`. Accepts a duration back from `now` or an absolute UTC date; returns a
/// human-readable error (listing the supported forms) for anything else.
pub(crate) fn parse_at(expr: &str, now: SystemTime) -> Result<SystemTime, String> {
    let trimmed = expr.trim();

    // A humane duration ("2h", "3d", "1h30m") means "that long ago", clamped at
    // the epoch so a duration longer than `now`'s age never underflows.
    if let Ok(duration) = vard_core::parse_duration(trimmed) {
        let since_epoch = now.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        let back = since_epoch.checked_sub(duration).unwrap_or(Duration::ZERO);
        return Ok(UNIX_EPOCH + back);
    }

    // An absolute UTC date, with or without a time-of-day.
    if let Some(t) = parse_absolute_utc(trimmed) {
        return Ok(t);
    }

    Err(format!(
        "unsupported --at value {expr:?}; use a duration back from now like \"2h\" or \"3d\", \
         or an absolute UTC date \"YYYY-MM-DD\" or \"YYYY-MM-DD HH:MM\""
    ))
}

/// Parses `YYYY-MM-DD` or `YYYY-MM-DD HH:MM` (a `T` separator is also accepted)
/// as a UTC instant. Returns `None` for any malformed or out-of-range value, or
/// a date before the Unix epoch.
fn parse_absolute_utc(s: &str) -> Option<SystemTime> {
    let (date, time) = match s.split_once([' ', 'T']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };

    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = parse_two_digit(date_parts.next()?)?;
    let day: i64 = parse_two_digit(date_parts.next()?)?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let (hour, minute) = match time {
        None => (0i64, 0i64),
        Some(t) => {
            let mut tp = t.split(':');
            let h: i64 = parse_two_digit(tp.next()?)?;
            let m: i64 = parse_two_digit(tp.next()?)?;
            // A trailing seconds field is tolerated but ignored; anything more
            // is rejected.
            if tp.next().is_some() && tp.next().is_some() {
                return None;
            }
            if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
                return None;
            }
            (h, m)
        }
    };

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60;
    if secs < 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
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
pub(crate) fn format_rfc3339_utc(t: SystemTime) -> String {
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
    fn absolute_date_is_utc_midnight() {
        let at = parse_at("2026-07-09", SystemTime::now()).unwrap();
        assert_eq!(format_rfc3339_utc(at), "2026-07-09T00:00:00Z");
    }

    #[test]
    fn absolute_date_with_time() {
        let at = parse_at("2026-07-09 14:03", SystemTime::now()).unwrap();
        assert_eq!(format_rfc3339_utc(at), "2026-07-09T14:03:00Z");
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
    fn rfc3339_round_trips_through_civil_math() {
        // A known instant: 2000-01-01T00:00:00Z is 946684800 seconds.
        let t = UNIX_EPOCH + Duration::from_secs(946_684_800);
        assert_eq!(format_rfc3339_utc(t), "2000-01-01T00:00:00Z");
    }
}
