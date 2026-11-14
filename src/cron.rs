use std::fmt;

use thiserror::Error;

const SECONDS_PER_DAY: i64 = 86_400;
const SEARCH_DAYS: i64 = 366 * 9;
const MAX_TIMESTAMP: i64 = 253_402_300_799;

const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CronError {
    #[error(
        "a cron expression has exactly 5 fields (minute hour day-of-month month day-of-week), found {0}"
    )]
    FieldCount(usize),
    #[error("{field}: {detail}")]
    Field { field: &'static str, detail: String },
    #[error("the expression never fires: no matching date exists within 9 years")]
    NeverFires,
}

fn bad(field: &'static str, detail: impl Into<String>) -> CronError {
    CronError::Field {
        field,
        detail: detail.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days_of_month: u32,
    months: u16,
    days_of_week: u8,
    any_day_of_month: bool,
    any_day_of_week: bool,
}

struct Spec {
    field: &'static str,
    min: u32,
    max: u32,
    names: &'static [&'static str],
    name_base: u32,
}

fn parse_value(spec: &Spec, token: &str) -> Result<u32, CronError> {
    if token.is_empty() {
        return Err(bad(spec.field, "empty value"));
    }
    let value = if token.bytes().all(|b| b.is_ascii_digit()) {
        token
            .parse::<u32>()
            .map_err(|_| bad(spec.field, format!("`{token}` is out of range")))?
    } else {
        let lower = token.to_ascii_lowercase();
        let position = spec
            .names
            .iter()
            .position(|name| *name == lower)
            .ok_or_else(|| {
                bad(
                    spec.field,
                    format!("`{token}` is not a valid value or name"),
                )
            })?;
        u32::try_from(position).unwrap_or(0) + spec.name_base
    };
    if value < spec.min || value > spec.max {
        return Err(bad(
            spec.field,
            format!("{value} is outside {}-{}", spec.min, spec.max),
        ));
    }
    Ok(value)
}

fn parse_field(spec: &Spec, text: &str) -> Result<(u64, bool), CronError> {
    if text.is_empty() {
        return Err(bad(spec.field, "empty field"));
    }
    let mut bits = 0u64;
    for item in text.split(',') {
        if item.is_empty() {
            return Err(bad(spec.field, "empty list item"));
        }
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => (range, Some(step)),
            None => (item, None),
        };
        let (low, high) = if range == "*" {
            (spec.min, spec.max)
        } else if let Some((low, high)) = range.split_once('-') {
            let low = parse_value(spec, low)?;
            let high = parse_value(spec, high)?;
            if low > high {
                return Err(bad(spec.field, format!("range {low}-{high} is reversed")));
            }
            (low, high)
        } else {
            let value = parse_value(spec, range)?;
            if step.is_some() {
                return Err(bad(
                    spec.field,
                    format!("a step needs `*` or a range, not the single value `{range}`"),
                ));
            }
            (value, value)
        };
        let step = match step {
            None => 1,
            Some(step) => {
                if step.is_empty() || !step.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(bad(spec.field, format!("`{step}` is not a valid step")));
                }
                let step: u32 = step
                    .parse()
                    .map_err(|_| bad(spec.field, format!("step `{step}` is out of range")))?;
                if step == 0 {
                    return Err(bad(spec.field, "a step must be at least 1"));
                }
                if step > spec.max - spec.min + 1 {
                    return Err(bad(
                        spec.field,
                        format!("step {step} is larger than the field"),
                    ));
                }
                step
            }
        };
        let mut value = low;
        while value <= high {
            bits |= 1u64 << value;
            value += step;
        }
    }
    Ok((bits, text.starts_with('*')))
}

impl Cron {
    /// ```
    /// use inchworm::cron::Cron;
    ///
    /// let weekly = Cron::parse("0 3 * * sat").unwrap();
    /// assert_eq!(weekly.next_after(0), Some(2 * 86_400 + 3 * 3600));
    /// assert!(Cron::parse("0 3 * *").is_err());
    /// assert!(Cron::parse("0 0 30 2 *").is_err());
    /// ```
    pub fn parse(text: &str) -> Result<Cron, CronError> {
        let fields: Vec<&str> = text.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronError::FieldCount(fields.len()));
        }
        let (minutes, _) = parse_field(
            &Spec {
                field: "minute",
                min: 0,
                max: 59,
                names: &[],
                name_base: 0,
            },
            fields[0],
        )?;
        let (hours, _) = parse_field(
            &Spec {
                field: "hour",
                min: 0,
                max: 23,
                names: &[],
                name_base: 0,
            },
            fields[1],
        )?;
        let (days_of_month, any_day_of_month) = parse_field(
            &Spec {
                field: "day-of-month",
                min: 1,
                max: 31,
                names: &[],
                name_base: 0,
            },
            fields[2],
        )?;
        let (months, _) = parse_field(
            &Spec {
                field: "month",
                min: 1,
                max: 12,
                names: &MONTH_NAMES,
                name_base: 1,
            },
            fields[3],
        )?;
        let (mut days_of_week, any_day_of_week) = parse_field(
            &Spec {
                field: "day-of-week",
                min: 0,
                max: 7,
                names: &DAY_NAMES,
                name_base: 0,
            },
            fields[4],
        )?;
        if days_of_week & (1 << 7) != 0 {
            days_of_week = (days_of_week & !(1 << 7)) | 1;
        }
        let cron = Cron {
            minutes,
            hours: u32::try_from(hours).unwrap_or(0),
            days_of_month: u32::try_from(days_of_month).unwrap_or(0),
            months: u16::try_from(months).unwrap_or(0),
            days_of_week: u8::try_from(days_of_week).unwrap_or(0),
            any_day_of_month,
            any_day_of_week,
        };
        if cron.next_after(0).is_none() {
            return Err(CronError::NeverFires);
        }
        Ok(cron)
    }

    fn day_matches(&self, days: i64) -> bool {
        let (_, month, day) = civil_from_days(days);
        if self.months & (1 << month) == 0 {
            return false;
        }
        let of_month = self.days_of_month & (1 << day) != 0;
        let of_week = self.days_of_week & (1 << weekday(days)) != 0;
        if self.any_day_of_month || self.any_day_of_week {
            of_month && of_week
        } else {
            of_month || of_week
        }
    }

    /// ```
    /// use inchworm::cron::Cron;
    ///
    /// let every_quarter = Cron::parse("*/15 * * * *").unwrap();
    /// assert!(every_quarter.matches(900));
    /// assert!(!every_quarter.matches(901));
    /// assert!(!every_quarter.matches(960));
    /// ```
    pub fn matches(&self, timestamp: i64) -> bool {
        if !(0..=MAX_TIMESTAMP).contains(&timestamp) || timestamp % 60 != 0 {
            return false;
        }
        let days = timestamp.div_euclid(SECONDS_PER_DAY);
        let minute_of_day = timestamp.rem_euclid(SECONDS_PER_DAY) / 60;
        self.day_matches(days)
            && self.hours & (1 << (minute_of_day / 60)) != 0
            && self.minutes & (1 << (minute_of_day % 60)) != 0
    }

    /// ```
    /// use inchworm::cron::Cron;
    ///
    /// let hourly = Cron::parse("30 * * * *").unwrap();
    /// assert_eq!(hourly.next_after(0), Some(1800));
    /// assert_eq!(hourly.next_after(1800), Some(5400));
    /// ```
    pub fn next_after(&self, timestamp: i64) -> Option<i64> {
        if !(0..=MAX_TIMESTAMP).contains(&timestamp) {
            return None;
        }
        let start = (timestamp.div_euclid(60) + 1) * 60;
        let first_day = start.div_euclid(SECONDS_PER_DAY);
        for days in first_day..first_day + SEARCH_DAYS {
            if !self.day_matches(days) {
                continue;
            }
            let from = if days == first_day {
                start.rem_euclid(SECONDS_PER_DAY) / 60
            } else {
                0
            };
            for hour in from / 60..24 {
                if self.hours & (1 << hour) == 0 {
                    continue;
                }
                let low = if hour == from / 60 { from % 60 } else { 0 };
                let candidates = self.minutes >> low;
                if candidates != 0 {
                    let minute = low + i64::from(candidates.trailing_zeros());
                    return Some(days * SECONDS_PER_DAY + hour * 3600 + minute * 60);
                }
            }
        }
        None
    }

    /// ```
    /// use inchworm::cron::Cron;
    ///
    /// let hourly = Cron::parse("30 * * * *").unwrap();
    /// assert_eq!(hourly.due(5400, 0, None), Some(5400));
    /// assert_eq!(hourly.due(5400, 0, Some(5400)), None);
    /// assert_eq!(hourly.due(9000, 0, Some(1800)), Some(9000));
    /// assert_eq!(hourly.due(9000, 9001, None), None);
    /// ```
    pub fn due(&self, now: i64, first_seen: i64, last_fired: Option<i64>) -> Option<i64> {
        let tick = self.latest_at_or_before(now)?;
        (tick >= first_seen && last_fired.is_none_or(|last| tick > last)).then_some(tick)
    }

    /// ```
    /// use inchworm::cron::Cron;
    ///
    /// let hourly = Cron::parse("30 * * * *").unwrap();
    /// assert_eq!(hourly.latest_at_or_before(5400), Some(5400));
    /// assert_eq!(hourly.latest_at_or_before(5399), Some(1800));
    /// assert_eq!(hourly.latest_at_or_before(1799), None);
    /// ```
    pub fn latest_at_or_before(&self, timestamp: i64) -> Option<i64> {
        if !(0..=MAX_TIMESTAMP).contains(&timestamp) {
            return None;
        }
        let end = timestamp.div_euclid(60) * 60;
        let last_day = end.div_euclid(SECONDS_PER_DAY);
        for days in (last_day - SEARCH_DAYS + 1..=last_day).rev() {
            if days < 0 {
                return None;
            }
            if !self.day_matches(days) {
                continue;
            }
            let until = if days == last_day {
                end.rem_euclid(SECONDS_PER_DAY) / 60
            } else {
                1439
            };
            for hour in (0..=until / 60).rev() {
                if self.hours & (1 << hour) == 0 {
                    continue;
                }
                let high = if hour == until / 60 { until % 60 } else { 59 };
                let candidates = self.minutes & (u64::MAX >> (63 - high));
                if candidates != 0 {
                    let minute = 63 - i64::from(candidates.leading_zeros());
                    return Some(days * SECONDS_PER_DAY + hour * 3600 + minute * 60);
                }
            }
        }
        None
    }
}

pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn weekday(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

pub struct UtcStamp(pub i64);

impl fmt::Display for UtcStamp {
    /// ```
    /// use inchworm::cron::UtcStamp;
    ///
    /// assert_eq!(UtcStamp(0).to_string(), "1970-01-01 00:00 UTC");
    /// assert_eq!(UtcStamp(1_795_057_200).to_string(), "2026-11-19 03:00 UTC");
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.0.div_euclid(SECONDS_PER_DAY);
        let minute_of_day = self.0.rem_euclid(SECONDS_PER_DAY) / 60;
        let (year, month, day) = civil_from_days(days);
        write!(
            f,
            "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
            minute_of_day / 60,
            minute_of_day % 60
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn at(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> i64 {
        days_from_civil(year, month, day) * SECONDS_PER_DAY + hour * 3600 + minute * 60
    }

    fn cron(text: &str) -> Cron {
        Cron::parse(text).unwrap()
    }

    fn err(text: &str) -> String {
        Cron::parse(text).unwrap_err().to_string()
    }

    #[test]
    fn civil_conversion_round_trips_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(days_from_civil(2000, 2, 29)), (2000, 2, 29));
        assert_eq!(
            civil_from_days(days_from_civil(2026, 11, 14)),
            (2026, 11, 14)
        );
        assert_eq!(weekday(days_from_civil(2026, 11, 14)), 6);
        assert_eq!(weekday(days_from_civil(2026, 11, 15)), 0);
        assert_eq!(weekday(0), 4);
    }

    #[test]
    fn every_valid_form_parses() {
        for text in [
            "* * * * *",
            "0 0 1 1 *",
            "59 23 31 12 7",
            "*/5 */2 */3 */4 */2",
            "0,15,30,45 * * * *",
            "10-20/5 1-5 1-15 jan-jun mon-fri",
            "0 3 * * SAT",
            "0 3 * * Sun",
            "0 0 1 JAN,Mar,dec *",
            "0 0 * * 0,7",
            "0 0 * * 5-7",
            "  0   3\t* * 6  ",
            "*/60 * * * *",
            "0 0 29 2 *",
            "0 0 31 * *",
            "*/1 * * * *",
            "0 0 * * */7",
        ] {
            assert!(Cron::parse(text).is_ok(), "{text}");
        }
    }

    #[test]
    fn every_malformed_form_is_rejected_with_the_field_named() {
        let cases = [
            ("", "exactly 5 fields"),
            ("* * * *", "exactly 5 fields"),
            ("* * * * * *", "exactly 5 fields"),
            ("@daily", "exactly 5 fields"),
            ("@weekly *  * * *", "minute"),
            ("60 * * * *", "minute: 60 is outside 0-59"),
            ("* 24 * * *", "hour: 24 is outside 0-23"),
            ("* * 0 * *", "day-of-month: 0 is outside 1-31"),
            ("* * 32 * *", "day-of-month: 32 is outside 1-31"),
            ("* * * 0 *", "month: 0 is outside 1-12"),
            ("* * * 13 *", "month: 13 is outside 1-12"),
            ("* * * * 8", "day-of-week: 8 is outside 0-7"),
            ("*/0 * * * *", "step must be at least 1"),
            ("*/61 * * * *", "larger than the field"),
            ("*/ * * * *", "not a valid step"),
            ("*/x * * * *", "not a valid step"),
            ("*/-1 * * * *", "not a valid step"),
            ("5/10 * * * *", "step needs"),
            ("30-10 * * * *", "reversed"),
            ("5-3/2 * * * *", "reversed"),
            ("1,,2 * * * *", "empty list item"),
            (",1 * * * *", "empty list item"),
            ("1, * * * *", "empty list item"),
            ("1- * * * *", "empty value"),
            ("-5 * * * *", "empty value"),
            ("- * * * *", "empty value"),
            ("? * * * *", "not a valid value"),
            ("* * L * *", "not a valid value"),
            ("* * * * 5#2", "not a valid value"),
            ("* * * foo *", "not a valid value"),
            ("* * * * funday", "not a valid value"),
            ("* * * mon *", "not a valid value"),
            ("* * * * jan", "not a valid value"),
            ("* * * * mon-", "empty value"),
            ("+5 * * * *", "not a valid value"),
            ("99999999999999 * * * *", "out of range"),
            ("*/99999999999999 * * * *", "out of range"),
            ("1.5 * * * *", "not a valid value"),
            ("*-5 * * * *", "not a valid value"),
            ("1-2-3 * * * *", "not a valid value"),
            ("*/2/3 * * * *", "not a valid step"),
        ];
        for (text, expect) in cases {
            let message = err(text);
            assert!(message.contains(expect), "{text:?}: {message}");
        }
    }

    #[test]
    fn expressions_that_never_fire_are_rejected() {
        for text in [
            "0 0 30 2 *",
            "0 0 31 4 *",
            "0 0 31 2,4,6,9,11 *",
            "0 0 30 feb *",
        ] {
            assert_eq!(Cron::parse(text), Err(CronError::NeverFires), "{text}");
        }
        assert!(Cron::parse("0 0 29 2 *").is_ok());
        assert!(Cron::parse("0 0 30 2 6").is_ok());
    }

    #[test]
    fn weekly_saturday_and_sunday_schedules_fire_on_saturday_and_sunday_at_0300_utc() {
        let scout = cron("0 3 * * 6");
        let prover = cron("0 3 * * 0");
        let saturday = at(2026, 11, 14, 3, 0);
        assert_eq!(scout.next_after(saturday - 1), Some(saturday));
        assert_eq!(scout.next_after(saturday), Some(saturday + 7 * 86_400));
        assert_eq!(prover.next_after(saturday), Some(at(2026, 11, 15, 3, 0)));
        assert_eq!(
            prover.next_after(at(2026, 11, 15, 3, 0)),
            Some(at(2026, 11, 22, 3, 0))
        );
        assert!(scout.matches(saturday));
        assert!(!scout.matches(saturday + 60));
        assert!(!prover.matches(saturday));
    }

    #[test]
    fn sunday_is_zero_or_seven_and_names_are_case_insensitive() {
        assert_eq!(cron("0 0 * * 0"), cron("0 0 * * 7"));
        assert_eq!(cron("0 0 * * 0"), cron("0 0 * * SUN"));
        assert_eq!(cron("0 0 * * 0"), cron("0 0 * * sun"));
        assert_eq!(cron("0 0 1 1 *"), cron("0 0 1 JAN *"));
        assert_eq!(cron("0 0 * * 5-7"), cron("0 0 * * fri,sat,sun"));
        assert_eq!(cron("0 0 * * mon-fri"), cron("0 0 * * 1-5"));
    }

    #[test]
    fn day_of_month_and_day_of_week_are_ored_when_both_are_restricted() {
        let friday_or_13th = cron("0 0 13 * 5");
        assert!(friday_or_13th.matches(at(2026, 11, 13, 0, 0)));
        assert!(friday_or_13th.matches(at(2026, 11, 6, 0, 0)));
        assert!(friday_or_13th.matches(at(2026, 12, 13, 0, 0)));
        assert!(!friday_or_13th.matches(at(2026, 11, 14, 0, 0)));
        let only_13th = cron("0 0 13 * *");
        assert!(!only_13th.matches(at(2026, 11, 6, 0, 0)));
        let only_friday = cron("0 0 * * 5");
        assert!(!only_friday.matches(at(2026, 12, 13, 0, 0)));
        let star_step = cron("0 0 */2 * 5");
        assert!(!star_step.matches(at(2026, 11, 6, 0, 0)));
        assert!(star_step.matches(at(2026, 11, 13, 0, 0)));
    }

    #[test]
    fn rollovers_and_leap_days_are_exact() {
        assert_eq!(
            cron("59 23 31 12 *").next_after(at(2026, 12, 31, 23, 59)),
            Some(at(2027, 12, 31, 23, 59))
        );
        assert_eq!(
            cron("0 0 * * *").next_after(at(2026, 12, 31, 12, 0)),
            Some(at(2027, 1, 1, 0, 0))
        );
        assert_eq!(
            cron("0 0 29 2 *").next_after(at(2026, 1, 1, 0, 0)),
            Some(at(2028, 2, 29, 0, 0))
        );
        assert_eq!(
            cron("0 0 29 2 *").next_after(at(2096, 3, 1, 0, 0)),
            Some(at(2104, 2, 29, 0, 0))
        );
        assert_eq!(
            cron("0 0 31 * *").next_after(at(2026, 1, 31, 0, 0)),
            Some(at(2026, 3, 31, 0, 0))
        );
        assert_eq!(
            cron("30 2 * * *").next_after(at(2026, 3, 29, 2, 30)),
            Some(at(2026, 3, 30, 2, 30))
        );
    }

    #[test]
    fn next_after_is_strict_and_ignores_seconds_within_the_minute() {
        let every_minute = cron("* * * * *");
        assert_eq!(every_minute.next_after(0), Some(60));
        assert_eq!(every_minute.next_after(59), Some(60));
        assert_eq!(every_minute.next_after(60), Some(120));
        let hourly = cron("0 * * * *");
        assert_eq!(hourly.next_after(3599), Some(3600));
        assert_eq!(hourly.next_after(3600), Some(7200));
    }

    #[test]
    fn out_of_range_timestamps_are_handled_without_panicking() {
        let any = cron("* * * * *");
        assert_eq!(any.next_after(-1), None);
        assert_eq!(any.next_after(i64::MAX), None);
        assert_eq!(any.latest_at_or_before(-1), None);
        assert_eq!(any.latest_at_or_before(i64::MAX), None);
        assert!(!any.matches(-60));
        assert!(!any.matches(i64::MAX));
        assert!(any.next_after(MAX_TIMESTAMP - 3600).is_some());
    }

    #[test]
    fn latest_at_or_before_finds_the_most_recent_tick_across_days() {
        let scout = cron("0 3 * * 6");
        let saturday = at(2026, 11, 14, 3, 0);
        assert_eq!(scout.latest_at_or_before(saturday), Some(saturday));
        assert_eq!(
            scout.latest_at_or_before(saturday + 3 * 86_400),
            Some(saturday)
        );
        assert_eq!(
            scout.latest_at_or_before(saturday - 1),
            Some(saturday - 7 * 86_400)
        );
        let quarter = cron("*/15 9-10 * * *");
        assert_eq!(
            quarter.latest_at_or_before(at(2026, 11, 14, 12, 0)),
            Some(at(2026, 11, 14, 10, 45))
        );
        assert_eq!(
            quarter.latest_at_or_before(at(2026, 11, 14, 8, 0)),
            Some(at(2026, 11, 13, 10, 45))
        );
        assert_eq!(
            cron("0 0 1 1 *").latest_at_or_before(at(1970, 6, 1, 0, 0)),
            Some(0)
        );
        assert_eq!(
            cron("0 0 2 1 *").latest_at_or_before(at(1970, 1, 1, 12, 0)),
            None
        );
    }

    #[test]
    fn due_fires_the_latest_tick_once_and_never_backfills() {
        let scout = cron("0 3 * * 6");
        let saturday = at(2026, 11, 14, 3, 0);
        let seen = saturday - 3 * 86_400;
        assert_eq!(scout.due(saturday - 1, seen, None), None);
        assert_eq!(scout.due(saturday, seen, None), Some(saturday));
        assert_eq!(scout.due(saturday + 5 * 86_400, seen, None), Some(saturday));
        assert_eq!(scout.due(saturday + 5 * 86_400, seen, Some(saturday)), None);
        let three_weeks = saturday + 21 * 86_400;
        assert_eq!(
            scout.due(three_weeks + 100, seen, Some(saturday)),
            Some(three_weeks)
        );
        assert_eq!(scout.due(saturday + 60, saturday + 30, None), None);
        assert_eq!(scout.due(saturday, saturday, None), Some(saturday));
    }

    #[test]
    fn utc_stamps_render_fixed_width() {
        assert_eq!(
            UtcStamp(at(2026, 11, 14, 3, 5)).to_string(),
            "2026-11-14 03:05 UTC"
        );
    }

    fn cron_strategy() -> impl Strategy<Value = Cron> {
        (
            prop_oneof![
                Just("*".to_string()),
                (0u32..60).prop_map(|n| n.to_string()),
                (1u32..30).prop_map(|n| format!("*/{n}")),
                (0u32..30, 0u32..30).prop_map(|(a, b)| format!("{}-{}", a.min(b), a.max(b))),
                Just("0,15,30,45".to_string()),
            ],
            prop_oneof![
                Just("*".to_string()),
                (0u32..24).prop_map(|n| n.to_string()),
                (1u32..12).prop_map(|n| format!("*/{n}")),
            ],
            prop_oneof![
                Just("*".to_string()),
                (1u32..29).prop_map(|n| n.to_string()),
                Just("1,15".to_string()),
            ],
            prop_oneof![
                Just("*".to_string()),
                (1u32..13).prop_map(|n| n.to_string()),
                Just("jan-mar".to_string()),
            ],
            prop_oneof![
                Just("*".to_string()),
                (0u32..8).prop_map(|n| n.to_string()),
                Just("mon-fri".to_string()),
            ],
        )
            .prop_map(|(m, h, d, mo, w)| Cron::parse(&format!("{m} {h} {d} {mo} {w}")).unwrap())
    }

    proptest! {
        #[test]
        fn next_after_is_later_and_matches(cron in cron_strategy(), t in 0i64..4_102_444_800) {
            let next = cron.next_after(t).unwrap();
            prop_assert!(next > t);
            prop_assert!(cron.matches(next));
            prop_assert_eq!(next % 60, 0);
        }

        #[test]
        fn nothing_matches_strictly_between_t_and_next(cron in cron_strategy(), t in 0i64..4_102_444_800) {
            let next = cron.next_after(t).unwrap();
            let window_start = (t.div_euclid(60) + 1) * 60;
            let mut probe = window_start;
            let mut checked = 0;
            while probe < next && checked < 2000 {
                prop_assert!(!cron.matches(probe), "{} matched before {}", probe, next);
                probe += 60;
                checked += 1;
            }
        }

        #[test]
        fn latest_at_or_before_is_the_inverse_of_next_after(cron in cron_strategy(), t in 0i64..4_102_444_800) {
            if let Some(latest) = cron.latest_at_or_before(t) {
                prop_assert!(latest <= t);
                prop_assert!(cron.matches(latest));
                prop_assert!(cron.next_after(latest).unwrap() > t);
            }
        }

        #[test]
        fn civil_conversion_round_trips(days in -1_000_000i64..3_000_000) {
            let (year, month, day) = civil_from_days(days);
            prop_assert_eq!(days_from_civil(year, month, day), days);
            prop_assert!((1..=12).contains(&month) && (1..=31).contains(&day));
        }

        #[test]
        fn arbitrary_text_never_panics(text in ".*") {
            let _ = Cron::parse(&text);
        }

        #[test]
        fn five_random_tokens_never_panic(
            fields in prop::collection::vec("[0-9a-z*/,\\-]{0,6}", 5)
        ) {
            let _ = Cron::parse(&fields.join(" "));
        }
    }
}
