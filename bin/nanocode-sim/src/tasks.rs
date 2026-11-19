use std::{collections::BTreeMap, ops::Range, process::ExitCode, sync::Arc};

use nanocode_executor::{
    Builder, JoinError, JoinHandle, JoinSet, coop::consume_budget, sim, yield_now,
};
use nanocode_sync::{Slot, oneshot};

use crate::{
    drive::{self, Busy, Sleepers, Threads},
    rng::{FNV_OFFSET, Rng, fnv},
};

const TASKS_MAX: u64 = 16;
const HOPS_MAX: u64 = 6;
const PANIC_PPM: u64 = 100_000;
const AWAIT_PPM: u64 = 300_000;
const BUDGET_PPM: u64 = 200_000;
const SET_PPM: u64 = 400_000;
const ABORT_PPM: u64 = 300_000;
const DETACH_PPM: u64 = 150_000;
const YIELDS_MAX: u64 = 4;
const VALUE_STRIDE: u64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TasksMutation {
    None,
    SkipAbort,
    LeakCancelled,
    AbortsWrongTask,
    JoinsTwice,
    PanicAsCancelled,
    SignalDropped,
    SignalEarly,
    SkipAwait,
    UncountedPolls,
    UncheckedDrive,
    StaleWaiters,
}

impl TasksMutation {
    const ALL: [Self; 12] = [
        Self::None,
        Self::SkipAbort,
        Self::LeakCancelled,
        Self::AbortsWrongTask,
        Self::JoinsTwice,
        Self::PanicAsCancelled,
        Self::SignalDropped,
        Self::SignalEarly,
        Self::SkipAwait,
        Self::UncountedPolls,
        Self::UncheckedDrive,
        Self::StaleWaiters,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SkipAbort => "skip_abort",
            Self::LeakCancelled => "leak_cancelled",
            Self::AbortsWrongTask => "aborts_wrong_task",
            Self::JoinsTwice => "joins_twice",
            Self::PanicAsCancelled => "panic_as_cancelled",
            Self::SignalDropped => "signal_dropped",
            Self::SignalEarly => "signal_early",
            Self::SkipAwait => "skip_await",
            Self::UncountedPolls => "uncounted_polls",
            Self::UncheckedDrive => "unchecked_drive",
            Self::StaleWaiters => "stale_waiters",
        }
    }

    const fn executor(self) -> sim::Mutation {
        match self {
            Self::UncountedPolls => sim::Mutation::UncountedPolls,
            Self::UncheckedDrive => sim::Mutation::UncheckedDrive,
            Self::StaleWaiters => sim::Mutation::StaleWaiters,
            _ => sim::Mutation::None,
        }
    }

    pub fn names() -> String {
        Self::ALL
            .iter()
            .skip(1)
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TasksStats {
    pub seeds: u64,
    pub tasks: u64,
    pub finished: u64,
    pub cancelled: u64,
    pub unwound: u64,
    pub detached: u64,
    pub busy: u64,
    pub threaded: u64,
    pub messages: u64,
    pub sleepers: u64,
    pub naps: u64,
}

impl TasksStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.tasks += other.tasks;
        self.finished += other.finished;
        self.cancelled += other.cancelled;
        self.unwound += other.unwound;
        self.detached += other.detached;
        self.busy += other.busy;
        self.threaded += other.threaded;
        self.messages += other.messages;
        self.sleepers += other.sleepers;
        self.naps += other.naps;
    }
}

#[derive(Clone, Copy, Debug)]
enum Hop {
    Yield,
    Budget,
    Await(usize),
}

struct TaskPlan {
    hops: Vec<Hop>,
    panics: bool,
    in_set: bool,
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Yield(u64),
    Abort(usize),
    Detach(usize),
}

struct Script {
    tasks: Vec<TaskPlan>,
    actions: Vec<Action>,
    busy: Option<Busy>,
    threads: Option<Threads>,
    sleepers: Option<Sleepers>,
}

fn wide(value: usize) -> u64 {
    u64::try_from(value).expect("a count fits in 64 bits")
}

impl Script {
    fn of(seed: u64) -> Self {
        let mut rng = Rng::derive(seed, "tasks");
        let count = rng.range(1, TASKS_MAX);
        let tasks: Vec<TaskPlan> = (0..count)
            .map(|index| {
                let hops = (0..rng.below(HOPS_MAX + 1))
                    .map(|_| {
                        if index > 0 && rng.chance(AWAIT_PPM) {
                            Hop::Await(rng.pick(usize::try_from(index).expect("a task index")))
                        } else if rng.chance(BUDGET_PPM) {
                            Hop::Budget
                        } else {
                            Hop::Yield
                        }
                    })
                    .collect();
                TaskPlan {
                    hops,
                    panics: rng.chance(PANIC_PPM),
                    in_set: rng.chance(SET_PPM),
                }
            })
            .collect();
        let mut actions = Vec::new();
        for index in 0..tasks.len() {
            actions.push(Action::Yield(rng.below(YIELDS_MAX + 1)));
            if rng.chance(ABORT_PPM) {
                actions.push(Action::Abort(rng.pick(tasks.len())));
            } else if rng.chance(DETACH_PPM) && !tasks[index].in_set {
                actions.push(Action::Detach(index));
            }
        }
        let mut drives = Rng::derive(seed, "drive");
        Self {
            tasks,
            actions,
            busy: drive::busy_of(&mut drives),
            threads: drive::threads_of(&mut drives),
            sleepers: drive::sleepers_of(&mut Rng::derive(seed, "sleepers")),
        }
    }

    fn describe(&self) -> String {
        let tasks: Vec<String> = self
            .tasks
            .iter()
            .map(|task| {
                let hops: String = task
                    .hops
                    .iter()
                    .map(|hop| match hop {
                        Hop::Yield => "y".to_owned(),
                        Hop::Budget => "b".to_owned(),
                        Hop::Await(other) => format!("a{other}"),
                    })
                    .collect();
                format!(
                    "{}{}{hops}",
                    if task.in_set { "s:" } else { "" },
                    if task.panics { "!" } else { "" }
                )
            })
            .collect();
        let actions: Vec<String> = self
            .actions
            .iter()
            .filter_map(|action| match action {
                Action::Yield(_) => None,
                Action::Abort(index) => Some(format!("abort{index}")),
                Action::Detach(index) => Some(format!("detach{index}")),
            })
            .collect();
        let mut shape = format!(
            "scenario=executor tasks=[{}] actions=[{}]",
            tasks.join(" "),
            actions.join(" ")
        );
        for extra in [
            self.busy.map(Busy::describe),
            self.threads.map(Threads::describe),
            self.sleepers.as_ref().map(Sleepers::describe),
        ]
        .into_iter()
        .flatten()
        {
            shape.push(' ');
            shape.push_str(&extra);
        }
        shape
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Note {
    Hop(usize, usize),
    Woke(usize, usize, bool),
    Finished(usize),
    Panicked(usize),
    Dropped(usize),
    Aborted(usize, bool),
}

struct Notes(Slot<Vec<Note>>);

impl Notes {
    fn change<R>(&self, change: impl FnOnce(&mut Vec<Note>) -> R) -> R {
        let mut notes = self.0.take().expect(
            "a current-thread runtime polls one task at a time, so the notes are never taken twice",
        );
        let result = change(&mut notes);
        assert!(self.0.put(notes).is_none(), "the notes have one home");
        result
    }

    fn push(&self, note: Note) {
        self.change(|notes| notes.push(note));
    }

    fn read(&self) -> Vec<Note> {
        self.change(|notes| notes.clone())
    }
}

struct Guard {
    index: usize,
    notes: Arc<Notes>,
    completed: bool,
    leaks: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.leaks && !self.completed {
            return;
        }
        self.notes.push(Note::Dropped(self.index));
    }
}

fn value_of(index: usize, hops: usize) -> u64 {
    (wide(index) + 1) * VALUE_STRIDE + wide(hops)
}

async fn task(
    index: usize,
    hops: Vec<Hop>,
    panics: bool,
    mut waits: Vec<oneshot::Receiver<()>>,
    mut signals: Vec<oneshot::Sender<()>>,
    mut guard: Guard,
    mutation: TasksMutation,
) -> u64 {
    let notes = Arc::clone(&guard.notes);
    if mutation == TasksMutation::SignalEarly {
        for signal in std::mem::take(&mut signals) {
            signal.send(()).ok();
        }
    }
    for (at, hop) in hops.iter().enumerate() {
        notes.push(Note::Hop(index, at));
        match hop {
            Hop::Yield => yield_now().await,
            Hop::Budget => consume_budget().await,
            Hop::Await(_) if mutation == TasksMutation::SkipAwait => drop(waits.remove(0)),
            Hop::Await(other) => {
                let wait = waits.remove(0);
                let delivered = wait.await.is_ok();
                notes.push(Note::Woke(index, *other, delivered));
            }
        }
    }
    if panics {
        notes.push(Note::Panicked(index));
        std::panic::resume_unwind(Box::new(index));
    }
    if mutation == TasksMutation::SignalDropped {
        drop(signals);
    } else {
        for signal in signals {
            signal.send(()).ok();
        }
    }
    guard.completed = true;
    notes.push(Note::Finished(index));
    value_of(index, hops.len())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Joined {
    Value(u64),
    Cancelled,
    Panic(Option<usize>),
}

fn joined_of(outcome: Result<u64, JoinError>, mutation: TasksMutation) -> Joined {
    match outcome {
        Ok(value) => Joined::Value(value),
        Err(error) if error.is_cancelled() || mutation == TasksMutation::PanicAsCancelled => {
            Joined::Cancelled
        }
        Err(error) => Joined::Panic(
            error
                .into_panic()
                .downcast::<usize>()
                .ok()
                .map(|index| *index),
        ),
    }
}

async fn control(
    script: &Script,
    notes: &Arc<Notes>,
    mutation: TasksMutation,
) -> (Vec<(usize, Joined)>, u64) {
    let mut senders: Vec<Vec<oneshot::Sender<()>>> =
        script.tasks.iter().map(|_| Vec::new()).collect();
    let mut receivers: Vec<Vec<oneshot::Receiver<()>>> =
        script.tasks.iter().map(|_| Vec::new()).collect();
    for (index, plan) in script.tasks.iter().enumerate() {
        for hop in &plan.hops {
            if let Hop::Await(other) = hop {
                let (sender, receiver) = oneshot::channel();
                senders[*other].push(sender);
                receivers[index].push(receiver);
            }
        }
    }
    let mut handles: Vec<Option<JoinHandle<u64>>> = Vec::new();
    let mut aborts = Vec::new();
    let mut set = JoinSet::new();
    let mut members = BTreeMap::new();
    for (index, plan) in script.tasks.iter().enumerate() {
        let guard = Guard {
            index,
            notes: Arc::clone(notes),
            completed: false,
            leaks: mutation == TasksMutation::LeakCancelled,
        };
        let future = task(
            index,
            plan.hops.clone(),
            plan.panics,
            std::mem::take(&mut receivers[index]),
            std::mem::take(&mut senders[index]),
            guard,
            mutation,
        );
        if plan.in_set {
            let abort = set.spawn(future);
            members.insert(abort.id(), index);
            aborts.push(abort);
            handles.push(None);
        } else {
            let handle = nanocode_executor::spawn(future);
            aborts.push(handle.abort_handle());
            handles.push(Some(handle));
        }
    }
    let mut detached = 0;
    for action in &script.actions {
        match *action {
            Action::Yield(times) => {
                for _ in 0..times {
                    yield_now().await;
                }
            }
            Action::Abort(index) => {
                let finished = notes.read().iter().any(|note| {
                    matches!(note, Note::Finished(done) | Note::Panicked(done) if *done == index)
                });
                notes.push(Note::Aborted(index, finished));
                match mutation {
                    TasksMutation::SkipAbort => {}
                    TasksMutation::AbortsWrongTask => aborts[(index + 1) % aborts.len()].abort(),
                    _ => aborts[index].abort(),
                }
            }
            Action::Detach(index) => {
                if handles[index].take().is_some() {
                    detached += 1;
                }
            }
        }
    }
    let mut joined = Vec::new();
    for (index, handle) in handles.into_iter().enumerate() {
        if let Some(handle) = handle {
            joined.push((index, joined_of(handle.await, mutation)));
        }
    }
    while let Some(outcome) = set.join_next_with_id().await {
        let (id, result) = match outcome {
            Ok((id, value)) => (id, Ok(value)),
            Err(error) => (error.id(), Err(error)),
        };
        let index = members[&id];
        let outcome = joined_of(result, mutation);
        if mutation == TasksMutation::JoinsTwice {
            joined.push((index, outcome));
        }
        joined.push((index, outcome));
    }
    (joined, detached)
}

fn expected(index: usize, plan: &TaskPlan, notes: &[Note]) -> Result<Joined, String> {
    let finished = notes
        .iter()
        .position(|note| *note == Note::Finished(index) || *note == Note::Panicked(index));
    let aborted = notes
        .iter()
        .position(|note| matches!(note, Note::Aborted(other, false) if *other == index));
    match (finished, aborted) {
        (Some(done), Some(abort)) if abort < done => Err(format!(
            "task {index} ran to its end after an abort that found it unfinished"
        )),
        (_, Some(_)) => Ok(Joined::Cancelled),
        (Some(_), None) if plan.panics => Ok(Joined::Panic(Some(index))),
        (Some(_), None) => Ok(Joined::Value(value_of(index, plan.hops.len()))),
        (None, None) => Err(format!("task {index} was joined but never finished")),
    }
}

fn check(script: &Script, notes: &[Note], joined: &[(usize, Joined)]) -> Result<(), String> {
    for (index, plan) in script.tasks.iter().enumerate() {
        let drops = notes
            .iter()
            .filter(|note| **note == Note::Dropped(index))
            .count();
        if drops != 1 {
            return Err(format!(
                "task {index}'s future was dropped {drops} times; the runtime drops every future exactly once"
            ));
        }
        if let Some(abort) = notes
            .iter()
            .position(|note| matches!(note, Note::Aborted(other, false) if *other == index))
            && let Some(late) = notes[abort..].iter().find(|note| {
                matches!(note, Note::Hop(other, _) | Note::Woke(other, _, _) | Note::Finished(other) if *other == index)
            })
        {
            return Err(format!("task {index} ran ({late:?}) after it was aborted unfinished"));
        }
        let mut awaited = plan
            .hops
            .iter()
            .filter(|hop| matches!(hop, Hop::Await(_)))
            .count();
        for note in notes {
            if let Note::Woke(other, from, delivered) = note
                && *other == index
            {
                awaited -= 1;
                let sent = notes.contains(&Note::Finished(*from));
                if *delivered && !sent {
                    return Err(format!(
                        "task {index} heard from task {from}, which never finished"
                    ));
                }
                if !*delivered && sent {
                    return Err(format!(
                        "task {index} found task {from}'s signal closed though task {from} finished"
                    ));
                }
            }
        }
        if notes.contains(&Note::Finished(index)) && awaited != 0 {
            return Err(format!(
                "task {index} finished with {awaited} waits unheard"
            ));
        }
    }
    let mut seen = vec![0_u32; script.tasks.len()];
    for (index, outcome) in joined {
        seen[*index] += 1;
        let want = expected(*index, &script.tasks[*index], notes)?;
        if *outcome != want {
            return Err(format!(
                "task {index} joined as {outcome:?}, expected {want:?}"
            ));
        }
    }
    if let Some(twice) = seen.iter().position(|count| *count > 1) {
        return Err(format!("task {twice} was joined {} times", seen[twice]));
    }
    Ok(())
}

pub struct TasksOutcome {
    pub digest: u64,
    pub wrong: Option<String>,
    pub stats: TasksStats,
    pub shape: String,
}

fn simulate(seed: u64, mutation: TasksMutation) -> TasksOutcome {
    let script = Script::of(seed);
    let shape = script.describe();
    let runtime = Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds");
    let notes = Arc::new(Notes(Slot::new(Some(Vec::new()))));
    sim::mutate(mutation.executor());
    let (joined, detached) = runtime.block_on(control(&script, &notes, mutation));
    drop(runtime);
    let busy = script.busy.map_or(Ok(()), |plan| {
        Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime builds")
            .block_on(drive::busy(plan))
    });
    let threaded = script.threads.map(drive::threads);
    let slept = script.sleepers.as_ref().map(drive::sleepers);
    sim::mutate(sim::Mutation::None);
    let notes = notes.read();
    let mut digest = fnv(FNV_OFFSET, shape.as_bytes());
    for note in &notes {
        digest = fnv(digest, format!("{note:?}").as_bytes());
    }
    for outcome in &joined {
        digest = fnv(digest, format!("{outcome:?}").as_bytes());
    }
    digest = fnv(digest, format!("{busy:?}").as_bytes());
    if let Some(threaded) = &threaded {
        digest = fnv(digest, &threaded.received.to_le_bytes());
    }
    if let Some(slept) = &slept {
        digest = fnv(digest, &slept.naps.to_le_bytes());
    }
    let wrong = check(&script, &notes, &joined).and(busy).and_then(|()| {
        match threaded
            .as_ref()
            .and_then(|threaded| threaded.stalled.clone())
        {
            Some(stalled) => Err(stalled),
            None => Ok(()),
        }
    }).and_then(|()| {
        match slept.as_ref().and_then(|slept| slept.stalled.clone()) {
            Some(stalled) => Err(stalled),
            None => Ok(()),
        }
    });
    let count =
        |test: fn(&(usize, Joined)) -> bool| wide(joined.iter().filter(|item| test(item)).count());
    TasksOutcome {
        digest,
        wrong: wrong.err(),
        stats: TasksStats {
            seeds: 1,
            tasks: wide(script.tasks.len()),
            finished: count(|(_, outcome)| matches!(outcome, Joined::Value(_))),
            cancelled: count(|(_, outcome)| *outcome == Joined::Cancelled),
            unwound: count(|(_, outcome)| matches!(outcome, Joined::Panic(_))),
            detached,
            busy: u64::from(script.busy.is_some()),
            threaded: u64::from(threaded.is_some()),
            messages: threaded.as_ref().map_or(0, |threaded| threaded.received),
            sleepers: u64::from(slept.is_some()),
            naps: slept.as_ref().map_or(0, |slept| slept.naps),
        },
        shape,
    }
}

pub fn run_seed(seed: u64, mutation: TasksMutation) -> TasksOutcome {
    std::panic::catch_unwind(|| simulate(seed, mutation)).unwrap_or_else(|panic| {
        sim::mutate(sim::Mutation::None);
        sim::pause_before_drive(std::time::Duration::ZERO);
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default();
        TasksOutcome {
            digest: 0,
            wrong: Some(format!("panic: {message}")),
            stats: TasksStats::default(),
            shape: String::new(),
        }
    })
}

pub fn judge(
    seed: u64,
    mutation: TasksMutation,
    twice: bool,
    verbose: bool,
    totals: &mut TasksStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!("seed {seed} digest {:016x} {}", first.digest, first.shape);
    }
    if first.wrong.is_some() || !twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let second = run_seed(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: trace digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: TasksMutation) -> String {
    let mut command = format!("nanocode-sim --scenario executor --seed {seed}");
    if mutation != TasksMutation::None {
        command.push_str(" --mutation ");
        command.push_str(mutation.name());
    }
    command
}

pub fn report(
    seeds: &Range<u64>,
    passed: u64,
    failed: u64,
    twice: bool,
    mutation: TasksMutation,
    totals: &TasksStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=executor mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        TasksMutation::None if failed == 0 => ExitCode::SUCCESS,
        TasksMutation::None => ExitCode::FAILURE,
        _ if failed > 0 => {
            println!("MUTATION {} caught by {failed} seeds", mutation.name());
            ExitCode::SUCCESS
        }
        _ => {
            println!("MUTATION {} NOT caught", mutation.name());
            ExitCode::FAILURE
        }
    }
}
