use nanocode_code_mode::code_mode_spec::EXEC_PRAGMA_PREFIX;
use nanocode_sync::Atom;
use serde_json::Value;
use std::time::Duration;

const EARLY_REMINDER: &str = "Keep working until then. First put every output the task asks for in its final location, then use the time that remains to improve and test it. Test it on an input you made that differs from the given example, because the task's checks use inputs you have not seen.";
const EARLY_TARGET_REMINDER: &str = "First put every output the task asks for in its final location. If it already meets every target the task states, with a margin across repeated runs of the task's own check, finish now instead of improving it further; otherwise use the time that remains to reach them. Test it on an input you made that differs from the given example, because the task's checks use inputs you have not seen.";
const LAST_REMINDER: &str = "Finish now. Make sure every output the task asks for is in its final location. Delete only the scratch files and test output you made where the task's program or its checks write, read or wait for files, such as a file in /tmp. Keep what the task's program built or set up: caches and compiled artifacts (such as a compiled model or a build directory), installed packages, and the state the task asked for (repositories and their branches, served files, databases, services). Never reset or empty that state to make it clean. Stop background processes you started, unless the task asked to keep them running. Do not start a command or a wait that cannot finish in the time that remains.";
const FINISH_OPEN: &str =
    "<system-reminder>\nThis session is non-interactive and nobody will reply.";
const FINISH_CHECK: &str = " Before you finish, check the work:\n";
const FINISH_HEAD: &str = "- Every output the task names exists at that exact path, with a relative path resolved against the working directory.\n- The task's own check, or a direct test of the result, passes when you run it again now.\n- Each case the task statement names holds when you run it now on an input made for that case: empty input, leading or trailing whitespace, letter case, and each boundary or invalid form the statement lists. Compare exact output bytes, including newlines.\n- Each limit or target number the task states (a gap, range, tolerance or fitted value) was computed from the result by the method the task implies, printed and compared.\n- A library call or tool the task names without options runs with its defaults and no added option.\n";
const AMBIGUITY_CHECK: &str = "- Each place the task statement can be read two ways is listed and settled by its literal wording and examples, quoting the deciding sentence. That wording beats a convention or cleanup it does not ask for: a value it says to match or copy stays verbatim. If nothing decides it, the result holds under both readings where it can, else follows the reading the examples support, tested by a run. A rule seen in a shown input follows the general wording, not that input alone.\n";
const SCORE_CHECK: &str = "- If the task states a speed, size or score goal, the result was compared with at least one structurally different approach under repeated measurements, and the best one is kept. Beating the given baseline is not by itself the goal.\n";
const TARGET_CHECK: &str = "- If the task states a target you can check yourself (a score, win rate, accuracy, speed or size to reach on given inputs or against given opponents), the result meets it with a margin across repeated runs of that check, and meeting it is done: do not keep improving it. If the goal is measured against something you cannot see, such as a hidden reference time, the result was compared with at least one structurally different approach under repeated measurements, and the best one is kept.\n";
const FINISH_TAIL: &str = "- The task's checks use inputs you have not seen (other files, data, sizes or seeds) and run the program as a script does: stdout piped, not a terminal, and signals such as SIGINT. The result was run, with stdout piped when output matters, on an input you made that differs from the given example and, if some inputs must stay as they are (clean, valid or already correct), on one of those, and each output is right. Nothing is tuned to the given example alone.\n- The scratch files and test output you made where the task's program or its checks write, read or wait for files, such as a file in /tmp, are deleted, and nothing else is.\n- What the task's program built or set up is kept: caches and compiled artifacts (such as a compiled model or a build directory), installed packages, and the state the task asked for (repositories and their branches, served files, databases, services). None of that state is reset or emptied to make it clean.\n- Background processes you started are stopped, unless the task asked to keep them running.\n- Any choice left open is made, and its result is written.\n";
const TOOL_TRUTH_CHECK: &str = "- Each value the task says a named tool, program or library decides (its ground truth, such as a measurement with stated flags) was produced by running that tool with exactly those options on the final result and printed, not by your own reimplementation of it. If the tool is missing, install it first (the system package manager after refreshing its index, pip, or a build from source) and try that before writing a substitute: a missing curl, wget or python does not show that the network is down. If it still cannot run, keep every such value well inside its limit, by more than a substitute's likely error, and say in your final message which values were not checked with the named tool.\n";
const HELD_OUT_CHECK: &str = "- If the result is a program that the task's checks run on inputs you have not seen, and the task shows only one or a few example inputs: a copy of an example that is only shifted, padded, trimmed, re-encoded or noised is not a different input, because a rule tuned to the example still passes on it. List each constant the program took from looking at the example (a threshold, size, count, region, position, window, index or duration) and, for each, an input the task statement allows on which it would break: longer or shorter, a different count, speed, scale or position of what the program looks for, or more clutter. Make at least one such input whose right output you know from how you made it, by recombining, cropping, resampling or stretching parts of the example or by generating one, and run the program on it. Replace each constant that breaks with a value the program measures from its own input, and check that the example still comes out right. Use the time that remains for this rather than finishing early.\n";
const PERSIST_CHECK: &str = "- Every requirement and target the task states is met, and you checked it by running the result. One that is not met, or that you would report as unmet or unverified, is not done: while time remains, keep working on it with a different approach instead of reporting the gap.\n";
pub const BRIEF_FINAL: &str = "<system-reminder>\nKeep your final message to at most three lines: the result and where it is.\n</system-reminder>";
pub const STOP_AT_TARGET: &str = "<system-reminder>\nWhen the task states a target you can check yourself, such as a score, win rate, accuracy, speed or size to reach on given inputs or against given opponents, the task is done once the result meets it with a margin across repeated runs of the task's own check: put it in its final location, run the check once more and finish, instead of improving it further. When you compare many variants, generate and run them from one script or loop, not one written-out call per variant.\n</system-reminder>";
pub const LEAN_CELLS: &str = "<system-reminder>\nKeep each code cell to the commands the step needs, without comments or progress prints. Print only the lines you need from a file or a command, with head, tail, grep or a line range, not whole files or logs. Do not read a file again unless it changed since you read it.\n</system-reminder>";
const TIME_LIMIT: &str = "Time limit: ";
const TIME_UP: &str = " When the time is up the task stops, and only what is on disk then counts.";
const REMINDER_PERCENTS: [u128; 3] = [50, 75, 90];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deadline {
    at_unix_ms: u64,
    limit: Duration,
}

pub enum Cap<T> {
    Capped { input: T, notice: String },
    Expired(String),
    Unchanged,
}

pub struct Reminders {
    announced: Atom<usize>,
    deadline: Deadline,
    early: &'static str,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FinishChecks {
    pub ambiguity: bool,
    pub held_out: bool,
    pub persist: bool,
    pub stop_at_target: bool,
    pub tool_truth: bool,
}

impl Deadline {
    #[must_use]
    pub const fn new(at_unix_ms: u64, limit: Duration) -> Self {
        Self { at_unix_ms, limit }
    }

    fn remaining(self) -> Duration {
        Duration::from_millis(
            self.at_unix_ms
                .saturating_sub(nanocode_platform::unix_millis()),
        )
    }

    fn allowed(self) -> u64 {
        u64::try_from(self.remaining().as_millis()).unwrap_or(u64::MAX)
    }

    fn passed(self, remaining: Duration) -> usize {
        let elapsed = self.limit.saturating_sub(remaining).as_millis();
        let limit = self.limit.as_millis();
        REMINDER_PERCENTS
            .iter()
            .filter(|percent| elapsed * 100 >= limit * **percent)
            .count()
    }

    fn remain(self, remaining: Duration) -> String {
        format!(
            "{} of this task's {} remain.",
            spoken(remaining),
            spoken(self.limit)
        )
    }

    #[must_use]
    pub fn remain_now(self) -> String {
        self.remain(self.remaining())
    }

    #[must_use]
    pub fn cap(self, field: &str, input: &Value) -> Cap<Value> {
        let Some(mut object) = input.as_object().cloned() else {
            return Cap::Unchanged;
        };
        let Some(requested) = object.get(field).and_then(Value::as_f64) else {
            return Cap::Unchanged;
        };
        let allowed = self.allowed();
        if requested <= allowed as f64 {
            return Cap::Unchanged;
        }
        object.insert(field.into(), allowed.into());
        self.capped(Value::Object(object), (field, requested, allowed))
    }

    #[must_use]
    pub fn cap_share(self, percent: u8, field: &str, input: &Value) -> Cap<Value> {
        let Some(mut object) = input.as_object().cloned() else {
            return Cap::Unchanged;
        };
        let Some(requested) = object.get(field).and_then(Value::as_f64) else {
            return Cap::Unchanged;
        };
        let allowed = self.share(percent);
        if allowed == 0 || requested <= allowed as f64 {
            return Cap::Unchanged;
        }
        object.insert(field.into(), allowed.into());
        Cap::Capped {
            input: Value::Object(object),
            notice: self.shared(percent, (field, requested, allowed)),
        }
    }

    #[must_use]
    pub fn cap_source_share(self, percent: u8, code: &str) -> Cap<String> {
        let allowed = self.share(percent);
        if allowed == 0 {
            return Cap::Unchanged;
        }
        pragma_cap(code, allowed).map_or(Cap::Unchanged, |(code, requested)| Cap::Capped {
            input: code,
            notice: self.shared(percent, ("yield_time_ms", requested, allowed)),
        })
    }

    fn share(self, percent: u8) -> u64 {
        self.allowed().saturating_mul(u64::from(percent.min(100))) / 100
    }

    fn shared(self, percent: u8, request: (&str, f64, u64)) -> String {
        let (field, requested, allowed) = request;
        format!(
            "{field} {requested} ms is more than {percent}% of the {} left before this task's {} time limit, so this call ran with {field} {allowed} ms. Work it started keeps running.",
            spoken(self.remaining()),
            spoken(self.limit)
        )
    }

    #[must_use]
    pub fn cap_source(self, code: &str) -> Cap<String> {
        let allowed = self.allowed();
        pragma_cap(code, allowed).map_or(Cap::Unchanged, |(code, requested)| {
            self.capped(code, ("yield_time_ms", requested, allowed))
        })
    }

    fn capped<T>(self, input: T, request: (&str, f64, u64)) -> Cap<T> {
        let (field, requested, allowed) = request;
        if allowed == 0 {
            return Cap::Expired(format!(
                "The {} time limit for this task is reached, so this call did not run.",
                spoken(self.limit)
            ));
        }
        Cap::Capped {
            input,
            notice: format!(
                "{field} {requested} ms is longer than the {} left before this task's {} time limit, so this call ran with {field} {allowed} ms.",
                spoken(Duration::from_millis(allowed)),
                spoken(self.limit)
            ),
        }
    }
}

fn pragma_cap(code: &str, allowed: u64) -> Option<(String, f64)> {
    let (first, rest) = code.split_once('\n').unwrap_or((code, ""));
    let pragma = first.trim_start().strip_prefix(EXEC_PRAGMA_PREFIX)?;
    let Value::Object(mut pragma) = serde_json::from_str::<Value>(pragma).ok()? else {
        return None;
    };
    let requested = pragma.get("yield_time_ms")?.as_f64()?;
    if requested <= allowed as f64 {
        return None;
    }
    pragma.insert("yield_time_ms".into(), allowed.into());
    Some((
        format!("{EXEC_PRAGMA_PREFIX} {}\n{rest}", Value::Object(pragma)),
        requested,
    ))
}

impl Reminders {
    #[must_use]
    pub fn new(deadline: Deadline) -> Self {
        Self {
            announced: Atom::new(0),
            deadline,
            early: EARLY_REMINDER,
        }
    }

    #[must_use]
    pub fn stopping_at_target(deadline: Deadline) -> Self {
        Self {
            early: EARLY_TARGET_REMINDER,
            ..Self::new(deadline)
        }
    }

    #[must_use]
    pub const fn deadline(&self) -> Deadline {
        self.deadline
    }

    #[must_use]
    pub fn first_passed(&self) -> bool {
        self.deadline.passed(self.deadline.remaining()) > 0
    }

    #[must_use]
    pub fn last_passed(&self) -> bool {
        self.deadline.passed(self.deadline.remaining()) == REMINDER_PERCENTS.len()
    }

    #[must_use]
    pub fn notice(&self) -> Option<String> {
        let remaining = self.deadline.remaining();
        let passed = self.deadline.passed(remaining);
        let announced = self.announced.update(|announced| {
            let was = *announced;
            *announced = was.max(passed);
            was
        });
        if passed == 0 {
            return None;
        }
        let remain = self.deadline.remain(remaining);
        if passed <= announced {
            return Some(format!("{TIME_LIMIT}{remain}"));
        }
        let steps = if passed == REMINDER_PERCENTS.len() {
            LAST_REMINDER
        } else {
            self.early
        };
        Some(format!("{TIME_LIMIT}{remain}{TIME_UP} {steps}"))
    }

    #[must_use]
    pub fn remain(&self) -> String {
        self.deadline.remain(self.deadline.remaining())
    }

    #[must_use]
    pub fn left(&self) -> Duration {
        self.deadline.remaining()
    }
}

#[must_use]
pub fn finish_notice(deadline: Option<&Reminders>, brief: bool) -> String {
    finish_notice_with(deadline, brief, FinishChecks::default())
}

#[must_use]
pub fn persisting_finish_notice(deadline: Option<&Reminders>, brief: bool) -> String {
    finish_notice_with(
        deadline,
        brief,
        FinishChecks {
            ambiguity: false,
            held_out: false,
            persist: true,
            stop_at_target: false,
            tool_truth: false,
        },
    )
}

#[must_use]
pub fn finish_notice_with(
    deadline: Option<&Reminders>,
    brief: bool,
    checks: FinishChecks,
) -> String {
    let time = deadline.map_or_else(String::new, |reminders| format!(" {}", reminders.remain()));
    let reply = if brief {
        "reply with one line that says the task is done"
    } else {
        "reply with your final message. In it, name the checks you ran and the stated cases you did not test. A test you wrote alone does not show that the result works"
    };
    let checklist = checks.checklist();
    format!(
        "{FINISH_OPEN}{time}{FINISH_CHECK}{checklist}Fix what falls short. When everything holds, {reply}.\n</system-reminder>"
    )
}

#[must_use]
pub fn folded_finish_notice() -> String {
    folded_finish_notice_with(FinishChecks::default())
}

#[must_use]
pub fn persisting_folded_finish_notice() -> String {
    folded_finish_notice_with(FinishChecks {
        ambiguity: false,
        held_out: false,
        persist: true,
        stop_at_target: false,
        tool_truth: false,
    })
}

#[must_use]
pub fn folded_finish_notice_with(checks: FinishChecks) -> String {
    let checklist = checks.checklist();
    format!(
        "{FINISH_OPEN} Before your final message, check the work:\n{checklist}Fix what falls short before you reply with your final message. In it, name the checks you ran and the stated cases you did not test. A test you wrote alone does not show that the result works.\n</system-reminder>"
    )
}

const INCOMPLETE_ADMISSIONS: &[&str] = &[
    "couldn't",
    "could not",
    "wasn't able",
    "was not able",
    "weren't able",
    "unable to",
    "not able to",
    "does not meet",
    "doesn't meet",
    "does **not**",
    "do not meet",
    "don't meet",
    "not yet complete",
    "not complete",
    "is incomplete",
    "still incomplete",
    "remains incomplete",
    "remains the original",
    "remains the example",
    "didn't complete",
    "did not complete",
    "didn't finish",
    "did not finish",
    "haven't completed",
    "have not completed",
    "haven't finished",
    "have not finished",
    "haven't verified",
    "have not verified",
    "short of",
    "below the required",
    "does not satisfy",
    "doesn't satisfy",
    "won't satisfy",
    "will not satisfy",
    "fails to meet",
    "misses the",
    "not met",
    "unconfirmed",
    "i haven't",
    "i have not",
    "i did not",
    "i didn't",
];

#[must_use]
pub fn admits_incomplete(message: &str) -> bool {
    let message = message
        .replace(['\u{2018}', '\u{2019}'], "'")
        .to_lowercase();
    INCOMPLETE_ADMISSIONS
        .iter()
        .any(|admission| message.contains(admission))
}

pub const PERSISTENCE_NUDGE: &str = concat!(
    "<persistence>\n",
    "Your last message says part of the task is not done or not verified. This ",
    "session is non-interactive and nobody will reply, so continue now: work on what ",
    "remains, take a different approach where the last one fell short, and verify the ",
    "result. If every requirement is in fact met and verified, reply with your final ",
    "message again.\n",
    "</persistence>"
);

pub const FRESH_ATTEMPT_MARK: &str = "[fresh attempt]";

#[must_use]
pub fn fresh_finish_notice_with(
    deadline: Option<&Reminders>,
    brief: bool,
    checks: FinishChecks,
) -> String {
    let time = deadline.map_or_else(String::new, |reminders| format!(" {}", reminders.remain()));
    let reply = if brief {
        "reply with one line that says the task is done"
    } else {
        "reply with your final message"
    };
    let checklist = checks.checklist();
    format!(
        "{FINISH_OPEN}{time}{FINISH_CHECK}{checklist}Fix a gap that does not change the result, such as a scratch file or a background process. Then run the result on an input the task did not show and on an input that must come out unchanged. If one of these runs, the task's own check or a test of the result fails, do not fix it here: reply with one message that starts with {FRESH_ATTEMPT_MARK} and names each check that failed (its command, its input when the task did not show it, and its error), the approach you used, and each file you changed. A fresh attempt then starts from the files as they are now, with the task and that message. When everything holds, {reply}.\n</system-reminder>"
    )
}

#[must_use]
pub fn fresh_attempt_brief(report: &str) -> String {
    format!(
        "<system-reminder>\nAn earlier attempt at this task ran in this environment, and a check of its result failed, possibly on an input the task did not show. Its files and installed packages are still in place. Its report:\n{report}\nStart from the current files. Do not trust the report until you check it. Find the cause of each failed check, complete the task, and run the result again on the inputs the report names.\n</system-reminder>"
    )
}

impl FinishChecks {
    fn checklist(self) -> String {
        let score = if self.stop_at_target {
            TARGET_CHECK
        } else {
            SCORE_CHECK
        };
        let ambiguity = if self.ambiguity { AMBIGUITY_CHECK } else { "" };
        let persist = if self.persist { PERSIST_CHECK } else { "" };
        let tool_truth = if self.tool_truth { TOOL_TRUTH_CHECK } else { "" };
        let held_out = if self.held_out { HELD_OUT_CHECK } else { "" };
        format!("{FINISH_HEAD}{tool_truth}{ambiguity}{score}{FINISH_TAIL}{held_out}{persist}")
    }
}

#[must_use]
pub fn is_finish_notice(text: &str) -> bool {
    text.starts_with(FINISH_OPEN) && text.contains(FINISH_CHECK)
}

#[must_use]
pub fn is_time_left(text: &str) -> bool {
    text.starts_with(TIME_LIMIT) && !text.contains(TIME_UP)
}

#[cfg(test)]
mod finish_check_tests {
    use super::*;

    const ALL: [FinishChecks; 2] = [
        FinishChecks {
            ambiguity: false,
            held_out: false,
            persist: false,
            stop_at_target: false,
            tool_truth: false,
        },
        FinishChecks {
            ambiguity: true,
            held_out: false,
            persist: true,
            stop_at_target: true,
            tool_truth: false,
        },
    ];

    #[test]
    fn held_out_off_leaves_every_notice_unchanged() {
        for checks in ALL {
            let notice = finish_notice_with(None, false, checks);
            assert!(!notice.contains(HELD_OUT_CHECK));
            assert!(!folded_finish_notice_with(checks).contains(HELD_OUT_CHECK));
            assert!(!fresh_finish_notice_with(None, true, checks).contains(HELD_OUT_CHECK));
        }
        assert_eq!(finish_notice(None, false), finish_notice_with(None, false, ALL[0]));
        assert!(!persisting_finish_notice(None, true).contains(HELD_OUT_CHECK));
        assert!(!persisting_folded_finish_notice().contains(HELD_OUT_CHECK));
    }

    #[test]
    fn held_out_adds_its_bullet_once_after_the_tail_and_before_persist() {
        for base in ALL {
            let checks = FinishChecks {
                held_out: true,
                ..base
            };
            for notice in [
                finish_notice_with(None, false, checks),
                folded_finish_notice_with(checks),
                fresh_finish_notice_with(None, false, checks),
            ] {
                assert!(is_finish_notice(&notice) || notice.contains("Before your final message"));
                assert_eq!(notice.matches(HELD_OUT_CHECK).count(), 1);
                let at = notice.find(HELD_OUT_CHECK).unwrap();
                assert!(notice.find(FINISH_TAIL).unwrap() < at);
                if checks.persist {
                    assert!(at < notice.find(PERSIST_CHECK).unwrap());
                }
            }
        }
    }

    #[test]
    fn held_out_bullet_names_no_task_or_domain() {
        let lower = HELD_OUT_CHECK.to_lowercase();
        for word in ["video", "frame", "jump", "hurdle", "ocr", "ffmpeg", "terminal-bench"] {
            assert!(!lower.contains(word), "{word}");
        }
    }
}

fn spoken(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match (seconds / 60, seconds % 60) {
        (0, seconds) => format!("{seconds} s"),
        (minutes, 0) => format!("{minutes} min"),
        (minutes, seconds) => format!("{minutes} min {seconds} s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_truth_off_leaves_the_notices_unchanged() {
        let plain = FinishChecks::default();
        assert!(!plain.tool_truth);
        assert!(!finish_notice(None, false).contains(TOOL_TRUTH_CHECK));
        assert!(!finish_notice(None, true).contains(TOOL_TRUTH_CHECK));
        assert!(!folded_finish_notice().contains(TOOL_TRUTH_CHECK));
        assert!(!persisting_finish_notice(None, false).contains(TOOL_TRUTH_CHECK));
        assert!(!persisting_folded_finish_notice().contains(TOOL_TRUTH_CHECK));
        assert_eq!(plain.checklist(), format!("{FINISH_HEAD}{SCORE_CHECK}{FINISH_TAIL}"));
    }

    #[test]
    fn tool_truth_adds_one_item_after_the_head() {
        let checks = FinishChecks {
            tool_truth: true,
            ..FinishChecks::default()
        };
        let list = checks.checklist();
        assert_eq!(list.matches(TOOL_TRUTH_CHECK).count(), 1);
        assert!(list.starts_with(&format!("{FINISH_HEAD}{TOOL_TRUTH_CHECK}")));
        let notice = finish_notice_with(None, false, checks);
        assert!(is_finish_notice(&notice));
        assert!(notice.contains(TOOL_TRUTH_CHECK));
        assert!(folded_finish_notice_with(checks).contains(TOOL_TRUTH_CHECK));
    }

    #[test]
    fn tool_truth_composes_with_the_other_checks() {
        let checks = FinishChecks {
            ambiguity: true,
            held_out: false,
            persist: true,
            stop_at_target: true,
            tool_truth: true,
        };
        assert_eq!(
            checks.checklist(),
            format!(
                "{FINISH_HEAD}{TOOL_TRUTH_CHECK}{AMBIGUITY_CHECK}{TARGET_CHECK}{FINISH_TAIL}{PERSIST_CHECK}"
            )
        );
    }

    #[test]
    fn tool_truth_text_names_no_task() {
        for word in ["oligotm", "primer", "dna", "protein", "terminal-bench"] {
            assert!(!TOOL_TRUTH_CHECK.to_lowercase().contains(&word.to_lowercase()), "{word}");
        }
    }
}
