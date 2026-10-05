//! The proleptic Gregorian calendar, counted in days since the Unix epoch.

fn leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

pub(super) fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since the Unix epoch of a date.
pub(super) fn days(year: i64, month: i64, day: i64) -> i64 {
    // Years start in March so that the leap day is last.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year.rem_euclid(400);
    let of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let of_era_days = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    era * 146_097 + of_era_days - 719_468
}

/// The date of a day since the Unix epoch: year, month, day.
pub(super) fn date(day: i64) -> (i64, i64, i64) {
    let shifted = day + 719_468;
    let era = shifted.div_euclid(146_097);
    let of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * of_year + 2) / 153;
    let day = of_year - (153 * march_month + 2) / 5 + 1;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Span;

    /// The day after `(year, month, day)` by the calendar rules, counted by hand.
    fn next_day((year, month, day): (i64, i64, i64)) -> (i64, i64, i64) {
        if day < days_in_month(year, month) {
            (year, month, day + 1)
        } else if month < 12 {
            (year, month + 1, 1)
        } else {
            (year + 1, 1, 1)
        }
    }

    #[test]
    fn every_day_a_stamp_holds_follows_the_calendar() {
        let first = i64::MIN.div_euclid(Span::DAY.nanos());
        let last = i64::MAX.div_euclid(Span::DAY.nanos());
        let mut expected = date(first);
        for day in first..=last {
            assert_eq!(date(day), expected, "day {day}");
            assert_eq!(days(expected.0, expected.1, expected.2), day, "day {day}");
            expected = next_day(expected);
        }
    }
}
