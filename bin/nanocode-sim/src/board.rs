use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use nanocode_board::{
    Applied, Board, BoardError, ClaimId, EventId, NewTask, Op, Outcome, Phase, Rejection, Say,
    TaskId,
    sim::{self as board_sim, Mutation as BoardRule},
    stage::Stage,
    watch::Step,
};
use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_sync::Slot;
use nanocode_turnlog::{LogHost, MemoryLog, StoredCheckpoint};

use crate::{
    executor::{Clock, Ended, Executor, Task},
    rng::{FNV_OFFSET, Rng, fnv},
};

#[path = "board_access.rs"]
mod access;
#[path = "board_deliver.rs"]
mod deliver;
#[path = "board_due.rs"]
mod due;
#[path = "board_intent.rs"]
mod intent;
#[path = "board_messages.rs"]
mod messages;
#[path = "board_tasks.rs"]
mod tasks;

pub(crate) use access::identity;

const SCOPE: &str = "board";
const MS: u64 = 1_000_000;

#[path = "board_watch.rs"]
mod watching;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoardMutation {
    None,
    ClaimIgnoresLease,
    ClaimIgnoresNeeds,
    FinishIgnoresClaim,
    PostOverwrites,
    LeasesNeverExpire,
    ErrorsLoseWrites,
    PostIgnoresDoomedNeeds,
    KeysIgnored,
    CursorBeforeEffect,
    EffectBeforeCursor,
    GrantsIgnored,
    ManageUnchecked,
    RevokeIgnored,
    SpecificityIgnored,
    SubtreeSkipsRoot,
    SourceUncapped,
    CancelByAnyone,
    ForeignFinish,
    ReadLeaks,
    KeySquatting,
    MintedRestarts,
    ClaimIgnoresBlock,
    UnblockAnything,
    ReopenAnything,
    FailIgnoresPolicy,
    ClaimIgnoresAssignee,
    BatchIgnoresCycles,
    NextIgnoresAssignee,
    RunIgnoresLoss,
    TasksUnchecked,
    RelabelUnchecked,
    RelabelKeepsUnset,
    RankUnchecked,
    NextIgnoresRank,
    SpreadIgnoresTier,
    MoveUnchecked,
    MoveAllowsCycle,
    MoveKeepsChildren,
    ReadyIndexStale,
    LabelIndexStale,
    AckSkipsFold,
    ResumeSkipsRows,
    SnapshotSkipsRows,
    SnapshotLosesDedupe,
    CheckpointVersionIgnored,
    CheckpointTailUnchecked,
    CheckpointAhead,
    CheckpointEveryFixed,
    EvictionDiffersByReplica,
    EventsUncapped,
    GoneUnbounded,
    TextLimitUnmanaged,
    TextLimitOffByOne,
    TextLimitStuck,
    RowBoundIgnored,
    DueIgnored,
    DueFiresTwice,
    RecurrenceSkipsOnRestart,
    AttemptsNotCounted,
    WakeIgnoresDue,
    CronNamesShifted,
    EditsByAnyone,
    TombstonesEditable,
    RepliesNest,
    ReservedLabelsWritable,
    NamesUnchecked,
    ReactionsShared,
    UnknownFilesAccepted,
    FileLimitIgnored,
    TextLimitIgnored,
    SchemasMutable,
    SchemaShapeIgnored,
    BlocksUnchecked,
    TaskLabelStale,
    DeleteKeepsTask,
    TaskRootsEditable,
    MiddlewareEveryAttempt,
    MiddlewareRerunsLanded,
    CursorInclusive,
    SaysUnchecked,
    MessagesLeak,
    FencedUnsettled,
    Watch(BoardRule),
    Deliver(BoardRule),
}

impl BoardMutation {
    const ALL: [Self; 83] = [
        Self::None,
        Self::ClaimIgnoresLease,
        Self::ClaimIgnoresNeeds,
        Self::FinishIgnoresClaim,
        Self::PostOverwrites,
        Self::LeasesNeverExpire,
        Self::ErrorsLoseWrites,
        Self::PostIgnoresDoomedNeeds,
        Self::KeysIgnored,
        Self::CursorBeforeEffect,
        Self::EffectBeforeCursor,
        Self::GrantsIgnored,
        Self::ManageUnchecked,
        Self::RevokeIgnored,
        Self::SpecificityIgnored,
        Self::SubtreeSkipsRoot,
        Self::SourceUncapped,
        Self::CancelByAnyone,
        Self::ForeignFinish,
        Self::ReadLeaks,
        Self::KeySquatting,
        Self::MintedRestarts,
        Self::ClaimIgnoresBlock,
        Self::UnblockAnything,
        Self::ReopenAnything,
        Self::FailIgnoresPolicy,
        Self::ClaimIgnoresAssignee,
        Self::BatchIgnoresCycles,
        Self::NextIgnoresAssignee,
        Self::RunIgnoresLoss,
        Self::TasksUnchecked,
        Self::RelabelUnchecked,
        Self::RelabelKeepsUnset,
        Self::RankUnchecked,
        Self::NextIgnoresRank,
        Self::SpreadIgnoresTier,
        Self::MoveUnchecked,
        Self::MoveAllowsCycle,
        Self::MoveKeepsChildren,
        Self::ReadyIndexStale,
        Self::LabelIndexStale,
        Self::AckSkipsFold,
        Self::ResumeSkipsRows,
        Self::SnapshotSkipsRows,
        Self::SnapshotLosesDedupe,
        Self::CheckpointVersionIgnored,
        Self::CheckpointTailUnchecked,
        Self::CheckpointAhead,
        Self::CheckpointEveryFixed,
        Self::EvictionDiffersByReplica,
        Self::EventsUncapped,
        Self::GoneUnbounded,
        Self::TextLimitUnmanaged,
        Self::TextLimitOffByOne,
        Self::TextLimitStuck,
        Self::RowBoundIgnored,
        Self::DueIgnored,
        Self::DueFiresTwice,
        Self::RecurrenceSkipsOnRestart,
        Self::AttemptsNotCounted,
        Self::WakeIgnoresDue,
        Self::CronNamesShifted,
        Self::EditsByAnyone,
        Self::TombstonesEditable,
        Self::RepliesNest,
        Self::ReservedLabelsWritable,
        Self::NamesUnchecked,
        Self::ReactionsShared,
        Self::UnknownFilesAccepted,
        Self::FileLimitIgnored,
        Self::TextLimitIgnored,
        Self::SchemasMutable,
        Self::SchemaShapeIgnored,
        Self::BlocksUnchecked,
        Self::TaskLabelStale,
        Self::DeleteKeepsTask,
        Self::TaskRootsEditable,
        Self::MiddlewareEveryAttempt,
        Self::MiddlewareRerunsLanded,
        Self::CursorInclusive,
        Self::SaysUnchecked,
        Self::MessagesLeak,
        Self::FencedUnsettled,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .chain(watching::ALL.map(Self::Watch))
            .chain(intent::ALL.map(Self::Watch))
            .chain(deliver::ALL.map(Self::Deliver))
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ClaimIgnoresLease => "claim_ignores_lease",
            Self::ClaimIgnoresNeeds => "claim_ignores_needs",
            Self::FinishIgnoresClaim => "finish_ignores_claim",
            Self::PostOverwrites => "post_overwrites",
            Self::LeasesNeverExpire => "leases_never_expire",
            Self::ErrorsLoseWrites => "errors_lose_writes",
            Self::PostIgnoresDoomedNeeds => "post_ignores_doomed_needs",
            Self::KeysIgnored => "keys_ignored",
            Self::CursorBeforeEffect => "cursor_before_effect",
            Self::EffectBeforeCursor => "effect_before_cursor",
            Self::GrantsIgnored => "grants_ignored",
            Self::ManageUnchecked => "manage_unchecked",
            Self::RevokeIgnored => "revoke_ignored",
            Self::SpecificityIgnored => "specificity_ignored",
            Self::SubtreeSkipsRoot => "subtree_skips_root",
            Self::SourceUncapped => "source_uncapped",
            Self::CancelByAnyone => "cancel_by_anyone",
            Self::ForeignFinish => "foreign_finish",
            Self::ReadLeaks => "read_leaks",
            Self::KeySquatting => "key_squatting",
            Self::MintedRestarts => "minted_restarts",
            Self::ClaimIgnoresBlock => "claim_ignores_block",
            Self::UnblockAnything => "unblock_anything",
            Self::ReopenAnything => "reopen_anything",
            Self::FailIgnoresPolicy => "fail_ignores_policy",
            Self::ClaimIgnoresAssignee => "claim_ignores_assignee",
            Self::BatchIgnoresCycles => "batch_ignores_cycles",
            Self::NextIgnoresAssignee => "next_ignores_assignee",
            Self::RunIgnoresLoss => "run_ignores_loss",
            Self::TasksUnchecked => "tasks_unchecked",
            Self::RelabelUnchecked => "relabel_unchecked",
            Self::RelabelKeepsUnset => "relabel_keeps_unset",
            Self::RankUnchecked => "rank_unchecked",
            Self::NextIgnoresRank => "next_ignores_rank",
            Self::SpreadIgnoresTier => "spread_ignores_tier",
            Self::MoveUnchecked => "move_unchecked",
            Self::MoveAllowsCycle => "move_allows_cycle",
            Self::MoveKeepsChildren => "move_keeps_children",
            Self::ReadyIndexStale => "ready_index_stale",
            Self::LabelIndexStale => "label_index_stale",
            Self::AckSkipsFold => "ack_skips_fold",
            Self::ResumeSkipsRows => "resume_skips_rows",
            Self::SnapshotSkipsRows => "snapshot_skips_rows",
            Self::SnapshotLosesDedupe => "snapshot_loses_dedupe",
            Self::CheckpointVersionIgnored => "checkpoint_version_ignored",
            Self::CheckpointTailUnchecked => "checkpoint_tail_unchecked",
            Self::CheckpointAhead => "checkpoint_ahead",
            Self::CheckpointEveryFixed => "checkpoint_every_fixed",
            Self::EvictionDiffersByReplica => "eviction_differs_by_replica",
            Self::EventsUncapped => "events_uncapped",
            Self::GoneUnbounded => "gone_unbounded",
            Self::TextLimitUnmanaged => "text_limit_unmanaged",
            Self::TextLimitOffByOne => "text_limit_off_by_one",
            Self::TextLimitStuck => "text_limit_stuck",
            Self::RowBoundIgnored => "row_bound_ignored",
            Self::DueIgnored => "due_ignored",
            Self::DueFiresTwice => "due_fires_twice",
            Self::RecurrenceSkipsOnRestart => "recurrence_skips_on_restart",
            Self::AttemptsNotCounted => "attempts_not_counted",
            Self::WakeIgnoresDue => "wake_ignores_due",
            Self::CronNamesShifted => "cron_names_shifted",
            Self::EditsByAnyone => "edits_by_anyone",
            Self::TombstonesEditable => "tombstones_editable",
            Self::RepliesNest => "replies_nest",
            Self::ReservedLabelsWritable => "reserved_labels_writable",
            Self::NamesUnchecked => "names_unchecked",
            Self::ReactionsShared => "reactions_shared",
            Self::UnknownFilesAccepted => "unknown_files_accepted",
            Self::FileLimitIgnored => "file_limit_ignored",
            Self::TextLimitIgnored => "text_limit_ignored",
            Self::SchemasMutable => "schemas_mutable",
            Self::SchemaShapeIgnored => "schema_shape_ignored",
            Self::BlocksUnchecked => "blocks_unchecked",
            Self::TaskLabelStale => "task_label_stale",
            Self::DeleteKeepsTask => "delete_keeps_task",
            Self::TaskRootsEditable => "task_roots_editable",
            Self::MiddlewareEveryAttempt => "middleware_every_attempt",
            Self::MiddlewareRerunsLanded => "middleware_reruns_landed",
            Self::CursorInclusive => "cursor_inclusive",
            Self::SaysUnchecked => "says_unchecked",
            Self::MessagesLeak => "messages_leak",
            Self::FencedUnsettled => "fenced_unsettled",
            Self::Watch(rule) => match intent::name(rule) {
                Some(name) => name,
                None => watching::name(rule),
            },
            Self::Deliver(rule) => deliver::name(rule),
        }
    }

    pub fn names() -> String {
        Self::ALL
            .iter()
            .skip(1)
            .copied()
            .chain(watching::ALL.map(Self::Watch))
            .chain(intent::ALL.map(Self::Watch))
            .chain(deliver::ALL.map(Self::Deliver))
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }

    const fn rule(self) -> BoardRule {
        match self {
            Self::ClaimIgnoresLease => BoardRule::ClaimIgnoresLease,
            Self::ClaimIgnoresNeeds => BoardRule::ClaimIgnoresNeeds,
            Self::FinishIgnoresClaim => BoardRule::FinishIgnoresClaim,
            Self::PostOverwrites => BoardRule::PostOverwrites,
            Self::LeasesNeverExpire => BoardRule::LeasesNeverExpire,
            Self::ErrorsLoseWrites => BoardRule::ErrorsLoseWrites,
            Self::PostIgnoresDoomedNeeds => BoardRule::PostIgnoresDoomedNeeds,
            Self::KeysIgnored => BoardRule::KeysIgnored,
            Self::GrantsIgnored => BoardRule::GrantsIgnored,
            Self::ManageUnchecked => BoardRule::ManageUnchecked,
            Self::RevokeIgnored => BoardRule::RevokeIgnored,
            Self::SpecificityIgnored => BoardRule::SpecificityIgnored,
            Self::SubtreeSkipsRoot => BoardRule::SubtreeSkipsRoot,
            Self::SourceUncapped => BoardRule::SourceUncapped,
            Self::CancelByAnyone => BoardRule::CancelByAnyone,
            Self::ForeignFinish => BoardRule::ForeignFinish,
            Self::ReadLeaks => BoardRule::ReadLeaks,
            Self::KeySquatting => BoardRule::KeySquatting,
            Self::MintedRestarts => BoardRule::MintedRestarts,
            Self::ClaimIgnoresBlock => BoardRule::ClaimIgnoresBlock,
            Self::UnblockAnything => BoardRule::UnblockAnything,
            Self::ReopenAnything => BoardRule::ReopenAnything,
            Self::FailIgnoresPolicy => BoardRule::FailIgnoresPolicy,
            Self::ClaimIgnoresAssignee => BoardRule::ClaimIgnoresAssignee,
            Self::BatchIgnoresCycles => BoardRule::BatchIgnoresCycles,
            Self::NextIgnoresAssignee => BoardRule::NextIgnoresAssignee,
            Self::RunIgnoresLoss => BoardRule::RunIgnoresLoss,
            Self::TasksUnchecked => BoardRule::TasksUnchecked,
            Self::RelabelUnchecked => BoardRule::RelabelUnchecked,
            Self::RelabelKeepsUnset => BoardRule::RelabelKeepsUnset,
            Self::RankUnchecked => BoardRule::RankUnchecked,
            Self::NextIgnoresRank => BoardRule::NextIgnoresRank,
            Self::SpreadIgnoresTier => BoardRule::SpreadIgnoresTier,
            Self::MoveUnchecked => BoardRule::MoveUnchecked,
            Self::MoveAllowsCycle => BoardRule::MoveAllowsCycle,
            Self::MoveKeepsChildren => BoardRule::MoveKeepsChildren,
            Self::ReadyIndexStale => BoardRule::ReadyIndexStale,
            Self::LabelIndexStale => BoardRule::LabelIndexStale,
            Self::AckSkipsFold => BoardRule::AckSkipsFold,
            Self::ResumeSkipsRows => BoardRule::ResumeSkipsRows,
            Self::SnapshotSkipsRows => BoardRule::SnapshotSkipsRows,
            Self::SnapshotLosesDedupe => BoardRule::SnapshotLosesDedupe,
            Self::CheckpointVersionIgnored => BoardRule::CheckpointVersionIgnored,
            Self::CheckpointTailUnchecked => BoardRule::CheckpointTailUnchecked,
            Self::CheckpointAhead => BoardRule::CheckpointAhead,
            Self::CheckpointEveryFixed => BoardRule::CheckpointEveryFixed,
            Self::EvictionDiffersByReplica => BoardRule::EvictionDiffersByReplica,
            Self::EventsUncapped => BoardRule::EventsUncapped,
            Self::GoneUnbounded => BoardRule::GoneUnbounded,
            Self::TextLimitUnmanaged => BoardRule::TextLimitUnmanaged,
            Self::TextLimitOffByOne => BoardRule::TextLimitOffByOne,
            Self::TextLimitStuck => BoardRule::TextLimitStuck,
            Self::RowBoundIgnored => BoardRule::RowBoundIgnored,
            Self::DueIgnored => BoardRule::DueIgnored,
            Self::DueFiresTwice => BoardRule::DueFiresTwice,
            Self::RecurrenceSkipsOnRestart => BoardRule::RecurrenceSkipsOnRestart,
            Self::AttemptsNotCounted => BoardRule::AttemptsNotCounted,
            Self::WakeIgnoresDue => BoardRule::WakeIgnoresDue,
            Self::CronNamesShifted => BoardRule::CronNamesShifted,
            Self::EditsByAnyone => BoardRule::EditsByAnyone,
            Self::TombstonesEditable => BoardRule::TombstonesEditable,
            Self::RepliesNest => BoardRule::RepliesNest,
            Self::ReservedLabelsWritable => BoardRule::ReservedLabelsWritable,
            Self::NamesUnchecked => BoardRule::NamesUnchecked,
            Self::ReactionsShared => BoardRule::ReactionsShared,
            Self::UnknownFilesAccepted => BoardRule::UnknownFilesAccepted,
            Self::FileLimitIgnored => BoardRule::FileLimitIgnored,
            Self::TextLimitIgnored => BoardRule::TextLimitIgnored,
            Self::SchemasMutable => BoardRule::SchemasMutable,
            Self::SchemaShapeIgnored => BoardRule::SchemaShapeIgnored,
            Self::BlocksUnchecked => BoardRule::BlocksUnchecked,
            Self::TaskLabelStale => BoardRule::TaskLabelStale,
            Self::DeleteKeepsTask => BoardRule::DeleteKeepsTask,
            Self::TaskRootsEditable => BoardRule::TaskRootsEditable,
            Self::MiddlewareEveryAttempt => BoardRule::MiddlewareEveryAttempt,
            Self::MiddlewareRerunsLanded => BoardRule::MiddlewareRerunsLanded,
            Self::CursorInclusive => BoardRule::CursorInclusive,
            Self::SaysUnchecked => BoardRule::SaysUnchecked,
            Self::MessagesLeak => BoardRule::MessagesLeak,
            Self::FencedUnsettled => BoardRule::FencedUnsettled,
            Self::Watch(rule) | Self::Deliver(rule) => rule,
            Self::None | Self::CursorBeforeEffect | Self::EffectBeforeCursor => BoardRule::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BoardStats {
    pub seeds: u64,
    pub tasks: u64,
    pub workers: u64,
    pub rows: u64,
    pub claims: u64,
    pub takeovers: u64,
    pub resumed: u64,
    pub rejected: u64,
    pub unsettled: u64,
    pub conflicts: u64,
    pub faults: u64,
    pub crashes: u64,
    pub done: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub consumed: u64,
    pub forged: u64,
    pub stable: u64,
    pub continued: u64,
    pub batched: u64,
    pub picks: u64,
    pub runs: u64,
    pub lost: u64,
    pub blocks: u64,
    pub reopens: u64,
    pub checkpoints: u64,
    pub checkpoint_opens: u64,
    pub evicted: u64,
    pub due_runs: u64,
    pub due_retries: u64,
    pub due_restarts: u64,
    pub deliveries: u64,
    pub redeliveries: u64,
    pub delivery_takeovers: u64,
}

impl BoardStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.tasks += other.tasks;
        self.workers += other.workers;
        self.rows += other.rows;
        self.claims += other.claims;
        self.takeovers += other.takeovers;
        self.resumed += other.resumed;
        self.rejected += other.rejected;
        self.unsettled += other.unsettled;
        self.conflicts += other.conflicts;
        self.faults += other.faults;
        self.crashes += other.crashes;
        self.done += other.done;
        self.failed += other.failed;
        self.cancelled += other.cancelled;
        self.consumed += other.consumed;
        self.forged += other.forged;
        self.stable += other.stable;
        self.continued += other.continued;
        self.batched += other.batched;
        self.picks += other.picks;
        self.runs += other.runs;
        self.lost += other.lost;
        self.blocks += other.blocks;
        self.reopens += other.reopens;
        self.checkpoints += other.checkpoints;
        self.checkpoint_opens += other.checkpoint_opens;
        self.evicted += other.evicted;
        self.due_runs += other.due_runs;
        self.due_retries += other.due_retries;
        self.due_restarts += other.due_restarts;
        self.deliveries += other.deliveries;
        self.redeliveries += other.redeliveries;
        self.delivery_takeovers += other.delivery_takeovers;
    }
}

struct Shape {
    seed: u64,
    workers: u64,
    lease_ms: u64,
    poll_ms: u64,
    step_ms: u64,
    stall_ppm: u64,
    fail_ppm: u64,
    release_ppm: u64,
    before_ppm: u64,
    after_ppm: u64,
    read_ppm: u64,
    latency_ms: u64,
    plan: Vec<NewTask>,
    prs: Vec<u64>,
    hooks: Vec<Hook>,
    cancel: Option<(TaskId, u64)>,
    cancel_early: bool,
    crashes: Vec<u64>,
    deadline_ms: u64,
    stable: bool,
    tasks: tasks::Plan,
}

fn task(id: String, parent: Option<&TaskId>, needs: Vec<TaskId>, step: &str) -> NewTask {
    NewTask {
        id: TaskId(id),
        parent: parent.cloned(),
        spec: format!("{step} work"),
        labels: [("step", step)]
            .into_iter()
            .chain((step == "consolidate").then_some(("gate", "join")))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        needs,
        terms: nanocode_board::tasks::Terms::default(),
    }
}

impl Shape {
    fn of(seed: u64) -> Self {
        let mut rng = Rng::derive(seed, "board shape");
        let root = TaskId("goal".to_owned());
        let mut plan = vec![task(root.0.clone(), None, Vec::new(), "goal")];
        for repo in 0..rng.range(1, 4) {
            let lane = TaskId(format!("repo{repo}"));
            plan.push(task(lane.0.clone(), Some(&root), Vec::new(), "lane"));
            let mut verifies = Vec::new();
            for fix in 0..rng.range(1, 5) {
                let id = TaskId(format!("repo{repo}/fix{fix}"));
                plan.push(task(id.0.clone(), Some(&lane), Vec::new(), "fix"));
                let verify = TaskId(format!("repo{repo}/verify{fix}"));
                plan.push(task(verify.0.clone(), Some(&lane), vec![id], "verify"));
                verifies.push(verify);
            }
            plan.push(task(
                format!("repo{repo}/consolidate"),
                Some(&lane),
                verifies,
                "consolidate",
            ));
        }
        let lease_ms = rng.range(20, 200);
        let step_ms = rng.range(1, lease_ms / 3);
        let poll_ms = rng.range(1, lease_ms / 2);
        let latency_ms = rng.range(0, step_ms);
        let workers = rng.range(1, 6);
        let fixes = plan
            .iter()
            .filter(|task| task.labels["step"] == "fix")
            .collect::<Vec<_>>();
        let cancel = rng
            .chance(250_000)
            .then(|| fixes[rng.pick(fixes.len())].id.clone())
            .map(|task| (task, rng.range(0, lease_ms * 4)));
        let work_ms = plan.len() as u64 * 4 * (step_ms + latency_ms * 4 + poll_ms) + lease_ms * 2;
        let crashes = (0..rng.range(0, 3))
            .map(|_| rng.range(1, work_ms))
            .collect();
        let faulty = !rng.chance(200_000);
        let prs = (0..rng.range(0, 6)).collect::<Vec<_>>();
        let mut hooks = Vec::new();
        for pr in &prs {
            let opened = rng.range(0, work_ms / 2);
            hooks.push(Hook {
                key: format!("github:{pr}:opened"),
                at_ms: opened,
                pr: *pr,
                review: None,
            });
            for review in 0..rng.range(0, 3) {
                hooks.push(Hook {
                    key: format!("github:{pr}:review:{review}"),
                    at_ms: opened + rng.range(0, work_ms / 2),
                    pr: *pr,
                    review: Some(review),
                });
            }
        }
        let tasks = tasks::Plan::of(seed, &mut plan, workers, lease_ms);
        let mut later = Rng::derive(seed, "board early cancel");
        let cancel_early = later.chance(700_000);
        let posted = plan
            .iter()
            .filter(|task| {
                task.labels["step"] == "fix"
                    && !task
                        .parent
                        .as_ref()
                        .is_some_and(|lane| tasks.batched.contains(lane))
            })
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();
        let cancel = cancel.or_else(|| {
            (!posted.is_empty() && later.chance(700_000)).then(|| {
                (
                    posted[later.pick(posted.len())].clone(),
                    later.range(0, lease_ms * 4),
                )
            })
        });
        Self {
            seed,
            tasks,
            workers,
            lease_ms,
            poll_ms,
            step_ms,
            stall_ppm: rng.range(0, 150_000),
            fail_ppm: rng.range(0, 60_000),
            release_ppm: rng.range(0, 60_000),
            before_ppm: if faulty { rng.range(0, 80_000) } else { 0 },
            after_ppm: if faulty { rng.range(0, 80_000) } else { 0 },
            read_ppm: if faulty { rng.range(0, 40_000) } else { 0 },
            latency_ms,
            deadline_ms: work_ms * 8 + lease_ms * 8,
            plan,
            prs,
            hooks,
            cancel,
            cancel_early,
            crashes,
            stable: Rng::derive(seed, "board identity").chance(500_000),
        }
    }

    fn describe(&self) -> String {
        format!(
            "tasks={} hooks={} workers={} lease_ms={} step_ms={} poll_ms={} latency_ms={} stall_ppm={} fail_ppm={} release_ppm={} faults_ppm={}/{}/{} cancel={} crashes={:?} identity={}",
            self.plan.len(),
            self.hooks.len(),
            self.workers,
            self.lease_ms,
            self.step_ms,
            self.poll_ms,
            self.latency_ms,
            self.stall_ppm,
            self.fail_ppm,
            self.release_ppm,
            self.before_ppm,
            self.after_ppm,
            self.read_ppm,
            self.cancel
                .as_ref()
                .map_or("none".to_owned(), |(task, _)| task.0.clone()),
            self.crashes,
            if self.stable { "stable" } else { "epoch" }
        ) + " "
            + &self.tasks.describe()
    }
}

pub(crate) struct FaultLog {
    pub(crate) inner: MemoryLog,
    pub(crate) rng: Slot<Rng>,
    pub(crate) clock: Arc<Clock>,
    pub(crate) before_ppm: u64,
    pub(crate) after_ppm: u64,
    pub(crate) read_ppm: u64,
    pub(crate) latency_ms: u64,
    pub(crate) faults: AtomicU64,
    pub(crate) conflicts: AtomicU64,
    pub(crate) fenced: AtomicU64,
    pub(crate) checkpoint_reads: AtomicU64,
    pub(crate) checkpoint_saves: AtomicU64,
    pub(crate) checkpoint_encoded: AtomicU64,
    pub(crate) row_bytes_max: Option<u64>,
    pub(crate) oversize: AtomicU64,
}

impl FaultLog {
    fn roll<T>(&self, each: impl FnOnce(&mut Rng) -> T) -> T {
        let mut rng = self
            .rng
            .take()
            .expect("the simulator polls one task at a time");
        let value = each(&mut rng);
        assert!(self.rng.put(rng).is_none(), "the fault rng has one home");
        value
    }

    fn delay(&self) -> Duration {
        Duration::from_nanos(self.roll(|rng| rng.range(0, self.latency_ms * MS)))
    }

    fn injected(&self, what: &str) -> StoreError {
        self.faults.fetch_add(1, Ordering::SeqCst);
        StoreError::Backend(format!("injected board {what} fault"))
    }

    async fn read(
        &self,
        scope: &str,
        from: u64,
        to: u64,
    ) -> Result<Vec<(u64, String)>, StoreError> {
        self.clock.sleep(self.delay()).await;
        if self.roll(|rng| rng.chance(self.read_ppm)) {
            return Err(self.injected("read"));
        }
        let rows = self.inner.scan(scope, from, to).await;
        self.clock.sleep(self.delay()).await;
        rows
    }
}

impl LogHost for FaultLog {
    fn call_bound(&self) -> Option<std::time::Duration> {
        None
    }

    fn row_bytes_max(&self) -> Option<u64> {
        self.row_bytes_max
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(self.read(scope, from, to))
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(self.read(scope, 0, i64::MAX as u64))
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            if let Some(max) = self.row_bytes_max {
                let over = rows
                    .iter()
                    .filter(|(_, body)| (scope.len() + body.len()) as u64 > max)
                    .count();
                self.oversize.fetch_add(over as u64, Ordering::SeqCst);
            }
            self.clock.sleep(self.delay()).await;
            if self.roll(|rng| rng.chance(self.before_ppm)) {
                if self.fenced.fetch_add(1, Ordering::SeqCst) % 2 == 1 {
                    self.faults.fetch_add(1, Ordering::SeqCst);
                    return Err(StoreError::Fenced);
                }
                return Err(self.injected("append before"));
            }
            let appended = self.inner.append(scope, rows).await;
            if appended.is_err() {
                self.conflicts.fetch_add(1, Ordering::SeqCst);
            }
            self.clock.sleep(self.delay()).await;
            if appended.is_ok() && self.roll(|rng| rng.chance(self.after_ppm)) {
                return Err(self.injected("append after"));
            }
            appended
        })
    }

    fn checkpoint<'a>(
        &'a self,
        scope: &'a str,
    ) -> Option<StoreFuture<'a, Result<Option<StoredCheckpoint>, StoreError>>> {
        Some(Box::pin(async move {
            self.checkpoint_reads.fetch_add(1, Ordering::SeqCst);
            self.clock.sleep(self.delay()).await;
            if self.roll(|rng| rng.chance(self.read_ppm)) {
                return Err(self.injected("checkpoint read"));
            }
            self.inner
                .checkpoint(scope)
                .expect("a memory log keeps checkpoints")
                .await
        }))
    }

    fn save_checkpoint<'a>(
        &'a self,
        scope: &'a str,
        checkpoint: &'a StoredCheckpoint,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        Some(Box::pin(async move {
            self.checkpoint_encoded
                .fetch_add(checkpoint.seq, Ordering::SeqCst);
            self.clock.sleep(self.delay()).await;
            if self.roll(|rng| rng.chance(self.before_ppm)) {
                return Err(self.injected("checkpoint save"));
            }
            let saved = self
                .inner
                .save_checkpoint(scope, checkpoint)
                .expect("a memory log keeps checkpoints")
                .await;
            if saved.is_ok() {
                self.checkpoint_saves.fetch_add(1, Ordering::SeqCst);
            }
            saved
        }))
    }
}

enum Sent {
    Applied(u64),
    Rejected,
    Unsettled,
    Failed,
}

#[derive(Default)]
struct Oracle {
    effects: Vec<(TaskId, ClaimId, u64)>,
    sent: Vec<(String, Sent)>,
    wrong: Vec<String>,
    claims: BTreeMap<String, u64>,
    stats: BoardStats,
}

#[derive(Default)]
struct Watch {
    cursor: usize,
    consumed: Vec<(TaskId, String)>,
}

#[derive(Default)]
struct Firehose {
    acked: BTreeSet<String>,
}

struct World {
    shape: Shape,
    mutation: BoardMutation,
    clock: Arc<Clock>,
    host: Arc<FaultLog>,
    oracle: RefCell<Oracle>,
    watch: RefCell<Watch>,
    firehose: RefCell<Firehose>,
    posted: RefCell<bool>,
    cancelled: RefCell<bool>,
    minted: RefCell<BTreeMap<String, u64>>,
    reopens: RefCell<BTreeMap<TaskId, u64>>,
    chat: messages::Chat,
    watched: watching::Watched,
}

impl World {
    fn now_ms(&self) -> u64 {
        self.clock.now_ns() / MS
    }

    async fn nap(&self, ms: u64) {
        if !self.oracle.borrow().wrong.is_empty() {
            std::future::pending::<()>().await;
        }
        self.clock.sleep(Duration::from_nanos(ms * MS)).await;
    }

    async fn open(&self, writer: &str) -> Board {
        loop {
            let host = self
                .watched
                .wrap(Arc::clone(&self.host) as Arc<dyn LogHost>, &self.clock);
            match Board::open(host, SCOPE, access::identity(writer)).await {
                Ok(board) => {
                    self.resume(board.writer()).await;
                    return board;
                }
                Err(error) => {
                    self.note(&error);
                    self.nap(self.shape.poll_ms).await;
                }
            }
        }
    }

    async fn resume(&self, writer: &str) {
        let rows = self
            .host
            .inner
            .scan(SCOPE, 0, i64::MAX as u64)
            .await
            .unwrap_or_default();
        let prefix = format!("{writer}.");
        let landed = rows
            .iter()
            .filter_map(|(_, body)| serde_json::from_str::<nanocode_board::Event>(body).ok())
            .filter(|event| event.by == writer)
            .filter_map(|event| {
                event
                    .id
                    .0
                    .strip_prefix(&prefix)
                    .and_then(|count| count.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0);
        let mut oracle = self.oracle.borrow_mut();
        if landed > 0 {
            oracle.stats.continued += 1;
        }
        let mut lost = Vec::new();
        oracle.sent.retain(|(id, sent)| {
            let unlanded = id
                .strip_prefix(&prefix)
                .and_then(|count| count.parse::<u64>().ok())
                .is_some_and(|count| count > landed);
            if unlanded && let Sent::Applied(seq) = sent {
                lost.push(format!(
                    "{id} was applied at row {seq} but is not in the log when {writer} reopens"
                ));
            }
            !unlanded
        });
        oracle.wrong.extend(lost);
        drop(oracle);
        self.minted.borrow_mut().insert(writer.to_owned(), landed);
    }

    async fn submit(&self, board: &mut Board, op: Op) -> Result<Applied, BoardError> {
        let id = {
            let mut minted = self.minted.borrow_mut();
            let count = minted.entry(board.writer().to_owned()).or_default();
            *count += 1;
            format!("{}.{count}", board.writer())
        };
        let sent = board.submit(self.now_ms(), op).await;
        let mut oracle = self.oracle.borrow_mut();
        let outcome = match &sent {
            Ok(applied) if board.fold().outcome(&applied.event) != Some(&Ok(applied.seq)) => {
                oracle.wrong.push(format!(
                    "{id} settled at {} but the board's own fold does not hold it there",
                    applied.seq
                ));
                Sent::Applied(applied.seq)
            }
            Ok(applied) if applied.event.0 != id => {
                oracle
                    .wrong
                    .push(format!("{id} settled as {}", applied.event));
                Sent::Applied(applied.seq)
            }
            Ok(applied) => Sent::Applied(applied.seq),
            Err(BoardError::Rejected(_) | BoardError::Dropped(_) | BoardError::Row { .. }) => {
                Sent::Rejected
            }
            Err(BoardError::Unsettled {
                error: StoreError::Fenced,
                ..
            }) => {
                oracle.wrong.push(format!(
                    "{id} was fenced, which writes nothing, but the board reported it unsettled"
                ));
                Sent::Unsettled
            }
            Err(BoardError::Unsettled { .. } | BoardError::Corrupt { .. }) => Sent::Unsettled,
            Err(BoardError::Store(_)) => Sent::Failed,
        };
        oracle.sent.push((id, outcome));
        drop(oracle);
        sent
    }

    fn note(&self, error: &BoardError) {
        let mut oracle = self.oracle.borrow_mut();
        match error {
            BoardError::Corrupt { .. } => oracle.wrong.push(format!("{error}")),
            BoardError::Rejected(_) | BoardError::Dropped(_) | BoardError::Row { .. } => {
                oracle.stats.rejected += 1;
            }
            BoardError::Unsettled { .. } => oracle.stats.unsettled += 1,
            BoardError::Store(_) => {}
        }
    }

    const fn expected(&self) -> usize {
        self.shape.plan.len() + self.shape.prs.len() + self.chat.tasks
    }

    fn planned(&self, board: &Board) -> bool {
        *self.posted.borrow()
            && self.firehose.borrow().acked.len() == self.shape.hooks.len()
            && board.fold().tasks().len() == self.expected()
            && board.fold().settled()
    }
}

async fn lead(world: &World, epoch: u64) {
    access::setup(world).await;
    let writer = if world.shape.stable {
        format!("{}+lead", access::OWNER)
    } else {
        format!("{}+e{epoch}", access::OWNER)
    };
    let mut board = world.open(&writer).await;
    for task in &world.shape.plan {
        if task
            .parent
            .as_ref()
            .is_some_and(|parent| world.shape.tasks.batched.contains(parent))
        {
            continue;
        }
        loop {
            match world
                .submit(&mut board, Op::Post { task: task.clone() })
                .await
            {
                Ok(_) | Err(BoardError::Rejected(Rejection::Exists)) => break,
                Err(error @ BoardError::Rejected(_)) => {
                    world
                        .oracle
                        .borrow_mut()
                        .wrong
                        .push(format!("lead could not post {}: {error}", task.id));
                    break;
                }
                Err(error) => {
                    world.note(&error);
                    world.nap(world.shape.poll_ms).await;
                }
            }
        }
        if world.shape.cancel_early
            && world
                .shape
                .cancel
                .as_ref()
                .is_some_and(|(cancel, _)| *cancel == task.id)
        {
            replan(world, &mut board, &task.id).await;
        }
        if world.shape.tasks.batched.contains(&task.id) {
            tasks::post_batch(world, &mut board, &task.id).await;
        }
    }
    if world.shape.tasks.junk {
        tasks::junk(world, &mut board).await;
    }
    access::revoke(world).await;
    world.posted.replace(true);
    if let Some((task, after_ms)) = world.shape.cancel.clone()
        && !*world.cancelled.borrow()
    {
        world.nap(after_ms).await;
        replan(world, &mut board, &task).await;
    }
}

async fn replan(world: &World, board: &mut Board, task: &TaskId) {
    if *world.cancelled.borrow() {
        return;
    }
    loop {
        match world
            .submit(
                board,
                Op::Cancel {
                    task: task.clone(),
                    reason: "lead replanned".to_owned(),
                },
            )
            .await
        {
            Ok(_) | Err(BoardError::Rejected(_)) => break,
            Err(error) => {
                world.note(&error);
                world.nap(world.shape.poll_ms).await;
            }
        }
    }
    world.cancelled.replace(true);
}

#[derive(Clone, Debug)]
pub struct Hook {
    key: String,
    at_ms: u64,
    pr: u64,
    review: Option<u64>,
}

fn pr_task(pr: u64) -> TaskId {
    TaskId(format!("github/pr/{pr}"))
}

async fn firehose(world: &World, epoch: u64) {
    let mut board = world.open(access::SOURCE).await;
    let mut rng = Rng::derive(epoch, "board firehose");
    let mut queue = world
        .shape
        .hooks
        .iter()
        .filter(|hook| !world.firehose.borrow().acked.contains(&hook.key))
        .flat_map(|hook| {
            let copies = rng.range(1, 3);
            (0..copies).map(move |copy| (hook.clone(), copy))
        })
        .collect::<Vec<_>>();
    queue.sort_by_key(|(hook, copy)| (hook.at_ms + copy * world.shape.poll_ms, hook.key.clone()));
    let mut retry = Vec::new();
    while !queue.is_empty() || !retry.is_empty() {
        if queue.is_empty() {
            queue = std::mem::take(&mut retry);
            world.nap(world.shape.poll_ms).await;
        }
        let (hook, copy) = queue.remove(0);
        let wake = hook.at_ms + copy * world.shape.poll_ms;
        if world.now_ms() < wake {
            world.nap(wake - world.now_ms()).await;
        }
        let op = match hook.review {
            None => Op::Post {
                task: NewTask {
                    id: pr_task(hook.pr),
                    parent: None,
                    spec: format!("review pull request {}", hook.pr),
                    labels: BTreeMap::from([
                        ("source".to_owned(), "github".to_owned()),
                        ("step".to_owned(), "review".to_owned()),
                    ]),
                    needs: Vec::new(),
                    terms: nanocode_board::tasks::Terms::default(),
                },
            },
            Some(_) => Op::Reply {
                task: pr_task(hook.pr),
                body: hook.key.clone(),
            },
        };
        match board.submit_keyed(&hook.key, world.now_ms(), op).await {
            Ok(_) | Err(BoardError::Rejected(Rejection::Exists)) => {
                world.firehose.borrow_mut().acked.insert(hook.key.clone());
            }
            Err(BoardError::Rejected(Rejection::UnknownTask)) => retry.push((hook, copy)),
            Err(error) => {
                world.note(&error);
                retry.push((hook, copy));
            }
        }
    }
}

struct Worker<'a> {
    world: &'a World,
    board: Board,
    rng: Rng,
}

impl Worker<'_> {
    async fn submit(&mut self, op: Op) -> Option<u64> {
        match self.world.submit(&mut self.board, op).await {
            Ok(applied) => Some(applied.seq),
            Err(error) => {
                self.world.note(&error);
                None
            }
        }
    }

    fn until(&self, task: &TaskId, claim: &ClaimId) -> Option<u64> {
        match self.board.fold().task(task).map(|task| &task.phase) {
            Some(Phase::Claimed {
                claim: held,
                until_ms,
                ..
            }) if held == claim => Some(*until_ms),
            _ => None,
        }
    }

    async fn work(&mut self, task: TaskId, claim: ClaimId, mut until_ms: u64) {
        let world = self.world;
        let shape = &world.shape;
        for _ in 0..self.rng.range(1, 4) {
            if self.rng.chance(shape.stall_ppm) {
                world
                    .nap(shape.lease_ms + self.rng.range(1, shape.lease_ms))
                    .await;
            } else {
                world.nap(self.rng.range(1, shape.step_ms)).await;
            }
            if world.now_ms() < until_ms {
                world.oracle.borrow_mut().effects.push((
                    task.clone(),
                    claim.clone(),
                    world.now_ms(),
                ));
            }
            let renew = Op::Renew {
                task: task.clone(),
                claim: claim.clone(),
                lease_ms: shape.lease_ms,
            };
            if self.submit(renew).await.is_none() && self.board.refresh().await.is_err() {
                continue;
            }
            match self.until(&task, &claim) {
                Some(until) => until_ms = until,
                None => return,
            }
        }
        if self.rng.chance(shape.tasks.block_ppm) {
            let block = nanocode_board::tasks::TaskOp::Block {
                task,
                claim: Some(claim),
                kind: "awaiting_approval".to_owned(),
                reason: "needs a person to approve the change".to_owned(),
            };
            if self.submit(Op::Tasks(block)).await.is_some() {
                world.oracle.borrow_mut().stats.blocks += 1;
            }
            return;
        }
        if self.rng.chance(shape.release_ppm) {
            let _ = self
                .submit(Op::Release {
                    task,
                    claim,
                    note: "context full, handing off".to_owned(),
                })
                .await;
            return;
        }
        let failed = self.rng.chance(shape.fail_ppm)
            || (tasks::waits(self.board.fold(), &task)
                && self.rng.chance(shape.tasks.wait_fail_ppm));
        let outcome = if failed {
            Outcome::Failed(format!("{task} failed under {claim}"))
        } else {
            Outcome::Done(format!("{task} by {claim}"))
        };
        let _ = self
            .submit(Op::Finish {
                task,
                claim,
                outcome,
            })
            .await;
    }
}

async fn worker(world: &World, index: u64, epoch: u64) {
    let seat = format!("agent:w{index}+e{epoch}");
    let writer = if world.shape.stable {
        format!("agent:w{index}")
    } else {
        seat.clone()
    };
    let board = world.open(&writer).await;
    let mut worker = Worker {
        world,
        board,
        rng: Rng::derive(fnv(FNV_OFFSET, seat.as_bytes()), "board worker"),
    };
    loop {
        if let Err(error) = worker.board.refresh().await {
            world.note(&error);
            world.nap(world.shape.poll_ms).await;
            continue;
        }
        if world.planned(&worker.board) {
            return;
        }
        let now = world.now_ms();
        let fold = worker.board.fold();
        if let Some(id) = fold.tasks().iter().find_map(|(id, task)| {
            (matches!(task.stage(now), Stage::Lapsed)
                && matches!(fold.claimable(id, now), Err(Rejection::Held { .. })))
            .then(|| id.clone())
        }) {
            world.oracle.borrow_mut().wrong.push(format!(
                "{id} lapsed by {now} ms but the board still refuses its claim as held"
            ));
            return;
        }
        let mode = world.shape.tasks.modes[index as usize];
        let task = if mode == tasks::Mode::Random {
            let ready = worker
                .board
                .ready(world.now_ms())
                .into_iter()
                .filter(|task| {
                    worker
                        .board
                        .fold()
                        .admits(task, worker.board.writer())
                        .is_ok()
                })
                .collect::<Vec<_>>();
            (!ready.is_empty()).then(|| ready[worker.rng.pick(ready.len())].clone())
        } else {
            tasks::pick(world, &worker.board, &mut worker.rng)
        };
        let Some(task) = task else {
            world.nap(world.shape.poll_ms).await;
            continue;
        };
        if let Some(Phase::Claimed {
            claim: held,
            worker: holder,
            ..
        }) = worker.board.fold().task(&task).map(|task| &task.phase)
        {
            let mut oracle = world.oracle.borrow_mut();
            oracle.stats.takeovers += 1;
            let earlier = if world.shape.stable {
                oracle.claims.get(&held.0) != Some(&epoch)
            } else {
                !holder.ends_with(&format!("+e{epoch}"))
            };
            if earlier {
                oracle.stats.resumed += 1;
            }
        }
        if mode == tasks::Mode::Run {
            tasks::run(&mut worker, task).await;
            continue;
        }
        let Some(seq) = worker
            .submit(Op::Claim {
                task: task.clone(),
                lease_ms: world.shape.lease_ms,
                fence: None,
                cap: None,
            })
            .await
        else {
            continue;
        };
        world.oracle.borrow_mut().stats.claims += 1;
        let claim = worker
            .board
            .fold()
            .entries()
            .iter()
            .rev()
            .find(|entry| entry.seq == seq)
            .map(|entry| ClaimId(entry.event.0.clone()));
        if let Some(claim) = &claim {
            world
                .oracle
                .borrow_mut()
                .claims
                .insert(claim.0.clone(), epoch);
        }
        if let Some(claim) = claim
            && let Some(until) = worker.until(&task, &claim)
        {
            worker.work(task, claim, until).await;
        }
    }
}

async fn watcher(world: &World, epoch: u64) {
    let writer = if world.shape.stable {
        format!("{}+watch", access::OWNER)
    } else {
        format!("{}+watch-e{epoch}", access::OWNER)
    };
    let mut board = world.open(&writer).await;
    let mut rng = Rng::derive(epoch, "board watcher");
    loop {
        if let Err(error) = board.refresh().await {
            world.note(&error);
            world.nap(world.shape.poll_ms).await;
            continue;
        }
        let entries = board.fold().entries().to_vec();
        let mut cursor = world.watch.borrow().cursor;
        while let Some(entry) = entries.get(cursor) {
            if entry.change.terminal() {
                let item = (entry.task.clone(), format!("{:?}", entry.change));
                match world.mutation {
                    BoardMutation::CursorBeforeEffect => {
                        world.watch.borrow_mut().cursor = cursor + 1;
                        world.nap(rng.range(0, world.shape.lease_ms)).await;
                        world.watch.borrow_mut().consumed.push(item);
                    }
                    BoardMutation::EffectBeforeCursor => {
                        world.watch.borrow_mut().consumed.push(item);
                        world.nap(rng.range(0, world.shape.lease_ms)).await;
                        world.watch.borrow_mut().cursor = cursor + 1;
                    }
                    _ => {
                        {
                            let mut watch = world.watch.borrow_mut();
                            watch.consumed.push(item);
                            watch.cursor = cursor + 1;
                        }
                        world.nap(rng.range(0, world.shape.step_ms)).await;
                    }
                }
            } else {
                world.watch.borrow_mut().cursor = cursor + 1;
            }
            cursor += 1;
        }
        if world.planned(&board) {
            return;
        }
        world.nap(world.shape.poll_ms).await;
    }
}

async fn epoch(world: &World, number: u64) {
    let mut actors: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>>> = vec![
        Box::pin(lead(world, number)),
        Box::pin(firehose(world, number)),
        Box::pin(watcher(world, number)),
        Box::pin(access::intruder(world, number)),
        Box::pin(tasks::desk(world, number)),
        Box::pin(messages::chatter(world, number)),
    ];
    for index in 0..world.shape.workers {
        actors.push(Box::pin(worker(world, index, number)));
    }
    watching::actors(world, number, &mut actors);
    nanocode_sync::join_in_order(actors).await;
}

pub struct BoardOutcome {
    pub digest: u64,
    pub wrong: Option<String>,
    pub stats: BoardStats,
    pub shape: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Expect {
    Open,
    Claimed(String, u64),
    Done,
    Failed,
    Cancelled,
    Blocked,
}

impl Expect {
    const fn terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

struct Reference {
    tasks: BTreeMap<String, (Option<String>, Vec<String>, Expect)>,
    posts: BTreeMap<String, NewTask>,
    grants: BTreeMap<String, Vec<(u64, String)>>,
    finishes: BTreeMap<String, u64>,
    replies: BTreeMap<String, Vec<String>>,
    labels: BTreeMap<String, BTreeMap<String, String>>,
    ranks: BTreeMap<String, u64>,
    gate: access::Gate,
    verdicts: BTreeMap<String, bool>,
    assignees: BTreeMap<String, String>,
    waits: BTreeSet<String>,
    reopens: BTreeMap<String, u64>,
    roots: BTreeMap<String, (String, bool)>,
    schedules: BTreeMap<String, (String, Op, bool)>,
}

impl Reference {
    fn below(&self, id: &str) -> Vec<String> {
        self.tasks
            .iter()
            .filter(|(_, (parent, needs, _))| {
                parent.as_deref() == Some(id) || needs.iter().any(|need| need == id)
            })
            .map(|(below, _)| below.clone())
            .collect()
    }

    fn doom(&mut self, id: &str) {
        if !self.tasks.contains_key(id) || self.finished(id) {
            return;
        }
        self.tasks.get_mut(id).expect("present").2 = Expect::Cancelled;
        for below in self.below(id) {
            self.doom(&below);
        }
    }

    fn under(&self, id: &str, above: &str) -> bool {
        let mut at = Some(id.to_owned());
        for _ in 0..=self.tasks.len() {
            let Some(id) = at else {
                return false;
            };
            if id == above {
                return true;
            }
            at = self
                .tasks
                .get(&id)
                .and_then(|(parent, _, _)| parent.clone());
        }
        assert!(at.is_none(), "the model's parent links above {id} loop");
        false
    }

    fn holds(&self, task: &TaskId, claim: &ClaimId) -> bool {
        matches!(self.tasks.get(&task.0), Some((_, _, Expect::Claimed(held, _))) if *held == claim.0)
    }

    fn step(&mut self, by: &str, id: &str, now: u64, op: Op) -> bool {
        match op {
            Op::Post { task } => {
                let known = |id: &TaskId| self.tasks.contains_key(&id.0);
                if self.tasks.contains_key(&task.id.0) {
                    return self.posts.get(&task.id.0) == Some(&task);
                }
                if task.parent.as_ref().is_some_and(|parent| !known(parent))
                    || task.needs.iter().any(|need| !known(need))
                {
                    return false;
                }
                let doomed = task
                    .parent
                    .iter()
                    .chain(&task.needs)
                    .any(|above| self.dooms(&above.0));
                self.terms(&task);
                self.posts.insert(task.id.0.clone(), task.clone());
                self.tasks.insert(
                    task.id.0.clone(),
                    (
                        task.parent.map(|parent| parent.0),
                        task.needs.into_iter().map(|need| need.0).collect(),
                        Expect::Open,
                    ),
                );
                if doomed {
                    self.doom(&task.id.0);
                }
                true
            }
            Op::Claim { task, lease_ms, .. } => {
                let Some((_, needs, phase)) = self.tasks.get(&task.0) else {
                    return false;
                };
                let free = match phase {
                    Expect::Open => true,
                    Expect::Claimed(_, until) => *until <= now,
                    _ => false,
                };
                let met = needs.iter().all(|need| self.tasks[need].2 == Expect::Done);
                let admitted = self
                    .assignees
                    .get(&task.0)
                    .is_none_or(|to| nanocode_board::tasks::acts_for(by, to));
                if !(free && met && admitted) {
                    return false;
                }
                self.tasks.get_mut(&task.0).expect("present").2 =
                    Expect::Claimed(id.to_owned(), now + lease_ms);
                self.grants
                    .entry(task.0)
                    .or_default()
                    .push((now, id.to_owned()));
                true
            }
            Op::Renew {
                task,
                claim,
                lease_ms,
            } if self.holds(&task, &claim) => {
                self.tasks.get_mut(&task.0).expect("held").2 =
                    Expect::Claimed(claim.0, now + lease_ms);
                true
            }
            Op::Release { task, claim, .. } if self.holds(&task, &claim) => {
                self.tasks.get_mut(&task.0).expect("held").2 = Expect::Open;
                true
            }
            Op::Finish {
                task,
                claim,
                outcome,
            } if self.holds(&task, &claim) => {
                *self.finishes.entry(task.0.clone()).or_default() += 1;
                if let Outcome::Done(_) = outcome {
                    self.tasks.get_mut(&task.0).expect("held").2 = Expect::Done;
                } else {
                    self.tasks.get_mut(&task.0).expect("held").2 = Expect::Failed;
                    if !self.waits.contains(&task.0) {
                        for below in self.below(&task.0) {
                            self.doom(&below);
                        }
                    }
                }
                true
            }
            Op::Cancel { task, .. } => match self.tasks.get(&task.0) {
                Some(_) if !self.finished(&task.0) => {
                    self.doom(&task.0);
                    true
                }
                _ => false,
            },
            Op::Reply { task, body } if self.tasks.contains_key(&task.0) => {
                self.replies.entry(task.0).or_default().push(body);
                true
            }
            Op::Tasks(op) => self.steer(op, now),
            Op::Relabel { task, set, unset } if self.tasks.contains_key(&task.0) => {
                let labels = self.labels.entry(task.0).or_default();
                labels.retain(|key, _| !unset.contains(key));
                labels.extend(set);
                true
            }
            Op::Rank { task, rank } if self.tasks.contains_key(&task.0) => {
                match rank {
                    Some(rank) => self.ranks.insert(task.0, rank),
                    None => self.ranks.remove(&task.0),
                };
                true
            }
            Op::Move { task, parent } => {
                if !self.tasks.contains_key(&task.0) || self.finished(&task.0) {
                    return false;
                }
                if let Some(parent) = &parent
                    && (!self.tasks.contains_key(&parent.0)
                        || self.finished(&parent.0)
                        || self.under(&parent.0, &task.0))
                {
                    return false;
                }
                self.tasks.get_mut(&task.0).expect("present").0 = parent.map(|parent| parent.0);
                true
            }
            Op::Renew { .. }
            | Op::Release { .. }
            | Op::Finish { .. }
            | Op::Reply { .. }
            | Op::Relabel { .. }
            | Op::Rank { .. }
            | Op::Sync { .. } => false,
            Op::Grant { .. }
            | Op::Revoke { .. }
            | Op::Epoch { .. }
            | Op::Charter {}
            | Op::TextLimit { .. } => true,
            Op::Say { .. } => false,
            Op::Watch(_) | Op::Steer(_) => false,
        }
    }

    fn inner(&mut self, fold: &nanocode_board::Fold, id: String, by: &str, now: u64, op: Op) {
        let accepted =
            self.gate.allows(by, &op, &self.tasks) && self.step(by, &id, now, op.clone());
        if accepted {
            self.gate.record(by, &id, &op);
        }
        if accepted || fold.outcome(&EventId(id.clone())).is_some() {
            self.verdicts.insert(id, accepted);
        }
    }

    fn watch(&mut self, fold: &nanocode_board::Fold, id: &str, by: &str, now: u64, step: &Step) {
        match step {
            Step::Consume { effects, .. } => {
                for (index, op) in effects.iter().enumerate() {
                    self.inner(fold, format!("{id}/{index}"), by, now, op.clone());
                }
            }
            Step::Schedule {
                id: schedule,
                by: made,
                send,
                every_ms,
                ..
            } => {
                self.schedules.insert(
                    schedule.clone(),
                    (made.clone(), (**send).clone(), every_ms.is_some()),
                );
            }
            Step::Fire { id: schedule, n } => {
                let Some((made, send, repeating)) = self.schedules.get(schedule).cloned() else {
                    return;
                };
                let send = match send {
                    Op::Post { mut task } if repeating => {
                        task.id = TaskId(format!("{}/{n}", task.id));
                        Op::Post { task }
                    }
                    send => send,
                };
                self.inner(fold, format!("fire/{schedule}/{n}"), &made, now, send);
            }
            Step::Unschedule { id: schedule, .. } => {
                self.schedules.remove(schedule);
            }
            Step::Watch { .. }
            | Step::Unwatch { .. }
            | Step::Take { .. }
            | Step::Fail { .. }
            | Step::Bury { .. } => {}
        }
    }

    fn of(rows: &[(u64, String)]) -> Result<Self, String> {
        let mut model = Self {
            tasks: BTreeMap::new(),
            posts: BTreeMap::new(),
            grants: BTreeMap::new(),
            finishes: BTreeMap::new(),
            replies: BTreeMap::new(),
            labels: BTreeMap::new(),
            ranks: BTreeMap::new(),
            gate: access::Gate::default(),
            verdicts: BTreeMap::new(),
            assignees: BTreeMap::new(),
            waits: BTreeSet::new(),
            reopens: BTreeMap::new(),
            roots: BTreeMap::new(),
            schedules: BTreeMap::new(),
        };
        let mut fold = nanocode_board::Fold::new();
        for (seq, body) in rows {
            fold.apply(*seq, body)
                .map_err(|error| format!("reference refold: {error}"))?;
        }
        let mut now = 0;
        for (seq, body) in rows {
            let event: nanocode_board::Event = serde_json::from_str(body)
                .map_err(|error| format!("row {seq} is not an event: {error}"))?;
            if model.verdicts.contains_key(&event.id.0) || !access::owns(&event.by, &event.id.0) {
                continue;
            }
            now = now.max(event.at_ms);
            if let Op::Say { say } = &event.op {
                if !model.gate.says(&event.by, say, &model.tasks) {
                    model.verdicts.insert(event.id.0.clone(), false);
                    continue;
                }
                model.gate.heard(&event.id.0, say);
                if let Say::Delete { message } = say
                    && model.roots.get(&message.0) == Some(&(event.by.clone(), false))
                {
                    model
                        .roots
                        .insert(message.0.clone(), (event.by.clone(), true));
                    if !model.finished(&message.0) {
                        model.doom(&message.0);
                    }
                }
                continue;
            }
            if let Op::Watch(step) = &event.op {
                let accepted = fold.outcome(&event.id).is_some_and(Result::is_ok)
                    && model.gate.allows(&event.by, &event.op, &model.tasks);
                if accepted {
                    model.watch(&fold, &event.id.0, &event.by, now, step);
                }
                model.verdicts.insert(event.id.0, accepted);
                continue;
            }
            let accepted = model.gate.allows(&event.by, &event.op, &model.tasks)
                && model.step(&event.by, &event.id.0, now, event.op.clone());
            if accepted {
                model.gate.record(&event.by, &event.id.0, &event.op);
                if let Op::Post { task } = &event.op {
                    model
                        .roots
                        .entry(task.id.0.clone())
                        .or_insert((event.by.clone(), false));
                }
            }
            model.verdicts.insert(event.id.0, accepted);
        }
        Ok(model)
    }
}

fn indexes(rows: &[(u64, String)]) -> Result<(), String> {
    let mut fold = nanocode_board::Fold::new();
    let mut labels = BTreeSet::new();
    for (seq, body) in rows {
        fold.apply(*seq, body)
            .map_err(|error| format!("refold: {error}"))?;
        labels.extend(
            fold.tasks()
                .values()
                .flat_map(|task| task.spec.labels.clone()),
        );
        for (key, value) in &labels {
            let live = fold.labelled(key, value).collect::<Vec<_>>();
            let scan = fold
                .tasks()
                .iter()
                .filter(|(_, task)| {
                    !task.phase.terminal() && task.spec.labels.get(key) == Some(value)
                })
                .map(|(id, _)| id)
                .collect::<Vec<_>>();
            if live != scan {
                return Err(format!(
                    "after row {seq} the {key}={value} index holds {live:?}, a scan finds {scan:?}"
                ));
            }
        }
        for at in [fold.now_ms(), fold.now_ms() + 60_000] {
            let ready = fold.ready(at).collect::<Vec<_>>();
            let scan = fold
                .tasks()
                .keys()
                .filter(|id| fold.claimable(id, at).is_ok())
                .collect::<Vec<_>>();
            if ready != scan {
                return Err(format!(
                    "after row {seq} at {at} ms the ready index says {ready:?}, a scan says {scan:?}"
                ));
            }
            for (key, value) in &labels {
                let indexed = fold.ready_labelled(key, value, at).collect::<Vec<_>>();
                let scan = ready
                    .iter()
                    .copied()
                    .filter(|id| fold.tasks()[*id].spec.labels.get(key) == Some(value))
                    .collect::<Vec<_>>();
                if indexed != scan {
                    return Err(format!(
                        "after row {seq} the {key}={value} index says {indexed:?}, a scan says {scan:?}"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn resumes(rows: &[(u64, String)]) -> Result<(), String> {
    let fold = |rows: &[(u64, String)]| -> Result<nanocode_board::Fold, String> {
        let mut fold = nanocode_board::Fold::new();
        for (seq, body) in rows {
            fold.apply(*seq, body)
                .map_err(|error| format!("refold: {error}"))?;
        }
        Ok(fold)
    };
    let full = fold(rows)?;
    let log = Arc::new(MemoryLog::new());
    if !rows.is_empty() {
        nanocode_sync::block_on(log.append(SCOPE, rows))
            .map_err(|error| format!("copy the log: {error}"))?;
    }
    for cut in [0, rows.len() / 3, rows.len() / 2, rows.len()] {
        let held = fold(&rows[..cut])?;
        let held = if cut % 2 == 0 {
            let compact = nanocode_board::Fold::restore(&held.snapshot())
                .map_err(|error| format!("restore a compact snapshot of {cut} rows: {error}"))?;
            let json = nanocode_board::Fold::restore(held.snapshot_json().as_bytes())
                .map_err(|error| format!("restore a JSON snapshot of {cut} rows: {error}"))?;
            if compact != json {
                return Err(format!(
                    "compact and JSON snapshots of {cut} rows restore different folds"
                ));
            }
            compact
        } else {
            held
        };
        let host: Arc<dyn LogHost> = Arc::clone(&log) as Arc<dyn LogHost>;
        let board = nanocode_sync::block_on(Board::resume(
            host,
            SCOPE,
            access::identity("user:resumer"),
            held,
        ))
        .map_err(|error| format!("resume from a fold of {cut} rows: {error}"))?;
        if board.fold() != &full {
            return Err(format!(
                "a board resumed from a fold of {cut} rows differs from a full replay of {} rows",
                rows.len()
            ));
        }
    }
    Ok(())
}

/// A fold that evicts settled tasks must evict the same ones however it got its rows: straight, from
/// a compact or JSON snapshot at any cut, or through a resumed board. It holds no more entries than its
/// policy, tracks every terminal task it holds, and gives a writer's next event an id past every one
/// it evicted.
fn retains(seed: u64, rows: &[(u64, String)]) -> Result<u64, String> {
    use nanocode_board::{Fold, Retain};
    let fold = |mut fold: Fold, rows: &[(u64, String)]| -> Result<Fold, String> {
        for (seq, body) in rows {
            fold.apply(*seq, body)
                .map_err(|error| format!("retaining refold: {error}"))?;
        }
        Ok(fold)
    };
    let mut rng = Rng::derive(seed, "retention");
    let policy = Retain {
        settled: rng.below(4) as usize,
        entries: 1 + rng.below(rows.len() as u64 / 2 + 1) as usize,
        only: None,
        events: rng.below(4) as usize,
        gone: rng.below(3) as usize,
        label_events: None,
    };
    // Fold row by row, and at every cut restore the fold from its own compact snapshot: a restore
    // must give back exactly the fold that wrote it, retention bookkeeping included, or the restored
    // replica can evict differently later even when the end state happens to converge.
    let mut kept = Fold::retaining(policy.clone());
    for cut in 0..=rows.len() {
        let restored = Fold::restore(&kept.snapshot())
            .map_err(|error| format!("restore a retaining snapshot of {cut} rows: {error}"))?;
        if restored != kept {
            return Err(format!(
                "a retaining fold of {cut} rows restores from its snapshot as a different fold"
            ));
        }
        if let Some((seq, body)) = rows.get(cut) {
            kept.apply(*seq, body)
                .map_err(|error| format!("retaining refold: {error}"))?;
        }
    }
    if kept.entries().len() > policy.entries {
        return Err(format!(
            "a fold retaining {policy:?} holds {} entries",
            kept.entries().len()
        ));
    }
    if policy.events > 0 {
        for (id, task) in kept.tasks() {
            if task.replies.len() > policy.events {
                return Err(format!(
                    "a fold retaining {policy:?} holds {} replies on {}",
                    task.replies.len(),
                    id.0
                ));
            }
        }
    }
    let gone = kept.holding().gone;
    if policy.gone > 0 && gone > policy.gone + policy.entries {
        return Err(format!(
            "a fold retaining {policy:?} refuses a re-post of {gone} evicted tasks"
        ));
    }
    let terminal = kept
        .tasks()
        .values()
        .filter(|task| task.phase.terminal())
        .count();
    if kept.settled_count() != terminal {
        return Err(format!(
            "a fold retaining {policy:?} tracks {} settled tasks and holds {terminal}",
            kept.settled_count()
        ));
    }
    let full = fold(Fold::new(), rows)?;
    let writers = rows
        .iter()
        .filter_map(|(_, body)| serde_json::from_str::<nanocode_board::Event>(body).ok())
        .filter_map(|event| {
            event
                .id
                .0
                .rsplit_once('.')
                .map(|(writer, _)| writer.to_owned())
        })
        .collect::<BTreeSet<_>>();
    for writer in &writers {
        if kept.minted(writer) < full.minted(writer) {
            return Err(format!(
                "evicting set {writer}'s minted back from {} to {}",
                full.minted(writer),
                kept.minted(writer)
            ));
        }
    }
    let log = Arc::new(MemoryLog::new());
    if !rows.is_empty() {
        nanocode_sync::block_on(log.append(SCOPE, rows))
            .map_err(|error| format!("copy the log: {error}"))?;
    }
    for _ in 0..3 {
        let cut = rng.pick(rows.len() + 1);
        let held = fold(Fold::retaining(policy.clone()), &rows[..cut])?;
        let compact = Fold::restore(&held.snapshot())
            .map_err(|error| format!("restore a retaining snapshot of {cut} rows: {error}"))?;
        let json = Fold::restore(held.snapshot_json().as_bytes())
            .map_err(|error| format!("restore a retaining JSON snapshot of {cut} rows: {error}"))?;
        if compact != json {
            return Err(format!(
                "compact and JSON snapshots of a retaining fold of {cut} rows restore different folds"
            ));
        }
        if compact != held {
            return Err(format!(
                "a retaining fold restored at row {cut} of {} differs from the fold that wrote its snapshot",
                rows.len()
            ));
        }
        let (mut restored, mut straight) = (compact.clone(), held);
        for (index, (seq, body)) in rows[cut..].iter().enumerate() {
            restored
                .apply(*seq, body)
                .map_err(|error| format!("retaining refold: {error}"))?;
            straight
                .apply(*seq, body)
                .map_err(|error| format!("retaining refold: {error}"))?;
            if restored.evicted() != straight.evicted()
                || restored.tasks().keys().ne(straight.tasks().keys())
            {
                return Err(format!(
                    "a retaining fold restored at row {cut} of {} evicts differently at row {}",
                    rows.len(),
                    cut + index
                ));
            }
        }
        if restored != kept {
            return Err(format!(
                "a retaining fold restored at row {cut} of {} evicts differently from one that folded every row",
                rows.len()
            ));
        }
        let host: Arc<dyn LogHost> = Arc::clone(&log) as Arc<dyn LogHost>;
        let board = nanocode_sync::block_on(Board::resume(
            host,
            SCOPE,
            access::identity("user:resumer"),
            compact,
        ))
        .map_err(|error| format!("resume a retaining fold of {cut} rows: {error}"))?;
        if board.fold() != &kept {
            return Err(format!(
                "a board resumed from a retaining fold of {cut} rows evicts differently from a full replay"
            ));
        }
    }
    Ok(kept.evicted())
}

/// A board opened from a checkpoint at a random cut and the rows after it must fold what a full
/// replay folds; one opened over a checkpoint another fold's version wrote, or over a log that no
/// longer holds the row the checkpoint ends on, must replay the whole log instead.
fn reopens_from_checkpoints(seed: u64, rows: &[(u64, String)]) -> Result<(), String> {
    use nanocode_board::checkpoint::{self, Fallback, Opened};
    let fold = |rows: &[(u64, String)]| -> Result<nanocode_board::Fold, String> {
        let mut fold = nanocode_board::Fold::new();
        for (seq, body) in rows {
            fold.apply(*seq, body)
                .map_err(|error| format!("refold: {error}"))?;
        }
        Ok(fold)
    };
    let open = |rows: &[(u64, String)], saved: &StoredCheckpoint| {
        let log = Arc::new(MemoryLog::new());
        nanocode_sync::block_on(async {
            if !rows.is_empty() {
                log.append(SCOPE, rows).await?;
            }
            log.save_checkpoint(SCOPE, saved)
                .expect("a memory log keeps checkpoints")
                .await
        })
        .map_err(|error| format!("copy the log: {error}"))?;
        let host: Arc<dyn LogHost> = log;
        nanocode_sync::block_on(Board::open(host, SCOPE, access::identity("user:reopener")))
            .map_err(|error| format!("open over a checkpoint at {}: {error}", saved.seq))
    };
    if rows.is_empty() {
        return Ok(());
    }
    let full = fold(rows)?;
    let mut rng = Rng::derive(seed, "checkpoint cuts");
    for _ in 0..3 {
        let cut = 1 + rng.pick(rows.len());
        let held = fold(&rows[..cut])?;
        let saved = checkpoint::encode(&held, &rows[cut - 1].1);
        let board = open(rows, &saved)?;
        let tail = (rows.len() - cut) as u64;
        let expected = Opened::Checkpoint {
            seq: saved.seq,
            tail,
        };
        if board.opened() != &expected {
            return Err(format!(
                "a board over a checkpoint at row {cut} of {} opened as {:?}, not {expected:?}",
                rows.len(),
                board.opened()
            ));
        }
        if board.fold() != &full {
            return Err(format!(
                "a board opened from a checkpoint at row {cut} differs from a full replay of {} rows",
                rows.len()
            ));
        }
        // A fold another build wrote: the same snapshot under a version this build doesn't
        // write must not be trusted, whatever it holds.
        let stale = StoredCheckpoint {
            version: "board-fold/0/stale".to_owned(),
            ..saved.clone()
        };
        let board = open(rows, &stale)?;
        if !matches!(board.opened(), Opened::Replay(Fallback::Version(_))) {
            return Err(format!(
                "a board opened from a checkpoint at row {cut} another fold's version wrote ({:?})",
                board.opened()
            ));
        }
        if board.fold() != &full {
            return Err(format!(
                "a board over a stale checkpoint at row {cut} differs from a full replay"
            ));
        }
        // A log that lost the rows from the checkpoint's last one on, as a restore from an older
        // copy would: the checkpoint is ahead of the log and must not be used.
        let lost = &rows[..cut - 1];
        let board = open(lost, &saved)?;
        if board.fold() != &fold(lost)? {
            return Err(format!(
                "a board over {} rows opened from a checkpoint at row {cut} ({:?}) differs from a full replay",
                lost.len(),
                board.opened()
            ));
        }
    }
    Ok(())
}

/// Boards save checkpoints on a task of their own while commits go on, skip one while the last
/// still runs, lose one to a crash or a failed write, and wait longer as the fold grows. Whatever
/// that left in the log must still open as exactly the fold of the rows before its seq, with the
/// rest folded after it, and the boards must have encoded no more than twice the log each:
/// saves that came as often as the fewest events allowed would encode it quadratically.
/// A log the boards kept no checkpoint of, as with checkpoints off, must reopen by replaying
/// every row into exactly the fold of the log.
fn replays_without_checkpoint(rows: &[(u64, String)]) -> Result<(), String> {
    use nanocode_board::checkpoint::{Fallback, Opened};
    let log = Arc::new(MemoryLog::new());
    nanocode_sync::block_on(log.append(SCOPE, rows))
        .map_err(|error| format!("copy the log: {error}"))?;
    let reopened: Arc<dyn LogHost> = log;
    let board = nanocode_sync::block_on(Board::open(
        reopened,
        SCOPE,
        access::identity("user:reopener"),
    ))
    .map_err(|error| format!("open {} rows without a checkpoint: {error}", rows.len()))?;
    let expected = Opened::Replay(Fallback::Missing);
    if board.opened() != &expected {
        return Err(format!(
            "a board over {} rows and no checkpoint opened as {:?}, not {expected:?}",
            rows.len(),
            board.opened()
        ));
    }
    let mut full = nanocode_board::Fold::new();
    for (seq, body) in rows {
        full.apply(*seq, body)
            .map_err(|error| format!("refold: {error}"))?;
    }
    if board.fold() != &full {
        return Err(format!(
            "a board reopened without a checkpoint differs from a full replay of {} rows",
            rows.len()
        ));
    }
    Ok(())
}

fn kept_checkpoint(host: &FaultLog, rows: &[(u64, String)]) -> Result<u64, String> {
    use nanocode_board::checkpoint::Opened;
    let encoded = host.checkpoint_encoded.load(Ordering::SeqCst);
    let opens = host.checkpoint_reads.load(Ordering::SeqCst);
    let bound = 2 * rows.len() as u64 * opens;
    if encoded > bound {
        return Err(format!(
            "boards encoded {encoded} events of checkpoints over a log of {} rows after {opens} checkpoint reads, more than {bound}",
            rows.len()
        ));
    }
    let Some(saved) = nanocode_sync::block_on(
        host.inner
            .checkpoint(SCOPE)
            .expect("a memory log keeps checkpoints"),
    )
    .map_err(|error| format!("read the kept checkpoint: {error}"))?
    else {
        replays_without_checkpoint(rows)?;
        return Ok(0);
    };
    let log = Arc::new(MemoryLog::new());
    nanocode_sync::block_on(async {
        log.append(SCOPE, rows).await?;
        log.save_checkpoint(SCOPE, &saved)
            .expect("a memory log keeps checkpoints")
            .await
    })
    .map_err(|error| format!("copy the log: {error}"))?;
    let reopened: Arc<dyn LogHost> = log;
    let board = nanocode_sync::block_on(Board::open(
        reopened,
        SCOPE,
        access::identity("user:reopener"),
    ))
    .map_err(|error| format!("open over the kept checkpoint at {}: {error}", saved.seq))?;
    let expected = Opened::Checkpoint {
        seq: saved.seq,
        tail: (rows.len() as u64 + 1).saturating_sub(saved.seq),
    };
    if board.opened() != &expected {
        return Err(format!(
            "the checkpoint a board saved at {} over {} rows opened as {:?}, not {expected:?}",
            saved.seq,
            rows.len(),
            board.opened()
        ));
    }
    let mut full = nanocode_board::Fold::new();
    for (seq, body) in rows {
        full.apply(*seq, body)
            .map_err(|error| format!("refold: {error}"))?;
    }
    if board.fold() != &full {
        return Err(format!(
            "a board opened from the checkpoint saved at {} differs from a full replay of {} rows",
            saved.seq,
            rows.len()
        ));
    }
    Ok(1)
}

fn check(world: &World, rows: &[(u64, String)]) -> Result<BoardStats, String> {
    let mut fold = nanocode_board::Fold::new();
    for (seq, body) in rows {
        fold.apply(*seq, body)
            .map_err(|error| format!("refold: {error}"))?;
    }
    let oracle = world.oracle.borrow();
    if let Some(wrong) = oracle.wrong.first() {
        return Err(wrong.clone());
    }
    let oversize = world.host.oversize.load(Ordering::SeqCst);
    if oversize > 0 {
        return Err(format!(
            "{oversize} rows over the host's {}-byte row bound reached its append",
            messages::HOST_ROW_BYTES
        ));
    }
    let model = Reference::of(rows)?;
    access::audit(&fold, &model)?;
    messages::check(world, &fold, rows)?;
    let mut stats = oracle.stats;
    if fold.tasks().len() != world.expected() || model.tasks.len() != world.expected() {
        return Err(format!(
            "{} (reference {}) of {} expected tasks are on the board",
            fold.tasks().len(),
            model.tasks.len(),
            world.expected()
        ));
    }
    for (id, task) in fold.tasks() {
        let (_, _, expect) = &model.tasks[&id.0];
        let actual = match &task.phase {
            Phase::Open => Expect::Open,
            Phase::Claimed {
                claim, until_ms, ..
            } => Expect::Claimed(claim.0.clone(), *until_ms),
            Phase::Done { .. } => Expect::Done,
            Phase::Failed { .. } => Expect::Failed,
            Phase::Cancelled { .. } => Expect::Cancelled,
            Phase::Blocked { .. } => Expect::Blocked,
        };
        if actual != *expect {
            return Err(format!(
                "{id}: the board says {actual:?}, the reference model says {expect:?}"
            ));
        }
        if !actual.terminal() {
            return Err(format!("{id} never settled: {actual:?}"));
        }
        match actual {
            Expect::Done => stats.done += 1,
            Expect::Failed => stats.failed += 1,
            _ => stats.cancelled += 1,
        }
        let finishes = model.finishes.get(&id.0).copied().unwrap_or_default();
        if finishes > 1 + model.reopens.get(&id.0).copied().unwrap_or_default() {
            return Err(format!("{id} was finished {finishes} times"));
        }
    }
    for (task, claim, at_ms) in &oracle.effects {
        let grants = model
            .grants
            .get(&task.0)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let Some(mine) = grants.iter().position(|(_, held)| *held == claim.0) else {
            return Err(format!("{task}: {claim} acted without a granted claim"));
        };
        if let Some((granted, by)) = grants[mine + 1..]
            .iter()
            .find(|(granted, _)| granted < at_ms)
        {
            return Err(format!(
                "{task}: {claim} acted at {at_ms} ms, after {by} took the task over at {granted} ms"
            ));
        }
    }
    let mut landed = BTreeMap::new();
    let mut posts = BTreeMap::new();
    for (seq, body) in rows {
        let event: nanocode_board::Event = serde_json::from_str(body)
            .map_err(|error| format!("row {seq} is not an event: {error}"))?;
        if let Op::Post { task } = &event.op {
            posts.insert(*seq, task.clone());
        }
        landed.entry(event.id.0).or_insert(*seq);
    }
    for (id, sent) in &oracle.sent {
        match (sent, landed.get(id)) {
            (Sent::Applied(seq), Some(at)) if seq == at => {}
            (Sent::Applied(seq), Some(at))
                if seq < at
                    && posts
                        .get(seq)
                        .is_some_and(|task| posts.get(at) == Some(task)) => {}
            (Sent::Applied(seq), at) => {
                return Err(format!(
                    "{id} was applied at row {seq} but the log holds it at {at:?}"
                ));
            }
            (Sent::Failed, Some(at)) => {
                return Err(format!(
                    "{id} was reported as not written but landed at row {at}"
                ));
            }
            (Sent::Failed | Sent::Rejected | Sent::Unsettled, _) => {}
        }
    }
    for hook in &world.shape.hooks {
        if hook.review.is_none() {
            continue;
        }
        let task = pr_task(hook.pr);
        let copies = model.replies.get(&task.0).map_or(0, |replies| {
            replies.iter().filter(|body| **body == hook.key).count()
        });
        if copies != 1 {
            return Err(format!(
                "firehose event {} landed {copies} times on {task}",
                hook.key
            ));
        }
    }
    let terminal = fold
        .entries()
        .iter()
        .filter(|entry| entry.change.terminal())
        .map(|entry| (entry.task.clone(), format!("{:?}", entry.change)))
        .collect::<Vec<_>>();
    let watch = world.watch.borrow();
    if watch.consumed != terminal {
        let seen = watch.consumed.iter().collect::<BTreeSet<_>>();
        let missed = terminal.iter().filter(|item| !seen.contains(item)).count();
        return Err(format!(
            "the watcher consumed {} terminal changes for {} on the board ({missed} missed)",
            watch.consumed.len(),
            terminal.len()
        ));
    }
    watching::check(world, rows, &fold)?;
    stats.consumed = watch.consumed.len() as u64;
    stats.rows = rows.len() as u64;
    Ok(stats)
}

pub fn simulate(seed: u64, mutation: BoardMutation) -> BoardOutcome {
    let shape = Shape::of(seed);
    let watched = watching::Watched::new(seed);
    let described = format!("{} {}", shape.describe(), watched.shape.describe());
    let clock = Clock::new();
    let host = Arc::new(FaultLog {
        inner: MemoryLog::new(),
        rng: Slot::new(Some(Rng::derive(seed, "board faults"))),
        clock: Arc::clone(&clock),
        before_ppm: shape.before_ppm,
        after_ppm: shape.after_ppm,
        read_ppm: shape.read_ppm,
        latency_ms: shape.latency_ms,
        faults: AtomicU64::new(0),
        conflicts: AtomicU64::new(0),
        fenced: AtomicU64::new(0),
        checkpoint_reads: AtomicU64::new(0),
        checkpoint_saves: AtomicU64::new(0),
        checkpoint_encoded: AtomicU64::new(0),
        row_bytes_max: Some(messages::HOST_ROW_BYTES),
        oversize: AtomicU64::new(0),
    });
    crate::host::enter(seed, &clock);
    let world = World {
        mutation,
        clock: Arc::clone(&clock),
        host: Arc::clone(&host),
        oracle: RefCell::new(Oracle::default()),
        watch: RefCell::new(Watch::default()),
        firehose: RefCell::new(Firehose::default()),
        posted: RefCell::new(false),
        cancelled: RefCell::new(false),
        minted: RefCell::new(BTreeMap::new()),
        reopens: RefCell::new(BTreeMap::new()),
        chat: messages::Chat::of(seed),
        watched,
        shape,
    };
    board_sim::mutate(mutation.rule());
    // Checkpoints are off by default, so the sim turns them on: boards save them every few events
    // on most seeds and reopen from them after a crash. On some seeds they stay off, half of those
    // by a cadence of 0 and half by the default, so a reopen meets both, and a default board must
    // save none and reopen by replaying the log.
    let mut cadence = Rng::derive(seed, "checkpoint cadence");
    let mut default_cadence = false;
    nanocode_board::checkpoint::checkpoint_every_here(if cadence.chance(250_000) {
        default_cadence = cadence.chance(500_000);
        (!default_cadence).then_some(0)
    } else {
        Some(cadence.range(1, 33))
    });
    let mut crashes = world.shape.crashes.clone();
    crashes.sort_unstable();
    let mut number = 0;
    let mut wrong = None;
    loop {
        let crash_at = crashes.get(number as usize).map(|ms| ms * MS);
        let last = crash_at.is_none();
        let replies = world.shape.plan.len() as u64 * 2 + world.shape.hooks.len() as u64 * 2 + 32;
        let slack_ms = world.watched.shape.slack_ms(
            world.shape.step_ms,
            world.shape.poll_ms,
            world.shape.latency_ms,
            replies,
        );
        let deadline = clock.now_ns() + (world.shape.deadline_ms + slack_ms) * MS;
        let mut executor = Executor::new(
            Arc::clone(&clock),
            Rng::derive(seed, &format!("board scheduler {number}")),
        );
        let task: Task<'_> = Box::pin(epoch(&world, number));
        let ended = executor.run(
            vec![task],
            Some(crash_at.map_or(deadline, |at| at.max(clock.now_ns()))),
        );
        drop(executor);
        match ended {
            Ended::Finished if last => break,
            Ended::Finished => {}
            Ended::Crashed if last => {
                let rows =
                    nanocode_sync::block_on(world.host.inner.scan(SCOPE, 0, i64::MAX as u64))
                        .unwrap_or_default();
                let mut fold = nanocode_board::Fold::new();
                for (seq, body) in &rows {
                    let _ = fold.apply(*seq, body);
                }
                let open = fold
                    .tasks()
                    .iter()
                    .filter(|(_, task)| !task.finished())
                    .map(|(id, task)| {
                        format!(
                            "{id}={:?} to={:?} needs={:?}",
                            task.phase, task.assignee, task.spec.needs
                        )
                    })
                    .take(4)
                    .collect::<Vec<_>>();
                wrong = Some(format!(
                    "the board did not settle within {} ms of the last restart; {} rows, {}/{} tasks posted, open: {open:?}; {}",
                    world.shape.deadline_ms,
                    rows.len(),
                    fold.tasks().len(),
                    world.shape.plan.len(),
                    watching::stalled(&world, &fold)
                ));
                break;
            }
            Ended::Crashed => world.oracle.borrow_mut().stats.crashes += 1,
            Ended::Stuck => {
                if world.oracle.borrow().wrong.is_empty() {
                    wrong = Some(format!("epoch {number} got stuck"));
                }
                break;
            }
        }
        if crash_at.is_some() {
            world.posted.replace(false);
        }
        number += 1;
    }
    let live = access::live(&world);
    let rows = nanocode_sync::block_on(world.host.inner.scan(SCOPE, 0, i64::MAX as u64))
        .unwrap_or_default();
    let indexed = indexes(&rows);
    let resumed = resumes(&rows);
    let checkpointed = reopens_from_checkpoints(seed, &rows);
    let retained = retains(seed, &rows);
    let evicted = retained.as_ref().copied().unwrap_or_default();
    let retained = retained.map(|_| ());
    let kept = kept_checkpoint(&world.host, &rows);
    let checkpoint_opens = kept.as_ref().copied().unwrap_or_default();
    let kept = kept.map(|_| ());
    let saves = world.host.checkpoint_saves.load(Ordering::SeqCst);
    let default_off = if default_cadence && saves > 0 {
        Err(format!(
            "boards on the default cadence saved {saves} checkpoints; the default is off"
        ))
    } else {
        Ok(())
    };
    let due = due::run(seed);
    let intended = intent::run(seed).map(|_| ());
    let delivered = deliver::run(seed);
    board_sim::mutate(BoardRule::None);
    nanocode_board::checkpoint::checkpoint_every_here(None);
    for wrong in [
        live,
        indexed,
        resumed,
        checkpointed,
        retained,
        kept,
        default_off,
        intended,
    ]
    .into_iter()
    .filter_map(Result::err)
    {
        world.oracle.borrow_mut().wrong.push(wrong);
    }
    let mut digest = fnv(FNV_OFFSET, described.as_bytes());
    for (seq, body) in &rows {
        digest = fnv(digest, &seq.to_le_bytes());
        digest = fnv(digest, body.as_bytes());
    }
    let verdict = world
        .watched
        .broken()
        .or(wrong)
        .map_or_else(|| check(&world, &rows), Err)
        .and_then(|mut stats| {
            let due = due?;
            let delivered = delivered?;
            stats.deliveries = delivered.posted;
            stats.redeliveries = delivered.again;
            stats.delivery_takeovers = delivered.takeovers;
            stats.due_runs = due.runs;
            stats.due_retries = due.retries;
            stats.due_restarts = due.restarts;
            Ok(stats)
        });
    let mut stats = verdict
        .as_ref()
        .copied()
        .unwrap_or_else(|_| world.oracle.borrow().stats);
    stats.seeds = 1;
    stats.stable = u64::from(world.shape.stable);
    stats.tasks = world.expected() as u64;
    stats.workers = world.shape.workers;
    stats.faults = host.faults.load(Ordering::SeqCst);
    stats.conflicts = host.conflicts.load(Ordering::SeqCst);
    stats.checkpoints = host.checkpoint_saves.load(Ordering::SeqCst);
    stats.checkpoint_opens = checkpoint_opens;
    stats.evicted = evicted;
    BoardOutcome {
        digest,
        wrong: verdict.err(),
        stats,
        shape: described,
    }
}

pub fn run_seed(seed: u64, mutation: BoardMutation) -> BoardOutcome {
    std::panic::catch_unwind(|| simulate(seed, mutation)).unwrap_or_else(|panic| {
        board_sim::mutate(BoardRule::None);
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default();
        BoardOutcome {
            digest: 0,
            wrong: Some(format!("panic: {message}")),
            stats: BoardStats::default(),
            shape: String::new(),
        }
    })
}

pub fn judge(
    seed: u64,
    mutation: BoardMutation,
    twice: bool,
    verbose: bool,
    totals: &mut BoardStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!(
            "seed {seed} digest {:016x} {} stats={:?}",
            first.digest, first.shape, first.stats
        );
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

pub fn replay(seed: u64, mutation: BoardMutation) -> String {
    let mut command = format!("nanocode-sim --scenario board --seed {seed}");
    if mutation != BoardMutation::None {
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
    mutation: BoardMutation,
    totals: &BoardStats,
) -> ExitCode {
    println!("{}", watching::summary());
    println!(
        "RESULT seeds={}..{} scenario=board mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        BoardMutation::None if failed == 0 => ExitCode::SUCCESS,
        BoardMutation::None => ExitCode::FAILURE,
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
