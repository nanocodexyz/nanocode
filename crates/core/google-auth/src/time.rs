use std::time::{Duration, SystemTime};

const SECONDS_PER_DAY: i64 = 86_400;
const SECONDS_PER_HOUR: i64 = 3_600;
const SECONDS_PER_MINUTE: i64 = 60;
const NANOS_PER_SECOND: u32 = 1_000_000_000;
const FRACTION_DIGITS: usize = 9;

fn number(text: &str) -> Option<i64> {
    text.bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| text.parse().ok())
        .flatten()
}

pub fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let (date, rest) = text.split_once(['T', 't'])?;
    let mut date = date.splitn(3, '-');
    let year = number(date.next()?)?;
    let month = number(date.next()?)?;
    let day = number(date.next()?)?;
    let (clock, offset) = match rest.strip_suffix(['Z', 'z']) {
        Some(clock) => (clock, 0),
        None => {
            let split = rest.rfind(['+', '-'])?;
            let (clock, zone) = rest.split_at(split);
            let (hours, minutes) = zone[1..].split_once(':')?;
            let seconds = number(hours)? * SECONDS_PER_HOUR + number(minutes)? * SECONDS_PER_MINUTE;
            (
                clock,
                if zone.starts_with('-') {
                    -seconds
                } else {
                    seconds
                },
            )
        }
    };
    let (clock, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut clock = clock.splitn(3, ':');
    let hour = number(clock.next()?)?;
    let minute = number(clock.next()?)?;
    let second = number(clock.next()?)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    if fraction.len() > FRACTION_DIGITS || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let padded = format!("{fraction:0<FRACTION_DIGITS$}");
    assert_eq!(
        padded.len(),
        FRACTION_DIGITS,
        "a checked fraction pads to exactly its digits"
    );
    let nanos = padded.parse::<u32>().ok()?;
    let seconds = nanocode_platform::days_from_civil(year, month, day) * SECONDS_PER_DAY
        + hour * SECONDS_PER_HOUR
        + minute * SECONDS_PER_MINUTE
        + second
        - offset;
    let seconds = u64::try_from(seconds).ok()?;
    (nanos < NANOS_PER_SECOND).then(|| SystemTime::UNIX_EPOCH + Duration::new(seconds, nanos))
}
