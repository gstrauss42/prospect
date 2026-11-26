//! Calendar dates without a date library. All dates are UTC; days are counted from 1970-01-01.
//! The civil-date conversions are Howard Hinnant's algorithms.

use std::time::{SystemTime, UNIX_EPOCH};

pub const DAY_SECS: i64 = 86_400;

/// Seconds since the Unix epoch.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (year, month, day) for days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// "YYYY-MM-DD" for a Unix timestamp.
pub fn ymd(unix: i64) -> String {
    let (y, m, d) = civil_from_days(unix.div_euclid(DAY_SECS));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Today's date (UTC) as "YYYY-MM-DD".
pub fn today() -> String {
    ymd(now_unix())
}

/// Days since the epoch for a strict "YYYY-MM-DD", or None if it isn't a real date.
pub fn parse_ymd(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = &s[r];
        if part.bytes().all(|c| c.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let (y, m, d) = (num(0..4)?, num(5..7)? as u32, num(8..10)? as u32);
    if !(1900..=2200).contains(&y) || !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// The "YYYY-MM-DD" at the start of an RFC 3339 timestamp or plain date, if valid.
pub fn ymd_prefix(s: &str) -> Option<String> {
    let s = s.trim();
    let head = s.get(0..10)?;
    parse_ymd(head).map(|_| head.to_string())
}

/// `ymd` moved by `days` (negative to go back).
pub fn add_days(ymd_str: &str, days: i64) -> Option<String> {
    let d = parse_ymd(ymd_str)? + days;
    let (y, m, dd) = civil_from_days(d);
    Some(format!("{y:04}-{m:02}-{dd:02}"))
}

/// Day of the week for a Unix timestamp, 0 = Monday … 6 = Sunday.
pub fn weekday(unix: i64) -> u32 {
    // 1970-01-01 was a Thursday (3).
    ((unix.div_euclid(DAY_SECS) + 3).rem_euclid(7)) as u32
}

/// Hour of the day (UTC) for a Unix timestamp.
pub fn hour(unix: i64) -> u32 {
    (unix.rem_euclid(DAY_SECS) / 3600) as u32
}

/// "26 Nov 2026" for a "YYYY-MM-DD".
pub fn human(ymd_str: &str) -> String {
    const M: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    match parse_ymd(ymd_str) {
        Some(days) => {
            let (y, m, d) = civil_from_days(days);
            format!("{d} {} {y}", M[(m - 1) as usize])
        }
        None => ymd_str.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_many_days() {
        for z in (-800_000..800_000).step_by(997) {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    #[test]
    fn known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(1_795_682_592), "2026-11-26");
        assert_eq!(
            parse_ymd("2024-02-29").map(civil_from_days),
            Some((2024, 2, 29))
        );
        assert_eq!(parse_ymd("2025-02-29"), None);
        assert_eq!(parse_ymd("2026-13-01"), None);
        assert_eq!(parse_ymd("2026-1-01"), None);
        assert_eq!(parse_ymd("abcd-ef-gh"), None);
        assert_eq!(
            ymd_prefix("2026-09-22T16:00:00+00:00").as_deref(),
            Some("2026-09-22")
        );
        assert_eq!(ymd_prefix("Sept 2026"), None);
        assert_eq!(add_days("2026-12-30", 3).as_deref(), Some("2027-01-02"));
        assert_eq!(add_days("2026-03-01", -1).as_deref(), Some("2026-02-28"));
        assert_eq!(human("2026-11-26"), "26 Nov 2026");
    }

    #[test]
    fn weekdays_and_hours() {
        assert_eq!(weekday(0), 3); // Thursday
        assert_eq!(weekday(1_795_682_592), 3); // 2026-11-26 is a Thursday
        assert_eq!(hour(1_795_682_592), (1_795_682_592 % 86_400 / 3600) as u32);
    }
}
