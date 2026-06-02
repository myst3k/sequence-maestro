//! Pay schedule: when paychecks land. Cadence is *declared* (discovered once,
//! saved), never hardcoded; a `PaySchedule` enumerates real paydays in any range
//! so downstream counts actual paydays. Weekend paydays shift EARLIER to the
//! preceding Friday (paychecks never arrive later). Pure date math.

use chrono::{Datelike, Duration, NaiveDate, Weekday};
use serde::{Deserialize, Serialize};

pub fn last_day_of_month(year: i32, month: u32) -> NaiveDate {
    let (y, m) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(y, m, 1)
        .unwrap()
        .pred_opt()
        .unwrap()
}

/// Shift a weekend date back to the preceding Friday.
fn shift_for_weekend(d: NaiveDate) -> NaiveDate {
    match d.weekday() {
        Weekday::Sat => d - Duration::days(1),
        Weekday::Sun => d - Duration::days(2),
        _ => d,
    }
}

/// A day of the month where `-1` means "the last day" (clamped to month length).
fn resolve_month_day(year: i32, month: u32, day: i32) -> NaiveDate {
    let last = last_day_of_month(year, month).day();
    let d = if day < 0 {
        last
    } else {
        (day as u32).min(last)
    };
    NaiveDate::from_ymd_opt(year, month, d).unwrap()
}

/// How paychecks recur. Discovered from income history and saved; the engine
/// reads it back rather than assuming a cadence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaySchedule {
    /// Fixed days of each month, e.g. `[15, -1]` for the 15th and last day.
    SemiMonthly { days: Vec<i32> },
    /// One fixed day of each month (`-1` = last day).
    Monthly { day: i32 },
    /// Every 14 days from an anchor payday.
    Biweekly { anchor: NaiveDate },
    /// A fixed weekday, every week.
    Weekly { weekday: Weekday },
    /// No detectable cadence — cannot be scheduled against.
    Irregular,
}

impl Default for PaySchedule {
    fn default() -> Self {
        PaySchedule::SemiMonthly { days: vec![15, -1] }
    }
}

impl PaySchedule {
    /// Every payday in the inclusive range `[from, to]`, weekend-shifted to
    /// Friday, ascending. `Irregular` yields none.
    pub fn paydays_in(&self, from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
        if to < from {
            return Vec::new();
        }
        let mut out = Vec::new();
        match self {
            PaySchedule::SemiMonthly { days } => {
                let (mut y, mut m) = (from.year(), from.month());
                loop {
                    for &day in days {
                        let pd = shift_for_weekend(resolve_month_day(y, m, day));
                        if pd >= from && pd <= to {
                            out.push(pd);
                        }
                    }
                    if (y, m) == (to.year(), to.month()) {
                        break;
                    }
                    (y, m) = next_month(y, m);
                }
            }
            PaySchedule::Monthly { day } => {
                let (mut y, mut m) = (from.year(), from.month());
                loop {
                    let pd = shift_for_weekend(resolve_month_day(y, m, *day));
                    if pd >= from && pd <= to {
                        out.push(pd);
                    }
                    if (y, m) == (to.year(), to.month()) {
                        break;
                    }
                    (y, m) = next_month(y, m);
                }
            }
            PaySchedule::Biweekly { anchor } => {
                // Step back to the first payday on/before `from`, then forward.
                let days_off = (from - *anchor).num_days();
                let back = days_off.div_euclid(14) * 14;
                let mut pd = *anchor + Duration::days(back);
                while pd < from {
                    pd += Duration::days(14);
                }
                while pd <= to {
                    out.push(shift_for_weekend(pd));
                    pd += Duration::days(14);
                }
            }
            PaySchedule::Weekly { weekday } => {
                let mut pd = from;
                while pd.weekday() != *weekday {
                    pd += Duration::days(1);
                }
                while pd <= to {
                    out.push(shift_for_weekend(pd));
                    pd += Duration::days(7);
                }
            }
            PaySchedule::Irregular => {}
        }
        out.sort();
        out
    }

    /// The `n` most recent paydays strictly before `before`, newest first.
    pub fn paydays_before(&self, before: NaiveDate, n: usize) -> Vec<NaiveDate> {
        // 400 days back covers `n` for any cadence down to weekly.
        let from = before - Duration::days(400);
        let mut found: Vec<NaiveDate> = self
            .paydays_in(from, before)
            .into_iter()
            .filter(|d| *d < before)
            .collect();
        found.sort_by(|a, b| b.cmp(a));
        found.truncate(n);
        found
    }

    /// Count scheduled paydays in the inclusive range `[from, to]`.
    pub fn paydays_through(&self, from: NaiveDate, to: NaiveDate) -> u32 {
        self.paydays_in(from, to).len() as u32
    }
}

fn next_month(y: i32, m: u32) -> (i32, u32) {
    if m == 12 {
        (y + 1, 1)
    } else {
        (y, m + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn semi() -> PaySchedule {
        PaySchedule::SemiMonthly { days: vec![15, -1] }
    }

    #[test]
    fn last_day_handles_month_lengths() {
        assert_eq!(last_day_of_month(2026, 2), d(2026, 2, 28));
        assert_eq!(last_day_of_month(2026, 4), d(2026, 4, 30));
        assert_eq!(last_day_of_month(2026, 12), d(2026, 12, 31));
    }

    #[test]
    fn semi_monthly_weekend_paydays_shift_to_friday() {
        // 2026-08-15 Sat -> Fri the 14th
        let aug = semi().paydays_in(d(2026, 8, 1), d(2026, 8, 31));
        assert_eq!(aug[0], d(2026, 8, 14));
        // 2026-05-31 Sun -> Fri the 29th
        let may = semi().paydays_in(d(2026, 5, 1), d(2026, 5, 31));
        assert_eq!(may[1], d(2026, 5, 29));
    }

    #[test]
    fn two_paydays_before_a_due_date() {
        // due June 10th: two prior paydays are May 29 (shifted from Sun 31) and May 15
        let before = semi().paydays_before(d(2026, 6, 10), 2);
        assert_eq!(before, vec![d(2026, 5, 29), d(2026, 5, 15)]);
    }

    #[test]
    fn semi_monthly_counts_two_per_month() {
        assert_eq!(semi().paydays_through(d(2026, 1, 1), d(2026, 12, 31)), 24);
    }

    #[test]
    fn biweekly_steps_fourteen_days() {
        let s = PaySchedule::Biweekly {
            anchor: d(2026, 1, 2), // a Friday
        };
        let jan = s.paydays_in(d(2026, 1, 1), d(2026, 1, 31));
        assert_eq!(jan, vec![d(2026, 1, 2), d(2026, 1, 16), d(2026, 1, 30)]);
    }

    #[test]
    fn weekly_hits_one_weekday() {
        let s = PaySchedule::Weekly {
            weekday: Weekday::Fri,
        };
        let count = s.paydays_through(d(2026, 1, 1), d(2026, 1, 31));
        assert_eq!(count, 5); // five Fridays in Jan 2026
    }

    #[test]
    fn irregular_has_no_paydays() {
        assert_eq!(
            PaySchedule::Irregular.paydays_through(d(2026, 1, 1), d(2026, 12, 31)),
            0
        );
    }
}
