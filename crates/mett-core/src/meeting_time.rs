//! Meeting times and recurrences as the adapters read and ask for them.

use chrono::{DateTime, Duration, NaiveTime, Utc};

/// The midnight (UTC) of the day an all-day meeting's time falls in, for a
/// server that gives the time as a midnight, perhaps shifted to UTC from
/// the time zone it was made in. Adding 13 hours gives the day's date for
/// the zones from UTC-10 (Hawaii) to UTC+13 (New Zealand in summer): a few
/// zones beyond, such as UTC+14, are off by a day.
pub fn whole_day(time: DateTime<Utc>) -> DateTime<Utc> {
    (time + Duration::hours(13))
        .date_naive()
        .and_time(NaiveTime::MIN)
        .and_utc()
}

/// The time window to ask a calendar for the occurrence that starts at
/// `start` and ends at `end` (if known). A window lists what is there: one
/// of an instant, or of an end that isn't after the start, can have nothing
/// at its end, so it is given a second.
pub fn occurrence_window(
    start: DateTime<Utc>,
    end: Option<DateTime<Utc>>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let end = end
        .filter(|e| *e > start)
        .unwrap_or(start + Duration::seconds(1));
    (start, end)
}

/// A recurrence's "which weekday of the month" word (`first`, `second`,
/// `third`, `fourth` or `last`, in any case) as its `nth` (1 to 4, or -1
/// for the last).
pub fn nth_of(index: &str) -> Option<i8> {
    const NAMES: [(&str, i8); 5] = [
        ("first", 1),
        ("second", 2),
        ("third", 3),
        ("fourth", 4),
        ("last", -1),
    ];
    let index = index.trim();
    NAMES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(index))
        .map(|(_, nth)| *nth)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, day, hour, 0, 0).unwrap()
    }

    #[test]
    fn a_whole_day_is_the_date_of_its_midnight_in_nearly_every_zone() {
        // Midnight UTC, and midnight in zones from UTC-10 to UTC+13,
        // shifted to UTC, are all the day of the 5th.
        for shifted in [at(5, 0), at(4, 11), at(5, 10), at(4, 14)] {
            assert_eq!(whole_day(shifted), at(5, 0), "{shifted}");
        }
        // Beyond them, a day off.
        assert_eq!(whole_day(at(4, 9)), at(4, 0));
        assert_eq!(whole_day(at(4, 10)), at(4, 0));
        assert_eq!(whole_day(at(5, 11)), at(6, 0));
    }

    #[test]
    fn a_window_has_an_end_after_its_start() {
        let start = at(5, 9);
        assert_eq!(
            occurrence_window(start, Some(at(5, 10))),
            (start, at(5, 10))
        );
        let a_second = start + Duration::seconds(1);
        assert_eq!(occurrence_window(start, None), (start, a_second));
        assert_eq!(occurrence_window(start, Some(start)), (start, a_second));
        assert_eq!(occurrence_window(start, Some(at(5, 8))), (start, a_second));
    }

    #[test]
    fn names_the_weekday_of_the_month() {
        for (word, nth) in [("first", 1), ("Second", 2), ("THIRD", 3), (" Fourth ", 4)] {
            assert_eq!(nth_of(word), Some(nth));
        }
        assert_eq!(nth_of("Last"), Some(-1));
        assert_eq!(nth_of("fifth"), None);
        assert_eq!(nth_of(""), None);
    }
}
