//! `time` and `times`: what a pipeline took and what the shell and its children have
//! used, in bash's formats.

use std::time::Duration;

use crate::host::CpuTimes;
use crate::state::Vp;

/// bash's format when `TIMEFORMAT` is unset.
const DEFAULT_FORMAT: &[u8] = b"\nreal\t%3lR\nuser\t%3lU\nsys\t%3lS";
/// `time -p`.
const POSIX_FORMAT: &[u8] = b"real %2R\nuser %2U\nsys %2S";

/// The clocks `time` reads before its pipeline.
pub(crate) struct Start {
    wall: Duration,
    cpu: CpuTimes,
}

pub(crate) fn start(vp: &Vp) -> Start {
    Start {
        wall: vp.core.host.now(),
        cpu: vp.core.host.cpu_times(),
    }
}

/// Writes what the pipeline took to the shell's stderr, as bash does: the POSIX lines
/// for `time -p`, else `TIMEFORMAT` when it is set (nothing when it is empty), else the
/// three lines with a blank line before them.
pub(crate) async fn report(vp: &mut Vp, posix: bool, start: Start) {
    let real = vp.core.host.now().saturating_sub(start.wall);
    let cpu = vp.core.host.cpu_times();
    let user =
        (cpu.user + cpu.children_user).saturating_sub(start.cpu.user + start.cpu.children_user);
    let system = (cpu.system + cpu.children_system)
        .saturating_sub(start.cpu.system + start.cpu.children_system);
    let format = if posix {
        POSIX_FORMAT.to_vec()
    } else {
        match vp.vars.get("TIMEFORMAT").and_then(|v| v.get()) {
            Some(format) => format.to_vec(),
            None => DEFAULT_FORMAT.to_vec(),
        }
    };
    if format.is_empty() {
        return;
    }
    match render(&format, real, user, system) {
        Ok(mut line) => {
            line.push(b'\n');
            vp.error_raw(&line).await;
        }
        Err(c) => {
            let c = char::from(c);
            vp.error(&format!("TIMEFORMAT: `{c}': invalid format character"))
                .await;
        }
    }
}

/// `TIMEFORMAT`: `%[p][l]R`, `U` and `S` with `p` digits after the point (three at most, and
/// by default) and `l` for minutes, `%P` for the CPU percentage, `%%`. The error is the
/// character after a `%` that is none of these.
fn render(format: &[u8], real: Duration, user: Duration, system: Duration) -> Result<Vec<u8>, u8> {
    let mut out = Vec::with_capacity(format.len() + 16);
    let mut rest = format.iter().copied().peekable();
    while let Some(b) = rest.next() {
        if b != b'%' {
            out.push(b);
            continue;
        }
        match rest.next() {
            None => out.push(b'%'),
            Some(b'%') => out.push(b'%'),
            Some(b'P') => {
                let cpu = ((user + system).as_micros() * 10_000)
                    .checked_div(real.as_micros())
                    .unwrap_or(0);
                out.extend(format!("{}.{:02}", cpu / 100, cpu % 100).bytes());
            }
            Some(mut c) => {
                let mut precision = 3;
                if c.is_ascii_digit() {
                    precision = usize::from(c - b'0').min(3);
                    c = rest.next().unwrap_or(0);
                }
                let long = c == b'l';
                if long {
                    c = rest.next().unwrap_or(0);
                }
                let time = match c {
                    b'R' | b'E' => real,
                    b'U' => user,
                    b'S' => system,
                    _ => return Err(c),
                };
                clock(&mut out, precision, long, time);
            }
        }
    }
    Ok(out)
}

/// A time as bash prints it: rounded to milliseconds, then `precision` of their digits;
/// `long` is `MmS.FFFs`.
pub(crate) fn clock(out: &mut Vec<u8>, precision: usize, long: bool, time: Duration) {
    let millis = (time.as_micros() + 500) / 1000;
    let mut secs = millis / 1000;
    if long {
        out.extend(format!("{}m", secs / 60).bytes());
        secs %= 60;
    }
    out.extend(secs.to_string().bytes());
    if precision > 0 {
        out.push(b'.');
        out.extend(format!("{:03}", millis % 1000).bytes().take(precision));
    }
    if long {
        out.push(b's');
    }
}
