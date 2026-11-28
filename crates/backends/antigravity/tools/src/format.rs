use std::time::{SystemTime, UNIX_EPOCH};

pub const TRUNCATE_LIMIT: usize = 10_240;

pub const REMINDER: &str = "Reminder: work efficiently. In this turn, issue in parallel every independent search, read and edit or write you need next. Never one call per turn; never repeat a search or re-read content you already know that hasn't changed. Working efficiently never means skipping checks to save turns; verify in one command after your last edit and don't re-run passing tests or diffs without new edits.";

const SYSTEM_MESSAGE_LEAD: &str = "The following is a <SYSTEM_MESSAGE> not actually sent by the user. It is provided by the system as important information to pay attention to.";

pub fn system_message(text: &str) -> String {
    format!("{SYSTEM_MESSAGE_LEAD}\n\n<SYSTEM_MESSAGE>\n{text}\n</SYSTEM_MESSAGE>")
}

pub fn with_header(created: SystemTime, completed: SystemTime, body: &str) -> String {
    format!(
        "Created At: {}\nCompleted At: {}\n{body}",
        timestamp(created),
        timestamp(completed)
    )
}

pub fn truncate_output(full: &str, file: &str) -> String {
    truncate(full, Some(file))
}

pub fn truncate(full: &str, file: Option<&str>) -> String {
    if full.len() <= TRUNCATE_LIMIT {
        return full.to_owned();
    }
    cut_middle(full, full, full.len() as u64, file)
}

#[derive(Clone, Default)]
pub(crate) struct Preview {
    head: String,
    tail: String,
    total: u64,
}

const KEEP: usize = TRUNCATE_LIMIT / 2;

impl Preview {
    pub(crate) const fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub(crate) fn push(&mut self, chunk: &str) {
        self.total += chunk.len() as u64;
        if crate::sim::mutated(crate::sim::Mutation::OutputKept) {
            self.head.push_str(chunk);
            return;
        }
        let mut rest = chunk;
        if self.tail.is_empty() {
            let room = floor_boundary(chunk, KEEP.saturating_sub(self.head.len()));
            let (head, after) = chunk.split_at(room);
            self.head.push_str(head);
            rest = after;
        }
        if rest.len() > 2 * KEEP {
            self.tail.clear();
            rest = &rest[ceil_boundary(rest, rest.len() - KEEP)..];
        }
        self.tail.push_str(rest);
        if self.tail.len() > 2 * KEEP {
            let from = ceil_boundary(&self.tail, self.tail.len() - KEEP);
            self.tail.drain(..from);
        }
    }

    pub(crate) fn render(&self, file: Option<&str>) -> String {
        if crate::sim::mutated(crate::sim::Mutation::OutputKept) {
            return self.head.clone();
        }
        if self.total <= TRUNCATE_LIMIT as u64 {
            return format!("{}{}", self.head, self.tail);
        }
        cut_middle(&self.head, &self.tail, self.total, file)
    }
}

fn cut_middle(head: &str, tail: &str, total: u64, file: Option<&str>) -> String {
    let saved = file
        .map(|file| format!(" You can find the full output in a file at file://{file}."))
        .unwrap_or_default();
    let prefix = format!(
        "The tool's output was truncated due to being too long.{saved}

Here is a preview with the middle of the output truncated:
"
    );
    let mut half = 0;
    let mut marker = String::new();
    for _ in 0..3 {
        let cut = total.saturating_sub((half * 2) as u64);
        marker = format!(
            "
<truncated {cut} of {total} bytes from the middle of the output>
"
        );
        half = TRUNCATE_LIMIT.saturating_sub(prefix.len() + marker.len()) / 2;
    }
    let head_end = floor_boundary(head, half);
    let tail_start = ceil_boundary(tail, tail.len() - half);
    format!(
        "{prefix}{}{marker}{}",
        &head[..head_end],
        &tail[tail_start..]
    )
}

const fn floor_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

const fn ceil_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn unix_seconds(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
    })
}

pub fn timestamp(at: SystemTime) -> String {
    civil(unix_seconds(at), 0)
}

pub fn local_timestamp(at: SystemTime) -> String {
    civil(
        unix_seconds(at),
        i64::from(nanocode_platform::utc_offset_seconds(at)),
    )
}

fn civil(seconds: i64, offset: i64) -> String {
    let local = seconds + offset;
    let days = local.div_euclid(86_400);
    let day = local.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let of_era = shifted.rem_euclid(146_097);
    let year_of_era = (of_era - of_era / 1_460 + of_era / 36_524 - of_era / 146_096) / 365;
    let day_of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let date = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    let zone = match offset {
        0 => "Z".to_owned(),
        _ => {
            let sign = if offset < 0 { '-' } else { '+' };
            let minutes = offset.abs() / 60;
            format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
        }
    };
    format!(
        "{year:04}-{month:02}-{date:02}T{:02}:{:02}:{:02}{zone}",
        day / 3600,
        day / 60 % 60,
        day % 60
    )
}

pub fn go_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value)
        .unwrap_or_default()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

pub fn duration_label(seconds: u64) -> String {
    let parts = [
        (seconds / 3600, "h"),
        (seconds / 60 % 60, "m"),
        (seconds % 60, "s"),
    ];
    let label = parts
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, unit)| format!("{count}{unit}"))
        .collect::<String>();
    match label.is_empty() {
        true => "0s".to_owned(),
        false => label,
    }
}
