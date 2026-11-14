#[cfg(windows)]
mod crt;
#[cfg(windows)]
mod layout;
#[cfg(windows)]
mod runtime;
#[cfg(windows)]
mod win32;

use std::{
    sync::mpsc,
    time::Duration,
};

pub type Outcome = Result<String, String>;

pub struct Report {
    passed: u32,
    failed: u32,
    failures: Vec<String>,
    gaps: Vec<String>,
}

pub fn wine_gap(holds: bool, what: impl FnOnce() -> String) -> Result<(), String> {
    if holds { Ok(()) } else { Err(format!("wine-gap: {}", what())) }
}

impl Report {
    pub fn record(&mut self, name: &str, outcome: Outcome) {
        match outcome {
            Ok(detail) => {
                self.passed += 1;
                println!("PASS {name}: {detail}");
            }
            Err(detail) if detail.starts_with("wine-gap: ") && crate::running_under_wine() => {
                println!("GAP {name}: {detail}");
                self.gaps.push(name.to_string());
            }
            Err(detail) => {
                self.failed += 1;
                println!("FAIL {name}: {detail}");
                self.failures.push(name.to_string());
            }
        }
    }

    pub fn guarded(&mut self, name: &str, limit: Duration, check: fn() -> Outcome) {
        if let Ok(filter) = std::env::var("WINDOWS_RUNTIME_CHECKS")
            && !filter.split(',').any(|part| name.contains(part))
        {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(check)
                    .unwrap_or_else(|panic| Err(format!("panicked: {}", panic_text(&panic))));
                let _ = sender.send(outcome);
            });
        let outcome = match spawned {
            Ok(_) => receiver
                .recv_timeout(limit)
                .unwrap_or_else(|_| Err(format!("did not finish within {limit:?}"))),
            Err(error) => Err(format!("thread spawn failed: {error}")),
        };
        self.record(name, outcome);
    }
}

fn panic_text(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = panic.downcast_ref::<&str>() {
        return (*text).to_string();
    }
    if let Some(text) = panic.downcast_ref::<String>() {
        return text.clone();
    }
    "unknown payload".to_string()
}

pub fn ensure(holds: bool, what: impl FnOnce() -> String) -> Result<(), String> {
    if holds { Ok(()) } else { Err(what()) }
}

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    if arguments.get(1).map(String::as_str) == Some("--child") {
        std::process::exit(child(&arguments[2..]));
    }
    let mut report = Report {
        passed: 0,
        failed: 0,
        failures: Vec::new(),
        gaps: Vec::new(),
    };
    run(&mut report);
    println!(
        "RESULT passed={} failed={} wine_gaps={}{}{}",
        report.passed,
        report.failed,
        report.gaps.len(),
        if report.gaps.is_empty() {
            String::new()
        } else {
            format!(" gaps={}", report.gaps.join(","))
        },
        if report.failures.is_empty() {
            String::new()
        } else {
            format!(" failures={}", report.failures.join(","))
        }
    );
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::exit(i32::from(report.failed != 0));
}

#[cfg(windows)]
fn run(report: &mut Report) {
    println!(
        "target {} {} {}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        if cfg!(target_env = "msvc") { "msvc" } else { "gnu" }
    );
    check_layout(report);
    win32::run(report);
    crt::run(report);
    runtime::run(report);
}

#[cfg(windows)]
pub fn running_under_wine() -> bool {
    win32::under_wine()
}

#[cfg(not(windows))]
pub const fn running_under_wine() -> bool {
    false
}

#[cfg(windows)]
fn check_layout(report: &mut Report) {
    let mut mismatches = Vec::new();
    let mut checked = 0;
    for (label, actual, expected, bytes) in layout::ROWS {
        checked += 1;
        let same = if label.starts_with("C ") {
            let bits = (*bytes * 8) as u32;
            let mask = if bits >= 128 { -1_i128 } else { (1_i128 << bits) - 1 };
            actual & mask == expected & mask
        } else {
            actual == expected
        };
        if !same {
            mismatches.push(format!("{label}: nanocode {actual} headers {expected}"));
        }
    }
    let structs = layout::ROWS.iter().filter(|row| row.0.starts_with("S ")).count();
    let constants = layout::ROWS.iter().filter(|row| row.0.starts_with("C ")).count();
    for mismatch in &mismatches {
        println!("MISMATCH {mismatch}");
    }
    report.record(
        "sys.layout_and_constants",
        if mismatches.is_empty() {
            Ok(format!("{checked} rows ({structs} structs with sizes, alignments and field offsets, {constants} constants) match the mingw-w64 headers"))
        } else {
            Err(format!("{} of {checked} rows differ", mismatches.len()))
        },
    );
}

#[cfg(not(windows))]
fn run(report: &mut Report) {
    report.record("platform", Err("this harness runs on Windows".to_string()));
}

fn child(arguments: &[String]) -> i32 {
    use std::io::{Read, Write};
    match arguments.first().map(String::as_str) {
        Some("exit") => arguments[1].parse().unwrap_or(101),
        Some("print") => {
            print!("{}", arguments[1]);
            eprint!("{}", arguments.get(2).map_or("", String::as_str));
            0
        }
        Some("echo") => {
            let mut input = Vec::new();
            if std::io::stdin().read_to_end(&mut input).is_err() {
                return 102;
            }
            let upper = input.to_ascii_uppercase();
            if std::io::stdout().write_all(&upper).is_err() {
                return 103;
            }
            eprint!("read {}", input.len());
            0
        }
        Some("lines") => {
            let mut line = String::new();
            let mut count = 0;
            loop {
                line.clear();
                match std::io::stdin().read_line(&mut line) {
                    Ok(0) => return 0,
                    Ok(_) => {
                        count += 1;
                        print!("{count}:{line}");
                        let _ = std::io::stdout().flush();
                    }
                    Err(_) => return 104,
                }
            }
        }
        Some("flood") => {
            let bytes: usize = arguments[1].parse().unwrap_or(0);
            let chunk = vec![b'x'; 65536];
            let mut left = bytes;
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            while left > 0 {
                let take = left.min(chunk.len());
                if out.write_all(&chunk[..take]).is_err() || err.write_all(&chunk[..take]).is_err() {
                    return 105;
                }
                left -= take;
            }
            0
        }
        Some("sleep") => {
            std::thread::sleep(Duration::from_millis(arguments[1].parse().unwrap_or(1000)));
            0
        }
        #[cfg(windows)]
        Some("ctrl") => win32::ctrl_child(),
        #[cfg(windows)]
        Some("ctrl-c-executor") => runtime::ctrl_c_child(),
        _ => 100,
    }
}
