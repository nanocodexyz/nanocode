use std::collections::{BTreeMap, BTreeSet};

use nanocode::{durability::EncodedPayload, turnlog::ChildScope};
use nanocode_subagents::AgentStartReport;
use serde_json::{Value, json};

use super::{
    SpawnBurstStats,
    bytes::{LEGACY_SCOPE_BYTES, Layouts, RESIDENT_SCOPE_BYTES},
    facts::Replayed,
};

const HELD: &str = "stayed held";
const QUEUED: &str = "queued";
const FENCED: &str = "fenced";
const CAPACITY: &str = "at most";
const PARENT_APPENDS_BESIDE_SPAWNS: u64 = 32;

pub(super) const INVARIANTS: [&str; 13] = [
    "started_or_queued",
    "parent_survives",
    "no_duplicate_children",
    "fold_exactly_once",
    "no_held_wait",
    "writer_cliff",
    "fenced_during_work",
    "children_report",
    "resume_bounded",
    "new_scope_no_replay",
    "restored_child_no_turns",
    "parent_appends",
    "child_bytes",
];

#[derive(Clone, Debug)]
pub(super) enum Answer {
    Started(String),
    Queued(String),
    Refused(String),
    Held(String),
    Fenced(String),
    Failed(String),
    ParentStore(String),
    Replayed(Value),
    Folded(String),
    Interrupted,
}

impl Answer {
    pub(super) fn of(started: Result<AgentStartReport, String>) -> Self {
        match started {
            Ok(report) => Self::Started(report.agent_id.to_string()),
            Err(error) if error.contains(HELD) => Self::Held(error),
            Err(error) if error.contains(QUEUED) => Self::Queued(error),
            Err(error) if error.contains(FENCED) => Self::Fenced(error),
            Err(error) if error.contains(CAPACITY) => Self::Refused(error),
            Err(error) => Self::Failed(error),
        }
    }

    pub(super) fn failed(error: String) -> Self {
        Self::of(Err(error))
    }

    pub(super) fn replayed(output: &EncodedPayload) -> Self {
        output.decode::<Value>().map_or_else(
            |error| Self::Failed(format!("a replayed spawn did not decode: {error}")),
            Self::Replayed,
        )
    }

    pub(super) fn output(&self) -> Value {
        match self {
            Self::Started(id) | Self::Folded(id) => json!({"started": id}),
            Self::Queued(answer) => json!({"queued": answer}),
            Self::Refused(answer) => json!({"refused": answer}),
            Self::Held(error)
            | Self::Fenced(error)
            | Self::Failed(error)
            | Self::ParentStore(error) => {
                json!({"failed": error})
            }
            Self::Replayed(value) => value.clone(),
            Self::Interrupted => json!({"interrupted": true}),
        }
    }

    fn started(&self) -> Option<String> {
        match self {
            Self::Started(id) | Self::Folded(id) => Some(id.clone()),
            Self::Replayed(value) => value
                .get("started")
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Appended {
    appends: u64,
    spawns: u64,
    attempts: u64,
}

#[derive(Debug, Default)]
pub(super) struct Verdict {
    answers: Vec<BTreeMap<u64, Answer>>,
    parent: Vec<String>,
    listed: Option<Result<Vec<(String, String)>, String>>,
    spawned: Option<Vec<String>>,
    appended: Option<Appended>,
    replays: Vec<Replayed>,
    finished: bool,
    stuck: bool,
    crashes: u64,
    exhausted: Option<u64>,
    restored: Option<String>,
    parent_layout: bool,
    bytes: Option<Measured>,
}

#[derive(Debug)]
struct Measured {
    children: u64,
    layouts: Result<(Layouts, Layouts), String>,
}

impl Verdict {
    pub(super) fn parent_failed(&mut self, error: String) {
        self.parent.push(error);
    }

    pub(super) const fn finished(&mut self) {
        self.finished = true;
    }

    pub(super) fn answered(&mut self, answers: BTreeMap<u64, Answer>) {
        self.finished = true;
        self.answers.push(answers);
    }

    pub(super) fn listed(&mut self, listed: Result<Vec<(String, String)>, String>) {
        self.listed = Some(listed);
    }

    pub(super) const fn appended(&mut self, appends: u64, spawns: u64, attempts: u64) {
        self.appended = Some(Appended {
            appends,
            spawns,
            attempts,
        });
    }

    pub(super) fn spawned(&mut self, spawned: Option<Vec<String>>) {
        self.spawned = spawned;
    }

    pub(super) fn replayed(&mut self, replays: Vec<Replayed>) {
        self.replays.extend(replays);
    }

    pub(super) const fn stuck(&mut self) {
        self.stuck = true;
    }

    pub(super) const fn crashed(&mut self) {
        self.crashes += 1;
    }

    pub(super) fn layout(&mut self, layout: ChildScope) {
        self.parent_layout = layout == ChildScope::Parent;
    }

    pub(super) fn restored(&mut self, wrong: Option<String>) {
        self.restored = wrong;
    }

    pub(super) fn bytes(&mut self, children: u64, layouts: Result<(Layouts, Layouts), String>) {
        assert!(children > 0, "a byte measurement spawns children");
        assert!(
            self.bytes.is_none(),
            "a seed measures its children's bytes once"
        );
        self.bytes = Some(Measured { children, layouts });
    }

    pub(super) const fn resumes_exhausted(&mut self, attempts: u64) {
        self.exhausted = Some(attempts);
    }

    pub(super) fn stats(&self, spawns: u64) -> SpawnBurstStats {
        let mut stats = SpawnBurstStats {
            seeds: 1,
            spawns,
            crashes: self.crashes,
            ..SpawnBurstStats::default()
        };
        if let Some(Measured {
            children,
            layouts: Ok((legacy, resident)),
        }) = &self.bytes
        {
            stats.measured_children += children;
            stats.legacy_own_bytes += legacy.own.bytes;
            stats.legacy_parent_bytes += legacy.parent.bytes;
            stats.legacy_own_rows += legacy.own.rows;
            stats.legacy_parent_rows += legacy.parent.rows;
            stats.resident_own_rows += resident.own.rows;
            stats.resident_parent_rows += resident.parent.rows;
            stats.resident_own_bytes += resident.own.bytes;
            stats.resident_parent_bytes += resident.parent.bytes;
        }
        for replay in &self.replays {
            stats.replays += 1;
            stats.replay_scans += replay.scans;
            stats.replay_held_ms_max = stats.replay_held_ms_max.max(replay.held_ms);
        }
        for answer in self.answers.iter().flat_map(BTreeMap::values) {
            match answer {
                Answer::Started(_) => stats.started += 1,
                Answer::Queued(_) => stats.queued += 1,
                Answer::Refused(_) => stats.refused += 1,
                Answer::Held(_) => stats.held += 1,
                Answer::Replayed(_) => stats.replayed += 1,
                Answer::Folded(_) => stats.folded += 1,
                Answer::Interrupted => stats.interrupted += 1,
                _ => stats.failed += 1,
            }
        }
        stats
    }

    fn last(&self) -> Option<&BTreeMap<u64, Answer>> {
        self.answers.last()
    }

    fn started_or_queued(&self) -> Option<String> {
        let answers = self.last()?;
        answers.iter().find_map(|(index, answer)| match answer {
            Answer::Started(_)
            | Answer::Folded(_)
            | Answer::Queued(_)
            | Answer::Refused(_)
            | Answer::Replayed(_)
            | Answer::Interrupted => None,
            other => Some(format!(
                "spawn {index} of {} answered {other:?}, neither started nor a typed queue answer",
                answers.len()
            )),
        })
    }

    fn parent_survives(&self) -> Option<String> {
        self.parent
            .first()
            .map(|error| format!("the parent turn failed: {error}"))
    }

    fn no_held_wait(&self) -> Option<String> {
        self.answers
            .iter()
            .flat_map(BTreeMap::iter)
            .find_map(|(index, answer)| match answer {
                Answer::Held(error) => Some(format!(
                    "spawn {index} waited for the writer and was refused: {error}"
                )),
                _ => None,
            })
    }

    fn writer_cliff(&self) -> Option<String> {
        let answers = self.last()?;
        let held = answers
            .values()
            .filter(|answer| matches!(answer, Answer::Held(_)))
            .count();
        (held > 0).then(|| {
            format!(
                "{held} of {} concurrent spawns timed out behind the parent's one writer",
                answers.len()
            )
        })
    }

    fn fenced_during_work(&self) -> Option<String> {
        self.answers
            .iter()
            .flat_map(BTreeMap::iter)
            .find_map(|(index, answer)| match answer {
                Answer::Fenced(error) => Some(format!(
                    "spawn {index} answered a fence during concurrent work: {error}"
                )),
                _ => None,
            })
            .or_else(|| {
                self.parent
                    .iter()
                    .find(|error| error.contains(FENCED))
                    .map(|error| format!("the parent was fenced during concurrent work: {error}"))
            })
    }

    fn no_duplicate_children(&self) -> Option<String> {
        let Some(Ok(listed)) = &self.listed else {
            return None;
        };
        let mut tasks = BTreeSet::new();
        for (_, task) in listed {
            if !tasks.insert(task.clone()) {
                return Some(format!("two children run task {task:?}"));
            }
        }
        None
    }

    fn fold_exactly_once(&self) -> Option<String> {
        if self.parent_layout {
            return None;
        }
        let answers = self.last()?;
        let spawned = self.spawned.as_ref()?;
        let mut folded = BTreeSet::new();
        for answer in answers.values() {
            if let Some(child) = answer.started()
                && !folded.insert(child.clone())
            {
                return Some(format!("the parent folded child {child} twice"));
            }
        }
        let held: BTreeSet<String> = spawned.iter().cloned().collect();
        if let Some(missing) = folded.difference(&held).next() {
            return Some(format!(
                "the parent folded child {missing}, which it holds no spawn record of"
            ));
        }
        held.difference(&folded).next().map(|unfolded| {
            format!("the parent holds child {unfolded} but folded no result for it")
        })
    }

    fn parent_appends(&self) -> Option<String> {
        if self.parent_layout {
            return None;
        }
        let appended = self.appended?;
        let bound = appended.spawns + appended.attempts * PARENT_APPENDS_BESIDE_SPAWNS;
        (appended.appends > bound).then(|| {
            format!(
                "the parent made {} appends for {} spawns over {} attempts, past one per spawn and {PARENT_APPENDS_BESIDE_SPAWNS} per attempt ({bound})",
                appended.appends, appended.spawns, appended.attempts
            )
        })
    }

    fn new_scope_no_replay(&self) -> Option<String> {
        if self.parent_layout {
            return None;
        }
        self.replays
            .iter()
            .filter(|replay| replay.opening && replay.scans > 0)
            .max_by_key(|replay| replay.held_ms)
            .map(|replay| {
                format!(
                    "opening a new child's facts replayed {} segment scans of the parent's log over {} ms and folded {} facts",
                    replay.scans, replay.held_ms, replay.facts
                )
            })
    }

    fn child_bytes(&self) -> Option<String> {
        let measured = self.bytes.as_ref()?;
        let (legacy, resident) = match &measured.layouts {
            Ok(layouts) => layouts,
            Err(error) => return Some(format!("the byte measurement failed: {error}")),
        };
        let paths = [
            ("legacy", legacy, LEGACY_SCOPE_BYTES),
            ("resident", resident, RESIDENT_SCOPE_BYTES),
        ];
        paths.into_iter().find_map(|(path, layouts, scope_bytes)| {
            let bound = layouts.parent.bytes + measured.children * scope_bytes;
            (layouts.own.bytes > bound).then(|| {
                format!(
                    "on the {path} path {} children in their own scopes wrote {} bytes in {} rows, past the {} bytes and {} rows on the parent's log plus {scope_bytes} per scope ({bound})",
                    measured.children,
                    layouts.own.bytes,
                    layouts.own.rows,
                    layouts.parent.bytes,
                    layouts.parent.rows
                )
            })
        })
    }

    fn children_report(&self) -> Option<String> {
        if self.stuck {
            return Some("every task waits and no timer is set: a child never started or reported and its parent waits for it".to_owned());
        }
        match &self.listed {
            Some(Err(error)) => Some(format!("the parent could not list its children: {error}")),
            _ => None,
        }
    }

    fn resume_bounded(&self) -> Option<String> {
        let attempts = self.exhausted?;
        Some(format!(
            "the parent turn crashed its host {attempts} times, each resume running the crashing work again, with no report"
        ))
    }

    pub(super) fn wrong(&self, only: Option<&str>) -> Option<String> {
        INVARIANTS
            .iter()
            .filter(|name| only.is_none_or(|only| only == **name))
            .find_map(|name| {
                let found = match *name {
                    "started_or_queued" => self.started_or_queued(),
                    "parent_survives" => self.parent_survives(),
                    "no_duplicate_children" => self.no_duplicate_children(),
                    "fold_exactly_once" => self.fold_exactly_once(),
                    "no_held_wait" => self.no_held_wait(),
                    "writer_cliff" => self.writer_cliff(),
                    "fenced_during_work" => self.fenced_during_work(),
                    "children_report" => self.children_report(),
                    "resume_bounded" => self.resume_bounded(),
                    "new_scope_no_replay" => self.new_scope_no_replay(),
                    "restored_child_no_turns" => self.restored.clone(),
                    "parent_appends" => self.parent_appends(),
                    "child_bytes" => self.child_bytes(),
                    _ => None,
                };
                found.map(|message| format!("{name}: {message}"))
            })
    }
}
