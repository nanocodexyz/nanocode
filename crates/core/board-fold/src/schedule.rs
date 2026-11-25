use serde::{Deserialize, Serialize};

use crate::sim::{Mutation, mutated};

const MINUTE_MS: i64 = 60_000;
const DAY_MINUTES: i64 = 1_440;
const CYCLE_DAYS: i64 = 146_097;
const OFFSET_MINUTES_MAX: i16 = 23 * 60 + 59;
const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Recur {
    Every {
        every_ms: u64,
    },
    Cron {
        cron: String,
        #[serde(default)]
        offset_minutes: i16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retry {
    pub max_attempts: u64,
    pub backoff_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Invalid {
    Interval,
    Offset(i16),
    Cron(String),
    Never,
    Attempts,
    Backoff,
    Unscheduled,
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    any_day: bool,
    any_weekday: bool,
}

fn field(text: &str, low: u32, high: u32, names: &[&str]) -> Result<u64, String> {
    let mut bits = 0u64;
    for item in text.split(',') {
        let (base, step) = match item.split_once('/') {
            Some((base, step)) => (
                base,
                step.parse::<u32>()
                    .ok()
                    .filter(|step| *step > 0)
                    .ok_or_else(|| format!("bad step in {item:?}"))?,
            ),
            None => (item, 1),
        };
        let number = |value: &str| {
            names
                .iter()
                .position(|name| name.eq_ignore_ascii_case(value))
                .map(|at| at as u32 + low + u32::from(mutated(Mutation::CronNamesShifted)))
                .or_else(|| value.parse::<u32>().ok())
                .filter(|value| (low..=high).contains(value))
                .ok_or_else(|| format!("{value:?} is outside {low}-{high}"))
        };
        let (from, to) = match base.split_once('-') {
            _ if base == "*" => (low, high),
            Some((from, to)) => (number(from)?, number(to)?),
            None if step > 1 => (number(base)?, high),
            None => {
                let at = number(base)?;
                (at, at)
            }
        };
        if from > to {
            return Err(format!("{item:?} runs backwards"));
        }
        let mut at = from;
        while at <= to {
            bits |= 1 << at;
            let Some(next) = at.checked_add(step) else {
                break;
            };
            at = next;
        }
    }
    Ok(bits)
}

pub const fn civil(days: i64) -> (u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(CYCLE_DAYS);
    let of_era = shifted - era * CYCLE_DAYS;
    let year = (of_era - of_era / 1_460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year + year / 4 - year / 100);
    let shifted_month = (5 * of_year + 2) / 153;
    let day = of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (month as u32, day as u32)
}

impl Cron {
    pub fn parse(text: &str) -> Result<Self, String> {
        let fields = text.split_whitespace().collect::<Vec<_>>();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(format!("{} fields, not 5", fields.len()));
        };
        let weekdays = field(weekday, 0, 7, &WEEKDAYS)?;
        Ok(Self {
            minutes: field(minute, 0, 59, &[])?,
            hours: field(hour, 0, 23, &[])? as u32,
            days: field(day, 1, 31, &[])? as u32,
            months: field(month, 1, 12, &MONTHS)? as u16,
            weekdays: ((weekdays | weekdays >> 7) & 0x7f) as u8,
            any_day: day.starts_with('*'),
            any_weekday: weekday.starts_with('*'),
        })
    }

    const fn on_day(&self, days: i64) -> bool {
        let (month, day) = civil(days);
        let weekday = (days + 4).rem_euclid(7);
        let by_day = self.days & 1 << day != 0;
        let by_weekday = self.weekdays & 1 << weekday != 0;
        self.months & 1 << month != 0
            && match (self.any_day, self.any_weekday) {
                (false, false) => by_day || by_weekday,
                _ => by_day && by_weekday,
            }
    }

    pub const fn matches(&self, local_minute: i64) -> bool {
        let of_day = local_minute.rem_euclid(DAY_MINUTES);
        self.on_day(local_minute.div_euclid(DAY_MINUTES))
            && self.hours & 1 << (of_day / 60) != 0
            && self.minutes & 1 << (of_day % 60) != 0
    }

    fn first(&self, of_day: i64) -> Option<i64> {
        (of_day..DAY_MINUTES)
            .find(|at| self.hours & 1 << (at / 60) != 0 && self.minutes & 1 << (at % 60) != 0)
    }

    pub fn next_after(&self, after_ms: u64, offset_minutes: i16) -> Option<u64> {
        let offset = i64::from(offset_minutes) * MINUTE_MS;
        let local = i64::try_from(after_ms).ok()?.checked_add(offset)?;
        let start = local.div_euclid(MINUTE_MS).checked_add(1)?;
        let day = start.div_euclid(DAY_MINUTES);
        let mut of_day = start.rem_euclid(DAY_MINUTES);
        for days in day..=day.checked_add(CYCLE_DAYS)? {
            if self.on_day(days)
                && let Some(at) = self.first(of_day)
            {
                let minute = days.checked_mul(DAY_MINUTES)?.checked_add(at)?;
                let utc = minute.checked_mul(MINUTE_MS)?.checked_sub(offset)?;
                return u64::try_from(utc).ok();
            }
            of_day = 0;
        }
        None
    }
}

impl Recur {
    pub fn next_after(&self, after_ms: u64) -> Result<u64, Invalid> {
        match self {
            Self::Every { every_ms: 0 } => Err(Invalid::Interval),
            Self::Every { every_ms } => after_ms.checked_add(*every_ms).ok_or(Invalid::Overflow),
            Self::Cron { offset_minutes, .. } if offset_minutes.abs() > OFFSET_MINUTES_MAX => {
                Err(Invalid::Offset(*offset_minutes))
            }
            Self::Cron {
                cron,
                offset_minutes,
            } => Cron::parse(cron)
                .map_err(Invalid::Cron)?
                .next_after(after_ms, *offset_minutes)
                .ok_or(Invalid::Never),
        }
    }

    pub fn check(&self, due_at_ms: Option<u64>) -> Result<(), Invalid> {
        self.next_after(due_at_ms.ok_or(Invalid::Unscheduled)?)
            .map(drop)
    }
}

impl Retry {
    pub fn delay(&self, attempts: u64) -> Option<u64> {
        let doublings = u32::try_from(attempts.checked_sub(1)?).ok()?;
        self.backoff_ms.checked_mul(1u64.checked_shl(doublings)?)
    }

    pub fn check(&self) -> Result<(), Invalid> {
        match self.max_attempts {
            0 => Err(Invalid::Attempts),
            1 => Ok(()),
            most => self.delay(most - 1).map(drop).ok_or(Invalid::Backoff),
        }
    }
}
