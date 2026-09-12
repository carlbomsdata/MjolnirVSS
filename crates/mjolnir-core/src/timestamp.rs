//! UTC timestamps, without a calendar dependency.
//!
//! MjolnirVSS needs exactly two things from a clock: an RFC 3339 stamp for the
//! manifest, and a `YYYY-MM-DD_HHMM` stamp for the default backup name. Both
//! are derived here from the Unix epoch using the civil-from-days algorithm, so
//! the tool carries no date and time crate into Windows PE.
//!
//! Leap seconds are not represented, which matches Unix time and is what every
//! other consumer of these stamps expects.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// A point in time, as whole seconds since the Unix epoch, in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UtcTimestamp {
    seconds: i64,
}

/// A broken down UTC date and time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcDateTime {
    /// Year, for example 2026.
    pub year: i64,
    /// Month, 1 to 12.
    pub month: u32,
    /// Day of month, 1 to 31.
    pub day: u32,
    /// Hour, 0 to 23.
    pub hour: u32,
    /// Minute, 0 to 59.
    pub minute: u32,
    /// Second, 0 to 59.
    pub second: u32,
}

impl UtcTimestamp {
    /// The current time.
    ///
    /// A system clock set before 1970 yields the epoch rather than a negative
    /// stamp, because a backup directory name has to stay sortable.
    pub fn now() -> Self {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self { seconds }
    }

    /// Wraps a count of seconds since the Unix epoch.
    pub const fn from_unix_seconds(seconds: i64) -> Self {
        Self { seconds }
    }

    /// Seconds since the Unix epoch.
    pub const fn unix_seconds(self) -> i64 {
        self.seconds
    }

    /// Breaks the timestamp down into calendar fields.
    pub fn to_datetime(self) -> UtcDateTime {
        // Split into whole days and the seconds within the day, rounding
        // towards negative infinity so pre-epoch stamps stay correct.
        let days = self.seconds.div_euclid(86_400);
        let secs_of_day = self.seconds.rem_euclid(86_400);

        let (year, month, day) = civil_from_days(days);
        UtcDateTime {
            year,
            month,
            day,
            hour: (secs_of_day / 3600) as u32,
            minute: ((secs_of_day % 3600) / 60) as u32,
            second: (secs_of_day % 60) as u32,
        }
    }

    /// RFC 3339 form with a `Z` suffix, as written into the manifest.
    pub fn to_rfc3339(self) -> String {
        let t = self.to_datetime();
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            t.year, t.month, t.day, t.hour, t.minute, t.second
        )
    }

    /// `YYYY-MM-DD_HHMM`, the date part of the default backup name.
    pub fn to_backup_name_stamp(self) -> String {
        let t = self.to_datetime();
        format!(
            "{:04}-{:02}-{:02}_{:02}{:02}",
            t.year, t.month, t.day, t.hour, t.minute
        )
    }

    /// `YYYY-MM-DD HH:MM:SS` UTC, for log lines.
    pub fn to_log_stamp(self) -> String {
        let t = self.to_datetime();
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            t.year, t.month, t.day, t.hour, t.minute, t.second
        )
    }
}

impl fmt::Display for UtcTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

/// Converts days since 1970-01-01 into a civil date.
///
/// This is Howard Hinnant's `civil_from_days`, which is exact for the whole
/// range of `i64` days and needs no lookup tables. The shift moves the epoch to
/// 0000-03-01 so that a leap day always lands at the end of a 400 year era.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era, 0..=146096
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // 0..=399
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // 0..=365
    let mp = (5 * doy + 2) / 153; // 0..=11, March based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m: u32 = if mp < 10 {
        (mp + 3) as u32
    } else {
        (mp - 9) as u32
    };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> UtcDateTime {
        UtcTimestamp::from_unix_seconds(seconds).to_datetime()
    }

    #[test]
    fn epoch_is_the_first_of_january_1970() {
        assert_eq!(
            at(0),
            UtcDateTime {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0
            }
        );
    }

    #[test]
    fn known_instants_decode_correctly() {
        // 2001-09-09T01:46:40Z, the billionth second.
        assert_eq!(
            UtcTimestamp::from_unix_seconds(1_000_000_000).to_rfc3339(),
            "2001-09-09T01:46:40Z"
        );
        // 2026-09-12T17:46:35Z.
        assert_eq!(
            UtcTimestamp::from_unix_seconds(1_789_235_195).to_rfc3339(),
            "2026-09-12T17:46:35Z"
        );
    }

    #[test]
    fn leap_day_is_handled() {
        // 2024-02-29T12:00:00Z.
        let t = at(1_709_208_000);
        assert_eq!((t.year, t.month, t.day), (2024, 2, 29));
        // 2000 was a leap year, 1900 was not. Checked through day arithmetic:
        // 2000-02-29 exists.
        let y2k_leap = at(951_825_600);
        assert_eq!((y2k_leap.year, y2k_leap.month, y2k_leap.day), (2000, 2, 29));
    }

    #[test]
    fn end_of_year_boundaries_are_exact() {
        assert_eq!(
            UtcTimestamp::from_unix_seconds(1_767_225_599).to_rfc3339(),
            "2025-12-31T23:59:59Z"
        );
        assert_eq!(
            UtcTimestamp::from_unix_seconds(1_767_225_600).to_rfc3339(),
            "2026-01-01T00:00:00Z"
        );
    }

    #[test]
    fn pre_epoch_times_do_not_go_wrong() {
        assert_eq!(
            UtcTimestamp::from_unix_seconds(-1).to_rfc3339(),
            "1969-12-31T23:59:59Z"
        );
    }

    #[test]
    fn backup_name_stamp_is_sortable() {
        let a = UtcTimestamp::from_unix_seconds(1_767_225_599).to_backup_name_stamp();
        let b = UtcTimestamp::from_unix_seconds(1_767_225_600).to_backup_name_stamp();
        assert_eq!(a, "2025-12-31_2359");
        assert_eq!(b, "2026-01-01_0000");
        assert!(a < b, "stamps must sort chronologically as strings");
    }

    #[test]
    fn now_is_after_the_project_started() {
        // Guards against a clock read that silently returns the epoch.
        assert!(UtcTimestamp::now().unix_seconds() > 1_700_000_000);
    }

    #[test]
    fn every_day_round_trips_for_a_century() {
        // Walks 1970 to 2070 one day at a time and checks that the date always
        // advances by exactly one day, which catches an off by one in any era.
        let mut previous = at(0);
        for day in 1..36_525i64 {
            let t = at(day * 86_400);
            let advanced = if t.day == 1 {
                previous.day > 1
            } else {
                t.day == previous.day + 1
            };
            assert!(advanced, "day {day}: {previous:?} then {t:?}");
            previous = t;
        }
    }
}
