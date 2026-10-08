//! Calendar dates in the computer's time zone, for "today", "this month" and dates shown
//! to the user.

/// Seconds the local time zone is ahead of UTC right now.
pub fn utc_offset() -> i64 {
    #[cfg(unix)]
    {
        let now = crate::model::now_unix() as libc::time_t;
        // SAFETY: `localtime_r` only writes the `tm` it is given.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            if !libc::localtime_r(&now, &mut tm).is_null() {
                return tm.tm_gmtoff as i64;
            }
        }
        0
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        // SAFETY: the call only fills in the struct it is given.
        unsafe {
            let mut tz: TIME_ZONE_INFORMATION = std::mem::zeroed();
            // UTC = local time + bias (in minutes); daylight saving time adds its own bias.
            let bias = match GetTimeZoneInformation(&mut tz) {
                1 => tz.Bias + tz.StandardBias,
                2 => tz.Bias + tz.DaylightBias,
                0 => tz.Bias,
                _ => 0,
            };
            -(bias as i64) * 60
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        0
    }
}

/// Days since 1970-01-01 for a date (Howard Hinnant's algorithm).
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let (m, d) = (month as i64, day as i64);
    let y = if m <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01: (year, month 1-12, day 1-31).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// A moment in local time, as days since 1970-01-01.
pub fn local_day(unix: i64, offset: i64) -> i64 {
    (unix + offset).div_euclid(86_400)
}

/// When a local day (from [`local_day`]) starts, as a unix time.
pub fn day_start(day: i64, offset: i64) -> i64 {
    day * 86_400 - offset
}

/// When a local month starts, as a unix time. `month` may run past 1-12 either way.
pub fn month_start(year: i64, month: i64, offset: i64) -> i64 {
    let index = year * 12 + (month - 1);
    let (y, m) = (index.div_euclid(12), index.rem_euclid(12) + 1);
    day_start(days_from_civil(y, m as u32, 1), offset)
}

pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
pub const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Day of the week of a local day, 0 = Sunday.
pub fn weekday(day: i64) -> usize {
    // 1970-01-01 was a Thursday.
    (day + 4).rem_euclid(7) as usize
}

/// "14 Mar 2014".
pub fn date(unix: i64, offset: i64) -> String {
    let (y, m, d) = civil_from_days(local_day(unix, offset));
    format!("{d} {} {y}", MONTHS[m as usize - 1])
}

/// "just now", "5 min ago", "3 h ago", "yesterday", then a date.
pub fn ago(unix: i64, now: i64, offset: i64) -> String {
    let secs = (now - unix).max(0);
    if secs < 60 {
        return "just now".into();
    }
    if secs < 3600 {
        return format!("{} min ago", secs / 60);
    }
    let days = local_day(now, offset) - local_day(unix, offset);
    if days == 0 {
        return format!("{} h ago", secs / 3600);
    }
    if days == 1 {
        return "yesterday".into();
    }
    if days < 7 {
        return format!("{days} days ago");
    }
    date(unix, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        for day in [-1000, 0, 59, 365, 11_016, 11_017, 20_000, 30_000] {
            let (y, m, d) = civil_from_days(day);
            assert_eq!(days_from_civil(y, m, d), day);
        }
        // 2024 is a leap year.
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 28) + 1), (2024, 2, 29));
        assert_eq!(weekday(0), 4);
        assert_eq!(WEEKDAYS[weekday(days_from_civil(2026, 10, 8))], "Thu");
    }

    #[test]
    fn local_days_and_months() {
        // 2026-10-08 00:30 in UTC+2 is still 2026-10-07 22:30 in UTC.
        let offset = 2 * 3600;
        let t = day_start(days_from_civil(2026, 10, 8), offset) + 1800;
        assert_eq!(civil_from_days(local_day(t, offset)), (2026, 10, 8));
        assert_eq!(civil_from_days(local_day(t, 0)), (2026, 10, 7));
        assert_eq!(date(t, offset), "8 Oct 2026");
        // Months before January belong to the year before.
        assert_eq!(month_start(2026, 0, 0), day_start(days_from_civil(2025, 12, 1), 0));
        assert_eq!(month_start(2026, 13, 0), day_start(days_from_civil(2027, 1, 1), 0));
    }

    #[test]
    fn how_long_ago() {
        let now = day_start(days_from_civil(2026, 10, 8), 0) + 15 * 3600;
        assert_eq!(ago(now - 10, now, 0), "just now");
        assert_eq!(ago(now - 300, now, 0), "5 min ago");
        assert_eq!(ago(now - 3 * 3600, now, 0), "3 h ago");
        assert_eq!(ago(now - 20 * 3600, now, 0), "yesterday");
        assert_eq!(ago(now - 3 * 86_400, now, 0), "3 days ago");
        assert_eq!(ago(now - 30 * 86_400, now, 0), "8 Sep 2026");
    }
}
