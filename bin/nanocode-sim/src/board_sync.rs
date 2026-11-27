//! Two-way board sync under faults. Per seed: one board scope on a faulty log,
//! fake Slack, GitHub and Linear platforms that refuse or lose calls and outlive
//! crashes, a lead that posts and answers on the board, an outside actor that
//! replays platform webhooks (redelivered, duplicated, reordered, dropped), an
//! echo actor that feeds the bot's own posts back, and one drainer per epoch.
//! A reference built only from the log rows and the platforms' final state
//! judges imports, deliveries, order, edits, read state, authors, modes, direct
//! messages, labels and dead letters.

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
    Board, BoardError, Change, Event as Row, EventId, Fold, NewTask, Op, Rejection, TaskId,
    access::{Can, Grant, Who},
    mirror::Synced as Note,
    sim::{self as board_sim, Mutation as BoardRule},
    tasks::Terms,
};
use nanocode_board_sync::{
    Accounts, Action, Author, Channels, Control, Delivery, Engine, Event, Inbound, Join, Kind,
    Mode, Plain, Platform, Rendered, Renderer, Route, Router, Settle, SyncOp, Target,
    fake::{self, Echo, Fake, Fault},
    sim::{self as sync_sim, Mutation as SyncRule},
    slack,
};
use nanocode_sync::Slot;
use nanocode_turnlog::{LogHost, MemoryLog};

use crate::{
    board::{FaultLog, identity},
    executor::{Clock, Ended, Executor, Task},
    rng::{FNV_OFFSET, Rng, fnv},
};

const SCOPE: &str = "sync";
const OWNER: &str = "user:lead";
const LEAD: &str = "user:lead+lead";
const SYNC: &str = "agent:sync";
const ATTEMPTS: u64 = 5;
/// Share of 429 storms that open with a burst: each platform answers its first few posts, edits
/// and deletes with 429s while letting lookups through, so the delivery at the head of its queue
/// is throttled again and again. Only such a run of throttled attempts can spend a delivery's
/// attempts when throttles are counted as failures.
const BURST_PPM: u64 = 600_000;
const MS: u64 = 1_000_000;
const BOT: &str = "agent";
const MEMBERS: [&str; 4] = ["ann", "bob", "cat", "dan"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMutation {
    None,
    DeliverToOrigin,
    ReceiptsIgnored,
    DeletesNotSynced,
    ParkedDropped,
    ImportsDuplicated,
    EchoImported,
    EditsUnordered,
    StaleLabels,
    IdentityIgnored,
    MentionsNotDm,
    DeadLettersBlock,
    StatusPostsAll,
    FollowIgnored,
    OrderIgnored,
    UnlinkIgnored,
    BotMarksRead,
    ReadRegresses,
    RetryGuardSkipped,
    AttemptAfterCall,
    RootEditsIgnored,
    RetryAfterIgnored,
    RateLimitCountsAttempt,
    RootDeadIgnored,
    SplitDropsBreak,
    SplitIgnoresMessageLimit,
}

impl SyncMutation {
    const ALL: [Self; 26] = [
        Self::None,
        Self::DeliverToOrigin,
        Self::ReceiptsIgnored,
        Self::DeletesNotSynced,
        Self::ParkedDropped,
        Self::ImportsDuplicated,
        Self::EchoImported,
        Self::EditsUnordered,
        Self::StaleLabels,
        Self::IdentityIgnored,
        Self::MentionsNotDm,
        Self::DeadLettersBlock,
        Self::StatusPostsAll,
        Self::FollowIgnored,
        Self::OrderIgnored,
        Self::UnlinkIgnored,
        Self::BotMarksRead,
        Self::ReadRegresses,
        Self::RetryGuardSkipped,
        Self::AttemptAfterCall,
        Self::RootEditsIgnored,
        Self::RetryAfterIgnored,
        Self::RateLimitCountsAttempt,
        Self::RootDeadIgnored,
        Self::SplitDropsBreak,
        Self::SplitIgnoresMessageLimit,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::DeliverToOrigin => "deliver_to_origin",
            Self::ReceiptsIgnored => "receipts_ignored",
            Self::DeletesNotSynced => "deletes_not_synced",
            Self::ParkedDropped => "parked_dropped",
            Self::ImportsDuplicated => "imports_duplicated",
            Self::EchoImported => "echo_imported",
            Self::EditsUnordered => "edits_unordered",
            Self::StaleLabels => "stale_labels",
            Self::IdentityIgnored => "identity_ignored",
            Self::MentionsNotDm => "mentions_not_dm",
            Self::DeadLettersBlock => "dead_letters_block",
            Self::StatusPostsAll => "status_posts_all",
            Self::FollowIgnored => "follow_ignored",
            Self::OrderIgnored => "order_ignored",
            Self::UnlinkIgnored => "unlink_ignored",
            Self::BotMarksRead => "bot_marks_read",
            Self::ReadRegresses => "read_regresses",
            Self::RetryGuardSkipped => "retry_guard_skipped",
            Self::AttemptAfterCall => "attempt_after_call",
            Self::RootEditsIgnored => "root_edits_ignored",
            Self::RetryAfterIgnored => "retry_after_ignored",
            Self::RateLimitCountsAttempt => "rate_limit_counts_attempt",
            Self::RootDeadIgnored => "root_dead_ignored",
            Self::SplitDropsBreak => "split_drops_break",
            Self::SplitIgnoresMessageLimit => "split_ignores_message_limit",
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

    const fn board(self) -> BoardRule {
        match self {
            Self::DeliverToOrigin => BoardRule::DeliverToOrigin,
            Self::ReceiptsIgnored => BoardRule::ReceiptsIgnored,
            Self::DeletesNotSynced => BoardRule::DeletesNotSynced,
            Self::ParkedDropped => BoardRule::ParkedDropped,
            Self::ImportsDuplicated => BoardRule::ImportsDuplicated,
            Self::EchoImported => BoardRule::EchoImported,
            Self::EditsUnordered => BoardRule::EditsUnordered,
            Self::StaleLabels => BoardRule::StaleLabels,
            Self::IdentityIgnored => BoardRule::IdentityIgnored,
            Self::MentionsNotDm => BoardRule::MentionsNotDm,
            Self::DeadLettersBlock => BoardRule::DeadLettersBlock,
            Self::StatusPostsAll => BoardRule::StatusPostsAll,
            Self::FollowIgnored => BoardRule::FollowIgnored,
            Self::OrderIgnored => BoardRule::OrderIgnored,
            Self::UnlinkIgnored => BoardRule::UnlinkIgnored,
            Self::BotMarksRead => BoardRule::BotMarksRead,
            Self::ReadRegresses => BoardRule::ReadRegresses,
            Self::RootEditsIgnored => BoardRule::RootEditsIgnored,
            Self::RootDeadIgnored => BoardRule::RootDeadIgnored,
            Self::None
            | Self::RetryGuardSkipped
            | Self::AttemptAfterCall
            | Self::RetryAfterIgnored
            | Self::RateLimitCountsAttempt
            | Self::SplitDropsBreak
            | Self::SplitIgnoresMessageLimit => BoardRule::None,
        }
    }

    const fn engine(self) -> SyncRule {
        match self {
            Self::RetryGuardSkipped => SyncRule::RetryGuardSkipped,
            Self::AttemptAfterCall => SyncRule::AttemptAfterCall,
            Self::RetryAfterIgnored => SyncRule::RetryAfterIgnored,
            Self::RateLimitCountsAttempt => SyncRule::RateLimitCountsAttempt,
            Self::SplitDropsBreak => SyncRule::SplitDropsBreak,
            Self::SplitIgnoresMessageLimit => SyncRule::SplitIgnoresMessageLimit,
            _ => SyncRule::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SyncStats {
    pub seeds: u64,
    pub rows: u64,
    pub tasks: u64,
    pub links: u64,
    pub imports: u64,
    pub parked: u64,
    pub copies: u64,
    pub dms: u64,
    pub edits: u64,
    pub deleted: u64,
    pub follows: u64,
    pub dead: u64,
    pub reads: u64,
    pub abandoned: u64,
    pub unsettled: u64,
    pub faults: u64,
    pub conflicts: u64,
    pub calls: u64,
    pub crashes: u64,
    pub limited: u64,
}

impl SyncStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.rows += other.rows;
        self.tasks += other.tasks;
        self.links += other.links;
        self.imports += other.imports;
        self.parked += other.parked;
        self.copies += other.copies;
        self.dms += other.dms;
        self.edits += other.edits;
        self.deleted += other.deleted;
        self.follows += other.follows;
        self.dead += other.dead;
        self.reads += other.reads;
        self.abandoned += other.abandoned;
        self.unsettled += other.unsettled;
        self.faults += other.faults;
        self.conflicts += other.conflicts;
        self.calls += other.calls;
        self.crashes += other.crashes;
        self.limited += other.limited;
    }
}

#[derive(Clone, Debug)]
enum Step {
    Accounts,
    Post(NewTask),
    Answer {
        key: String,
        task: TaskId,
        body: String,
        controls: Vec<Control>,
    },
    Revise {
        key: String,
        task: TaskId,
        message: String,
        body: String,
    },
    Retract {
        key: String,
        task: TaskId,
        message: String,
    },
    Link {
        key: String,
        task: TaskId,
        target: Target,
        mode: Mode,
    },
    Unlink {
        key: String,
        task: TaskId,
        target: Target,
    },
    Follow {
        key: String,
        task: TaskId,
        target: Target,
    },
    Cancel {
        key: String,
        task: TaskId,
    },
}

#[derive(Clone, Debug)]
struct Planned {
    step: Step,
    nap_ms: u64,
    after: Option<String>,
}

#[derive(Clone, Debug)]
enum Place {
    Fixed(Target),
    Root(TaskId, Target),
    Copy(String, String),
}

#[derive(Clone, Debug)]
enum What {
    Opened {
        task: NewTask,
        message: String,
        mode: Mode,
    },
    Message {
        message: String,
        author: Author,
        body: String,
    },
    Edited {
        message: String,
        body: String,
        version: u64,
    },
    Deleted {
        message: String,
    },
    Reworded {
        author: Author,
        title: String,
        body: Option<String>,
        version: u64,
    },
    Labels {
        task: TaskId,
        set: BTreeMap<String, String>,
        version: u64,
    },
    Closed {
        task: TaskId,
    },
    React {
        author: Author,
    },
    Click {
        author: Author,
        control: String,
        values: BTreeMap<String, String>,
    },
}

#[derive(Clone, Debug)]
struct Hook {
    key: String,
    at_ms: u64,
    copies: u64,
    place: Place,
    what: What,
    after: Option<String>,
}

struct Shape {
    poll_ms: u64,
    latency_ms: u64,
    before_ppm: u64,
    after_ppm: u64,
    read_ppm: u64,
    refuse_ppm: u64,
    lose_ppm: u64,
    limit_ppm: u64,
    retry_ms: u64,
    storm: bool,
    /// How many posts, edits and deletes each platform throttles before its faults run as drawn.
    burst: u64,
    channels: BTreeMap<Target, Mode>,
    identity: BTreeMap<(String, String), String>,
    lead: Vec<Planned>,
    hooks: Vec<Hook>,
    poison: Option<String>,
    crashes: Vec<u64>,
    patience_ms: u64,
    deadline_ms: u64,
}

fn users(sync: &str) -> &'static [&'static str] {
    match sync {
        "slack" => &["U-ann", "U-bob", "U-eve"],
        "github" => &["gh-ann", "gh-cat", "gh-zed"],
        _ => &["lin-dan", "lin-yan"],
    }
}

fn bot_user(sync: &str) -> &'static str {
    match sync {
        "slack" => "U-bot",
        "github" => "gh-bot",
        _ => "lin-bot",
    }
}

fn human(user: &str) -> Author {
    let name = user.rsplit('-').next().unwrap_or(user);
    Author {
        user: user.to_owned(),
        name: name.to_owned(),
        bot: false,
    }
}

fn bot(sync: &str) -> Author {
    Author {
        user: bot_user(sync).to_owned(),
        name: BOT.to_owned(),
        bot: true,
    }
}

fn channel_key(task: &str, target: &Target) -> Target {
    Target::new(&target.sync, format!("{}#{task}", target.id))
}

fn keyed(id: &EventId, key: &str) -> bool {
    id.0.split_once('#').map_or(id.0.as_str(), |(_, rest)| rest) == key
}

fn root_id(task: &TaskId) -> String {
    format!("post/{task}/post")
}

fn named(body: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for word in body.split_whitespace() {
        if let Some(name) = word.strip_prefix('@')
            && !name.is_empty()
            && !found.iter().any(|seen| seen == name)
        {
            found.push(name.to_owned());
        }
    }
    found
}

fn mode(rng: &mut Rng, status_ppm: u64) -> Mode {
    if rng.chance(status_ppm) {
        Mode::Status
    } else {
        Mode::Always
    }
}

fn merge(rng: &mut Rng, mut lanes: Vec<Vec<Planned>>) -> Vec<Planned> {
    let mut out = Vec::new();
    lanes.retain(|lane| !lane.is_empty());
    while !lanes.is_empty() {
        let pick = rng.pick(lanes.len());
        out.push(lanes[pick].remove(0));
        if lanes[pick].is_empty() {
            lanes.remove(pick);
        }
    }
    out
}

struct Plan<'a> {
    rng: &'a mut Rng,
    hooks: Vec<Hook>,
    texts: u64,
    work_ms: u64,
    poll_ms: u64,
}

impl Plan<'_> {
    fn at(&mut self) -> u64 {
        self.rng.range(0, self.work_ms)
    }

    fn hook(&mut self, key: String, at_ms: u64, place: Place, what: What, after: Option<String>) {
        let copies = self.rng.range(1, 3);
        self.hooks.push(Hook {
            key,
            at_ms,
            copies,
            place,
            what,
            after,
        });
    }

    fn talk(&mut self, place: &Place, sync: &str, after: Option<&String>, follow: bool) {
        self.texts += 1;
        let ext = format!("h{}", self.texts);
        let pool = users(sync);
        let author = human(pool[self.rng.pick(pool.len())]);
        let mut body = if follow {
            "follow".to_owned()
        } else {
            format!("{ext} from {}", author.name)
        };
        if !follow && self.rng.chance(300_000) {
            body.push_str(&format!(" @{}", MEMBERS[self.rng.pick(MEMBERS.len())]));
        }
        let at_ms = self.at();
        let message = What::Message {
            message: ext.clone(),
            author,
            body,
        };
        self.hook(
            format!("{sync}/{ext}"),
            at_ms,
            place.clone(),
            message.clone(),
            after.cloned(),
        );
        if self.rng.chance(250_000) {
            let later = at_ms
                + self
                    .rng
                    .range(self.poll_ms, self.work_ms / 2 + self.poll_ms);
            self.hook(
                format!("{sync}/{ext}/again"),
                later,
                place.clone(),
                message,
                after.cloned(),
            );
        }
        if !follow && self.rng.chance(350_000) {
            let edits = self.rng.range(1, 3);
            for version in 1..=edits {
                let body = format!("{ext} edit {version}");
                let when = at_ms + self.rng.range(0, self.work_ms / 2);
                let edit = What::Edited {
                    message: ext.clone(),
                    body,
                    version,
                };
                self.hook(
                    format!("{sync}/{ext}/edit{version}"),
                    when,
                    place.clone(),
                    edit,
                    after.cloned(),
                );
            }
        }
        if !follow && self.rng.chance(150_000) {
            let when = at_ms + self.rng.range(0, self.work_ms / 2);
            self.hook(
                format!("{sync}/{ext}/delete"),
                when,
                place.clone(),
                What::Deleted { message: ext },
                after.cloned(),
            );
        }
    }
}

impl Shape {
    #[allow(clippy::too_many_lines)]
    fn of(seed: u64) -> Self {
        let mut rng = Rng::derive(seed, "board sync shape");
        let poll_ms = rng.range(1, 6);
        let step_ms = rng.range(1, 10);
        let latency_ms = rng.range(0, 3);
        let faulty = !rng.chance(150_000);
        let ppm = |rng: &mut Rng, high: u64| if faulty { rng.range(0, high) } else { 0 };
        let before_ppm = ppm(&mut rng, 60_000);
        let after_ppm = ppm(&mut rng, 60_000);
        let read_ppm = ppm(&mut rng, 30_000);
        let refuse_ppm = ppm(&mut rng, 200_000);
        let lose_ppm = ppm(&mut rng, 200_000);
        let mut channels = BTreeMap::from([(Target::new("slack", "C0"), mode(&mut rng, 400_000))]);
        if rng.chance(400_000) {
            channels.insert(Target::new("slack", "C1"), mode(&mut rng, 400_000));
        }
        let mut identity = BTreeMap::new();
        let mut account = |sync: &str, user: &str, member: &str| {
            identity.insert((sync.to_owned(), user.to_owned()), member.to_owned());
        };
        account("slack", "U-ann", "ann");
        for (sync, user, member) in [
            ("slack", "U-bob", "bob"),
            ("github", "gh-ann", "ann"),
            ("github", "gh-cat", "cat"),
            ("linear", "lin-dan", "dan"),
        ] {
            if rng.chance(600_000) {
                account(sync, user, member);
            }
        }
        for sync in ["github", "linear", "slack"] {
            account(sync, bot_user(sync), BOT);
        }
        let mut lanes = Vec::new();
        let mut answers = Vec::new();
        let mut controlled = Vec::new();
        let mut threads = Vec::new();
        let nap = |rng: &mut Rng| rng.range(0, step_ms);
        let lead_tasks = rng.range(2, 4);
        for index in 0..lead_tasks {
            let id = TaskId(format!("lead/{index}"));
            let quiet = rng.chance(250_000);
            let mut labels = BTreeMap::from([("kind".to_owned(), "lead".to_owned())]);
            if quiet {
                labels.insert("quiet".to_owned(), "yes".to_owned());
            }
            let task = NewTask {
                id: id.clone(),
                parent: None,
                spec: format!("plan {index}"),
                labels,
                needs: Vec::new(),
                terms: Terms::default(),
            };
            let mut lane = vec![Planned {
                step: Step::Post(task),
                nap_ms: nap(&mut rng),
                after: None,
            }];
            for number in 0..rng.range(1, 4) {
                let key = format!("ans/{id}/{number}");
                let mut body = format!("{id} note {number}");
                if rng.chance(500_000) {
                    body.push_str(&format!(" @{}", MEMBERS[rng.pick(MEMBERS.len())]));
                }
                let controls = match rng.below(10) {
                    0..=1 => vec![Control::Button {
                        id: "ok".to_owned(),
                        label: "Approve".to_owned(),
                    }],
                    2 => vec![Control::Form {
                        id: "why".to_owned(),
                        label: "Explain".to_owned(),
                        fields: vec!["reason".to_owned()],
                    }],
                    _ => Vec::new(),
                };
                if !controls.is_empty() {
                    controlled.push((key.clone(), controls[0].clone(), quiet));
                }
                answers.push((key.clone(), quiet));
                let step = Step::Answer {
                    key: key.clone(),
                    task: id.clone(),
                    body,
                    controls,
                };
                lane.push(Planned {
                    step,
                    nap_ms: nap(&mut rng),
                    after: None,
                });
                if rng.chance(250_000) {
                    let mut body = format!("{id} note {number} revised");
                    if rng.chance(300_000) {
                        body.push_str(&format!(" @{}", MEMBERS[rng.pick(MEMBERS.len())]));
                    }
                    let step = Step::Revise {
                        key: format!("rev/{key}"),
                        task: id.clone(),
                        message: key.clone(),
                        body,
                    };
                    lane.push(Planned {
                        step,
                        nap_ms: nap(&mut rng),
                        after: None,
                    });
                }
                if rng.chance(150_000) {
                    let step = Step::Retract {
                        key: format!("drop/{key}"),
                        task: id.clone(),
                        message: key,
                    };
                    lane.push(Planned {
                        step,
                        nap_ms: nap(&mut rng),
                        after: None,
                    });
                }
            }
            if !quiet {
                for (channel, mode) in &channels {
                    if *mode == Mode::Status && rng.chance(400_000) {
                        let at = rng.range(1, lane.len() as u64) as usize;
                        let step = Step::Follow {
                            key: format!("follow/{id}/{channel}"),
                            task: id.clone(),
                            target: channel.clone(),
                        };
                        lane.insert(
                            at,
                            Planned {
                                step,
                                nap_ms: nap(&mut rng),
                                after: None,
                            },
                        );
                    }
                }
            }
            if rng.chance(450_000) {
                let target = if rng.chance(500_000) {
                    Target::new("linear", format!("ISS-{index}"))
                } else {
                    Target::new("github", format!("issue/{index}"))
                };
                let mode = mode(&mut rng, 350_000);
                let at = rng.range(1, lane.len() as u64) as usize;
                let step = Step::Link {
                    key: format!("link/{id}/{target}"),
                    task: id.clone(),
                    target: target.clone(),
                    mode,
                };
                lane.insert(
                    at,
                    Planned {
                        step,
                        nap_ms: nap(&mut rng),
                        after: None,
                    },
                );
                if mode == Mode::Status && rng.chance(400_000) {
                    let later = rng.range(at as u64 + 1, lane.len() as u64) as usize;
                    let step = Step::Follow {
                        key: format!("follow/{id}/{target}"),
                        task: id.clone(),
                        target: target.clone(),
                    };
                    lane.insert(
                        later,
                        Planned {
                            step,
                            nap_ms: nap(&mut rng),
                            after: None,
                        },
                    );
                }
                if rng.chance(300_000) {
                    let later = rng.range(at as u64 + 1, lane.len() as u64) as usize;
                    let step = Step::Unlink {
                        key: format!("unlink/{id}/{target}"),
                        task: id.clone(),
                        target: target.clone(),
                    };
                    lane.insert(
                        later,
                        Planned {
                            step,
                            nap_ms: nap(&mut rng),
                            after: None,
                        },
                    );
                }
                threads.push((id.clone(), target, mode));
            }
            if rng.chance(500_000) {
                let step = Step::Cancel {
                    key: format!("cancel/{id}"),
                    task: id.clone(),
                };
                lane.push(Planned {
                    step,
                    nap_ms: nap(&mut rng),
                    after: None,
                });
            }
            lanes.push((id, quiet, lane));
        }
        let prs = rng.range(0, 2);
        let mut pr_lanes = Vec::new();
        for pr in 0..prs {
            let id = TaskId(format!("github/pr/{pr}"));
            let opened = format!("github/open/{pr}");
            let mut lane = Vec::new();
            for number in 0..rng.range(1, 2) {
                let key = format!("ans/{id}/{number}");
                let mut body = format!("{id} review {number}");
                if rng.chance(300_000) {
                    body.push_str(&format!(" @{}", MEMBERS[rng.pick(MEMBERS.len())]));
                }
                answers.push((key.clone(), false));
                let step = Step::Answer {
                    key,
                    task: id.clone(),
                    body,
                    controls: Vec::new(),
                };
                lane.push(Planned {
                    step,
                    nap_ms: nap(&mut rng),
                    after: Some(opened.clone()),
                });
            }
            pr_lanes.push(lane);
        }
        let work_ms = (lanes
            .iter()
            .map(|(_, _, lane)| lane.len() as u64)
            .sum::<u64>()
            + 4)
            * (step_ms + 4 * latency_ms + poll_ms)
            + 40;
        let mut plan = Plan {
            rng: &mut rng,
            hooks: Vec::new(),
            texts: 0,
            work_ms,
            poll_ms,
        };
        for pr in 0..prs {
            let target = Target::new("github", format!("pr/{pr}"));
            let id = TaskId(format!("github/pr/{pr}"));
            let opened = format!("github/open/{pr}");
            let task = NewTask {
                id: id.clone(),
                parent: None,
                spec: format!("pull {pr}"),
                labels: BTreeMap::from([("source".to_owned(), "github".to_owned())]),
                needs: Vec::new(),
                terms: Terms::default(),
            };
            let mode = mode(plan.rng, 350_000);
            let open_at = plan.rng.range(0, work_ms / 3);
            let what = What::Opened {
                task,
                message: format!("pr/{pr}/body"),
                mode,
            };
            plan.hook(
                opened.clone(),
                open_at,
                Place::Fixed(target.clone()),
                what,
                None,
            );
            let snapshots = plan.rng.range(1, 3);
            for version in 1..=snapshots {
                let label = format!("gh:l{}", plan.rng.range(0, 2));
                let set = BTreeMap::from([
                    ("gh:state".to_owned(), format!("s{version}")),
                    (label, "1".to_owned()),
                ]);
                let at = open_at + plan.rng.range(0, work_ms / 2);
                let what = What::Labels {
                    task: id.clone(),
                    set,
                    version,
                };
                plan.hook(
                    format!("github/labels/{pr}/{version}"),
                    at,
                    Place::Fixed(target.clone()),
                    what,
                    Some(opened.clone()),
                );
            }
            if plan.rng.chance(600_000) {
                for version in 1..=plan.rng.range(1, 4) {
                    let author = if plan.rng.chance(200_000) {
                        bot("github")
                    } else {
                        human(users("github")[plan.rng.pick(3)])
                    };
                    let body = plan
                        .rng
                        .chance(500_000)
                        .then(|| format!("body {version} by {}", author.name));
                    let what = What::Reworded {
                        author,
                        title: format!("pull {pr} title {version}"),
                        body,
                        version,
                    };
                    let at = open_at + plan.rng.range(0, work_ms / 2);
                    plan.hook(
                        format!("github/reword/{pr}/{version}"),
                        at,
                        Place::Fixed(target.clone()),
                        what,
                        Some(opened.clone()),
                    );
                }
            }
            for _ in 0..plan.rng.range(0, 2) {
                plan.talk(
                    &Place::Fixed(target.clone()),
                    "github",
                    Some(&opened),
                    false,
                );
            }
            if plan.rng.chance(500_000) {
                let at = open_at + plan.rng.range(work_ms / 4, work_ms);
                plan.hook(
                    format!("github/close/{pr}"),
                    at,
                    Place::Fixed(target),
                    What::Closed { task: id },
                    Some(opened),
                );
            }
        }
        for (id, quiet, _) in &lanes {
            if *quiet {
                continue;
            }
            for (channel, mode) in &channels {
                let place = Place::Root(id.clone(), channel.clone());
                for _ in 0..plan.rng.range(0, 2) {
                    plan.talk(&place, "slack", None, false);
                }
                if *mode == Mode::Status && plan.rng.chance(300_000) {
                    plan.talk(&place, "slack", None, true);
                }
            }
        }
        for (_, target, mode) in &threads {
            for _ in 0..plan.rng.range(0, 3) {
                plan.talk(&Place::Fixed(target.clone()), &target.sync, None, false);
            }
            if *mode == Mode::Status && plan.rng.chance(300_000) {
                plan.talk(&Place::Fixed(target.clone()), &target.sync, None, true);
            }
        }
        if plan.rng.chance(400_000) {
            plan.talk(
                &Place::Fixed(Target::new("linear", "ISS-none")),
                "linear",
                None,
                false,
            );
        }
        if plan.rng.chance(400_000) {
            plan.talk(
                &Place::Fixed(Target::new("slack", "C9:1.0")),
                "slack",
                None,
                false,
            );
        }
        let reach = |quiet: bool, key: &str| -> String {
            let task = key
                .trim_start_matches("ans/")
                .rsplit_once('/')
                .map_or("", |(task, _)| task);
            let thread = threads
                .iter()
                .find(|(id, _, _)| id.0 == task)
                .map(|(_, target, _)| target.sync.clone());
            match (task.starts_with("github/"), quiet, thread) {
                (true, _, _) => "github".to_owned(),
                (false, false, _) => "slack".to_owned(),
                (false, true, Some(sync)) => sync,
                (false, true, None) => "slack".to_owned(),
            }
        };
        for (key, control, quiet) in &controlled {
            if !plan.rng.chance(700_000) {
                continue;
            }
            let sync = reach(*quiet, key);
            let pool = users(&sync);
            let author = if plan.rng.chance(150_000) {
                bot(&sync)
            } else {
                human(pool[plan.rng.pick(pool.len())])
            };
            let values = match control {
                Control::Form { .. } if plan.rng.chance(800_000) => {
                    BTreeMap::from([("reason".to_owned(), "because".to_owned())])
                }
                Control::Form { .. } => BTreeMap::from([("nope".to_owned(), "x".to_owned())]),
                Control::Button { .. } => BTreeMap::new(),
            };
            let at = plan.at();
            let what = What::Click {
                author,
                control: control.id().to_owned(),
                values,
            };
            plan.hook(
                format!("{sync}/click/{key}"),
                at,
                Place::Copy(key.clone(), sync.clone()),
                what,
                None,
            );
        }
        for (key, quiet) in &answers {
            if !plan.rng.chance(300_000) {
                continue;
            }
            let sync = reach(*quiet, key);
            let pool = users(&sync);
            let author = if plan.rng.chance(200_000) {
                bot(&sync)
            } else {
                human(pool[plan.rng.pick(pool.len())])
            };
            let at = plan.at();
            plan.hook(
                format!("{sync}/react/{key}"),
                at,
                Place::Copy(key.clone(), sync.clone()),
                What::React { author },
                None,
            );
        }
        let hooks = plan.hooks;
        let roots = lanes
            .iter()
            .filter(|(_, quiet, _)| !quiet)
            .map(|(id, _, _)| format!("post/{id}/post"))
            .collect::<Vec<_>>();
        let poison = rng.chance(350_000).then(|| {
            if !roots.is_empty() && rng.chance(400_000) {
                roots[rng.pick(roots.len())].clone()
            } else {
                answers[rng.pick(answers.len())].0.clone()
            }
        });
        let mut lead = vec![Planned {
            step: Step::Accounts,
            nap_ms: 0,
            after: None,
        }];
        let mut all = lanes
            .into_iter()
            .map(|(_, _, lane)| lane)
            .collect::<Vec<_>>();
        all.extend(pr_lanes);
        lead.extend(merge(&mut rng, all));
        let crashes = (0..rng.range(0, 2))
            .map(|_| rng.range(1, work_ms * 2))
            .collect();
        let mut limits = Rng::derive(seed, "board sync rate limits");
        let storm = limits.chance(300_000);
        let limit_ppm = if storm {
            limits.range(100_000, 500_000)
        } else if limits.chance(300_000) {
            limits.range(0, 100_000)
        } else {
            0
        };
        let retry_ms = limits.range(1, 4 * poll_ms + 20);
        let burst = if storm && limits.chance(BURST_PPM) {
            limits.range(ATTEMPTS, 4 * ATTEMPTS)
        } else {
            0
        };
        let (refuse_ppm, lose_ppm, poison) = if storm {
            (0, 0, None)
        } else {
            (refuse_ppm, lose_ppm, poison)
        };
        Self {
            poll_ms,
            latency_ms,
            before_ppm,
            after_ppm,
            read_ppm,
            refuse_ppm,
            lose_ppm,
            limit_ppm,
            retry_ms,
            storm,
            burst,
            channels,
            identity,
            lead,
            hooks,
            poison,
            crashes,
            patience_ms: work_ms * 4 + 400,
            deadline_ms: work_ms * 40 + 60_000,
        }
    }

    fn describe(&self) -> String {
        format!(
            "lead_steps={} hooks={} channels={:?} poll_ms={} latency_ms={} faults_ppm={}/{}/{} refuse_ppm={} lose_ppm={} limit_ppm={} retry_ms={} storm={} burst={} poison={} crashes={:?}",
            self.lead.len(),
            self.hooks.len(),
            self.channels.values().collect::<Vec<_>>(),
            self.poll_ms,
            self.latency_ms,
            self.before_ppm,
            self.after_ppm,
            self.read_ppm,
            self.refuse_ppm,
            self.lose_ppm,
            self.limit_ppm,
            self.retry_ms,
            self.storm,
            self.burst,
            self.poison.as_deref().unwrap_or("none"),
            self.crashes
        )
    }
}

struct Quiet(Channels);

impl Router for Quiet {
    fn route(&self, thread: &Target, author: &Author, body: &str) -> Route {
        self.0.route(thread, author, body)
    }

    fn mirror(&self, task: &NewTask) -> Vec<(Target, Mode)> {
        if task.labels.contains_key("quiet") {
            Vec::new()
        } else {
            self.0.mirror(task)
        }
    }
}

#[derive(Default)]
struct Outside {
    acked: BTreeSet<String>,
    skipped: BTreeSet<String>,
    sent: BTreeMap<String, Target>,
}

#[derive(Default)]
struct Progress {
    lead: usize,
    accounts: bool,
    lead_done: bool,
    outside_done: bool,
    echo_idle: bool,
    drained: bool,
    abandoned: bool,
    wrong: Vec<String>,
    stats: SyncStats,
}

struct World {
    shape: Shape,
    clock: Arc<Clock>,
    host: Arc<FaultLog>,
    fakes: BTreeMap<String, Arc<Fake>>,
    outside: RefCell<Outside>,
    progress: RefCell<Progress>,
}

fn faults(
    seed: u64,
    sync: &str,
    shape: &Shape,
) -> impl FnMut(&str, &Target) -> Fault + Send + 'static {
    let mut rng = Rng::derive(seed, &format!("board sync {sync} faults"));
    let (refuse_ppm, lose_ppm, limit_ppm, retry_ms) = (
        shape.refuse_ppm,
        shape.lose_ppm,
        shape.limit_ppm,
        shape.retry_ms,
    );
    let mut burst = shape.burst;
    move |call: &str, _to: &Target| {
        if burst > 0 && call != "find" {
            burst -= 1;
            return Fault::Limit(rng.range(1, retry_ms));
        }
        if rng.chance(refuse_ppm) {
            Fault::Refuse
        } else if rng.chance(lose_ppm) {
            Fault::Lose
        } else if limit_ppm > 0 && rng.chance(limit_ppm) {
            Fault::Limit(rng.range(1, retry_ms + 1))
        } else {
            Fault::None
        }
    }
}

impl World {
    fn now_ms(&self) -> u64 {
        self.clock.now_ns() / MS
    }

    async fn nap(&self, ms: u64) {
        self.clock.sleep(Duration::from_nanos(ms.max(1) * MS)).await;
    }

    async fn open(&self, writer: &str) -> Board {
        loop {
            let host: Arc<dyn LogHost> = Arc::clone(&self.host) as Arc<dyn LogHost>;
            match Board::open(host, SCOPE, identity(writer)).await {
                Ok(board) => return board,
                Err(error) => {
                    self.note(&error);
                    self.nap(self.shape.poll_ms).await;
                }
            }
        }
    }

    fn engine(&self) -> Engine<Plain, Quiet, Accounts> {
        let channels = Channels {
            mirrored: self.shape.channels.clone(),
            bot: BTreeMap::new(),
            start: false,
        };
        let platforms = self
            .fakes
            .iter()
            .map(|(sync, fake)| (sync.clone(), Arc::clone(fake) as Arc<dyn Platform>))
            .collect();
        Engine::new(
            Plain,
            Quiet(channels),
            Accounts(self.shape.identity.clone()),
            platforms,
            ATTEMPTS,
        )
    }

    fn note(&self, error: &BoardError) {
        let mut progress = self.progress.borrow_mut();
        match error {
            BoardError::Corrupt { .. } => progress.wrong.push(format!("{error}")),
            BoardError::Unsettled { .. } => progress.stats.unsettled += 1,
            BoardError::Rejected(_)
            | BoardError::Store(_)
            | BoardError::Dropped(_)
            | BoardError::Row { .. } => {}
        }
    }

    fn copy(&self, sync: &str, marker: &str) -> Option<fake::Copy> {
        self.fakes[sync]
            .copies()
            .into_iter()
            .find(|copy| keyed(&copy.marker, marker) && !copy.deleted)
    }

    fn root_thread(&self, task: &TaskId, channel: &Target) -> Option<Target> {
        let root = root_id(task);
        let copies = self.fakes[&channel.sync].copies();
        let copy = copies
            .iter()
            .find(|copy| keyed(&copy.marker, &root) && copy.to == *channel)?;
        copy.thread
            .as_ref()
            .map(|thread| Target::new(&channel.sync, thread.clone()))
    }

    fn resolve(&self, place: &Place) -> Option<(Target, Option<String>)> {
        match place {
            Place::Fixed(target) => Some((target.clone(), None)),
            Place::Root(task, channel) => {
                self.root_thread(task, channel).map(|thread| (thread, None))
            }
            Place::Copy(marker, sync) => self
                .copy(sync, marker)
                .map(|copy| (copy.to, Some(copy.external))),
        }
    }

    fn event(&self, thread: Target, external: Option<String>, what: &What) -> Event {
        match what {
            What::Opened {
                task,
                message,
                mode,
            } => Event::Opened {
                task: task.clone(),
                thread,
                message: message.clone(),
                mode: *mode,
            },
            What::Message {
                message,
                author,
                body,
            } => Event::Message {
                thread,
                message: message.clone(),
                author: author.clone(),
                body: body.clone(),
                context: None,
                sub: None,
                marker: None,
            },
            What::Edited {
                message,
                body,
                version,
            } => Event::Edited {
                thread,
                message: message.clone(),
                body: body.clone(),
                version: *version,
            },
            What::Deleted { message } => Event::Deleted {
                thread,
                message: message.clone(),
            },
            What::Reworded {
                author,
                title,
                body,
                version,
            } => Event::Reworded {
                thread,
                author: author.clone(),
                title: title.clone(),
                body: body.clone(),
                version: *version,
            },
            What::Labels { task, set, version } => Event::Labels {
                task: task.clone(),
                set: set.clone(),
                unset: Vec::new(),
                own: vec!["gh:".to_owned()],
                version: *version,
            },
            What::Closed { task } => Event::Closed {
                task: task.clone(),
                outcome: Settle::Done("merged".to_owned()),
            },
            What::React { author } => Event::Reaction {
                thread,
                message: external.unwrap_or_default(),
                author: author.clone(),
                emoji: "eyes".to_owned(),
            },
            What::Click {
                author,
                control,
                values,
            } => Event::Clicked {
                thread,
                message: external.unwrap_or_default(),
                author: author.clone(),
                control: control.clone(),
                values: values.clone(),
            },
        }
    }
}

async fn settle<T>(world: &World, sent: Result<T, BoardError>) -> Option<bool> {
    match sent {
        Ok(_) => Some(true),
        Err(BoardError::Rejected(_)) => Some(false),
        Err(error) => {
            world.note(&error);
            world.nap(world.shape.poll_ms).await;
            None
        }
    }
}

async fn lead(world: &World) {
    let mut board = world.open(LEAD).await;
    let engine = world.engine();
    loop {
        let at = world.progress.borrow().lead;
        let Some(planned) = world.shape.lead.get(at).cloned() else {
            break;
        };
        if let Some(after) = &planned.after {
            let ready = world.outside.borrow().acked.contains(after);
            let skipped = world.outside.borrow().skipped.contains(after);
            if skipped {
                world.progress.borrow_mut().lead += 1;
                continue;
            }
            if !ready {
                world.nap(world.shape.poll_ms).await;
                continue;
            }
        }
        world.nap(planned.nap_ms).await;
        let now = world.now_ms();
        let done = match &planned.step {
            Step::Accounts => {
                let mut all = true;
                for ((sync, user), member) in &world.shape.identity {
                    let op = SyncOp::Account {
                        member: member.clone(),
                        sync: sync.clone(),
                        user: user.clone(),
                        linked: true,
                    };
                    let sent = engine
                        .apply(&mut board, &format!("acct/{sync}/{user}"), now, op)
                        .await;
                    if settle(world, sent).await.is_none() {
                        all = false;
                        break;
                    }
                }
                if all {
                    world.progress.borrow_mut().accounts = true;
                }
                all.then_some(true)
            }
            Step::Post(task) => {
                let sent = engine
                    .post(&mut board, &format!("post/{}", task.id), now, task.clone())
                    .await;
                settle(world, sent).await
            }
            Step::Answer {
                key,
                task,
                body,
                controls,
            } => {
                let op = SyncOp::Answer {
                    task: task.clone(),
                    body: body.clone(),
                    reply_to: None,
                    controls: controls.clone(),
                };
                settle(world, engine.apply(&mut board, key, now, op).await).await
            }
            Step::Revise {
                key,
                task,
                message,
                body,
            } => {
                let op = SyncOp::Revise {
                    task: task.clone(),
                    message: EventId(message.clone()),
                    body: body.clone(),
                };
                settle(world, engine.apply(&mut board, key, now, op).await).await
            }
            Step::Retract { key, task, message } => {
                let op = SyncOp::Retract {
                    task: task.clone(),
                    message: EventId(message.clone()),
                };
                settle(world, engine.apply(&mut board, key, now, op).await).await
            }
            Step::Link {
                key,
                task,
                target,
                mode,
            } => {
                let op = SyncOp::Link {
                    task: task.clone(),
                    target: target.clone(),
                    join: Join::Thread,
                    mode: *mode,
                };
                settle(world, engine.apply(&mut board, key, now, op).await).await
            }
            Step::Unlink { key, task, target } => {
                let op = SyncOp::Unlink {
                    task: task.clone(),
                    target: target.clone(),
                };
                settle(world, engine.apply(&mut board, key, now, op).await).await
            }
            Step::Follow { key, task, target } => {
                let op = SyncOp::Mode {
                    task: task.clone(),
                    target: target.clone(),
                    mode: Mode::Always,
                };
                settle(world, engine.apply(&mut board, key, now, op).await).await
            }
            Step::Cancel { key, task } => {
                let op = Op::Cancel {
                    task: task.clone(),
                    reason: "lead replanned".to_owned(),
                };
                settle(world, board.submit_keyed(key, now, op).await).await
            }
        };
        if done.is_some() {
            world.progress.borrow_mut().lead += 1;
        }
    }
    world.progress.borrow_mut().lead_done = true;
}

async fn outside(world: &World, epoch: u64) {
    let mut board = world.open(&format!("{SYNC}+outside")).await;
    let engine = world.engine();
    let mut rng = Rng::derive(epoch, "board sync outside");
    while !world.progress.borrow().accounts {
        world.nap(world.shape.poll_ms).await;
    }
    let start = world.now_ms();
    let mut queue = world
        .shape
        .hooks
        .iter()
        .filter(|hook| {
            !world.outside.borrow().acked.contains(&hook.key)
                && !world.outside.borrow().skipped.contains(&hook.key)
        })
        .flat_map(|hook| (0..hook.copies).map(move |copy| (hook.clone(), copy)))
        .collect::<Vec<_>>();
    queue.sort_by_key(|(hook, copy)| (hook.at_ms + copy * world.shape.poll_ms, hook.key.clone()));
    let mut retry = Vec::new();
    while !queue.is_empty() || !retry.is_empty() {
        if queue.is_empty() {
            queue = std::mem::take(&mut retry);
            world.nap(world.shape.poll_ms).await;
        }
        let (hook, copy) = queue.remove(0);
        let wake = start + hook.at_ms + copy * world.shape.poll_ms;
        if world.now_ms() < wake {
            world.nap(wake - world.now_ms()).await;
        }
        if world.outside.borrow().acked.contains(&hook.key) && copy > 0 && rng.chance(500_000) {
            continue;
        }
        let late = world.now_ms() > start + world.shape.patience_ms;
        let waiting = hook
            .after
            .as_ref()
            .is_some_and(|after| !world.outside.borrow().acked.contains(after));
        let resolved = if waiting {
            None
        } else {
            world.resolve(&hook.place)
        };
        let Some((thread, external)) = resolved else {
            if late {
                world.outside.borrow_mut().skipped.insert(hook.key.clone());
            } else {
                retry.push((hook, copy));
            }
            continue;
        };
        if rng.chance(150_000) && !late {
            retry.push((hook, copy));
            continue;
        }
        let inbound = Inbound {
            key: hook.key.clone(),
            event: world.event(thread.clone(), external, &hook.what),
        };
        match engine.ingest(&mut board, world.now_ms(), &inbound).await {
            Ok(_) => {
                let mut outside = world.outside.borrow_mut();
                outside.acked.insert(hook.key.clone());
                outside.sent.insert(hook.key.clone(), thread);
            }
            Err(error) => {
                world.note(&error);
                retry.push((hook, copy));
            }
        }
    }
    world.progress.borrow_mut().outside_done = true;
}

async fn echo(world: &World, epoch: u64) {
    let mut board = world.open(&format!("{SYNC}+echo")).await;
    let engine = world.engine();
    let mut rng = Rng::derive(epoch, "board sync echo");
    let mut queue: Vec<Inbound> = Vec::new();
    loop {
        for (sync, fake) in &world.fakes {
            for echoed in fake.echoes() {
                let Echo::Posted {
                    to,
                    external,
                    marker,
                    text,
                } = echoed
                else {
                    continue;
                };
                let thread = if sync == "slack" && !to.id.contains(':') && !to.is_dm() {
                    Target::new(sync, format!("{}:{external}", to.id))
                } else {
                    to
                };
                let event = Event::Message {
                    thread,
                    message: external.clone(),
                    author: bot(sync),
                    body: text,
                    context: None,
                    sub: None,
                    marker: Some(marker),
                };
                queue.push(Inbound {
                    key: format!("echo/{external}"),
                    event,
                });
            }
        }
        if queue.is_empty() {
            world.progress.borrow_mut().echo_idle = true;
            if world.progress.borrow().drained {
                break;
            }
            world.nap(world.shape.poll_ms).await;
            continue;
        }
        world.progress.borrow_mut().echo_idle = false;
        let inbound = queue.remove(rng.pick(queue.len()));
        world.nap(rng.range(0, world.shape.poll_ms)).await;
        if let Err(error) = engine.ingest(&mut board, world.now_ms(), &inbound).await {
            world.note(&error);
            queue.push(inbound);
        }
    }
}

async fn drainer(world: &World) {
    let mut board = world.open(&format!("{SYNC}+drain")).await;
    let mut engine = world.engine();
    for fake in world.fakes.values() {
        fake.restart();
    }
    let mut since = None;
    loop {
        let now = world.now_ms();
        for fake in world.fakes.values() {
            fake.at(now);
        }
        match engine.drain(&mut board, now).await {
            Ok(drained) => world.progress.borrow_mut().stats.unsettled += drained.unsettled,
            Err(error) => world.note(&error),
        }
        let calm = {
            let progress = world.progress.borrow();
            progress.lead_done && progress.outside_done && progress.echo_idle
        };
        if calm
            && board.refresh().await.is_ok()
            && board.fold().outbox().is_empty()
            && world
                .fakes
                .values()
                .all(|fake| fake.lock().echoes.is_empty())
        {
            break;
        }
        if calm {
            let first = *since.get_or_insert(world.now_ms());
            if world.now_ms() > first + world.shape.patience_ms {
                world.progress.borrow_mut().abandoned = true;
                break;
            }
        }
        world.nap(world.shape.poll_ms).await;
    }
    world.progress.borrow_mut().drained = true;
}

async fn charter(world: &World) {
    let mut board = world.open(OWNER).await;
    let grant = Grant {
        who: vec![Who::Principal(SYNC.to_owned())],
        can: BTreeMap::from([(
            "*".to_owned(),
            vec![Can::Read, Can::Post, Can::Label, Can::Sync],
        )]),
    };
    for (key, op) in [
        ("charter", Op::Charter {}),
        ("grant:sync", Op::Grant { grant }),
    ] {
        loop {
            match board.submit_keyed(key, world.now_ms(), op.clone()).await {
                Ok(_) | Err(BoardError::Rejected(Rejection::Exists)) => break,
                Err(error @ BoardError::Rejected(_)) => {
                    world
                        .progress
                        .borrow_mut()
                        .wrong
                        .push(format!("the owner could not write {key}: {error}"));
                    break;
                }
                Err(error) => {
                    world.note(&error);
                    world.nap(world.shape.poll_ms).await;
                }
            }
        }
    }
}

async fn epoch(world: &World, number: u64) {
    {
        let mut progress = world.progress.borrow_mut();
        progress.lead_done = false;
        progress.outside_done = false;
        progress.echo_idle = false;
        progress.drained = false;
    }
    charter(world).await;
    let actors: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>>> = vec![
        Box::pin(lead(world)),
        Box::pin(outside(world, number)),
        Box::pin(echo(world, number)),
        Box::pin(drainer(world)),
    ];
    nanocode_sync::join_in_order(actors).await;
}

#[derive(Clone, Debug)]
struct Link {
    task: TaskId,
    channel: bool,
    to: Target,
    mode: Mode,
    since: u64,
    thread: Option<Target>,
    root: bool,
    states: Vec<(u64, Mode, u64)>,
    unlinked: Option<u64>,
}

#[derive(Clone, Debug)]
struct Said {
    task: TaskId,
    kind: Kind,
    seq: u64,
    order: usize,
    author: String,
    body: String,
    origin: Option<(Target, Target)>,
    controls: Vec<Control>,
    deleted: bool,
    version: u64,
    dms: Vec<Target>,
}

type Waiting = (EventId, String, SyncOp);

#[derive(Default)]
struct Reference {
    tasks: BTreeMap<TaskId, (BTreeMap<String, String>, bool)>,
    words: BTreeMap<TaskId, (String, u64)>,
    links: BTreeMap<Target, Link>,
    unlinked: Vec<(Target, Link)>,
    ever: BTreeSet<(TaskId, Target)>,
    threads: BTreeMap<Target, Target>,
    gone: BTreeSet<Target>,
    accounts: BTreeMap<(String, String), String>,
    members: BTreeMap<String, BTreeMap<String, String>>,
    said: BTreeMap<EventId, Said>,
    by_task: BTreeMap<TaskId, Vec<EventId>>,
    status: BTreeMap<TaskId, EventId>,
    external: BTreeMap<(Target, String), EventId>,
    copies: BTreeSet<(EventId, Target)>,
    attempted: BTreeSet<(EventId, Target)>,
    dead: BTreeSet<(EventId, Target)>,
    parked: BTreeMap<(Target, Option<String>), Vec<Waiting>>,
    read: BTreeMap<(TaskId, String), u64>,
    seen: BTreeMap<(TaskId, String), u64>,
    owned: BTreeMap<(TaskId, String), u64>,
    imports: BTreeMap<EventId, String>,
    orders: usize,
}

fn eligible(channel: bool, mode: Mode, since: u64, said: &Said) -> bool {
    let starts = channel && said.kind == Kind::Root;
    match (mode, said.kind) {
        (_, Kind::Status) => mode == Mode::Status,
        (Mode::Status, Kind::Final) => true,
        (Mode::Status, _) => starts,
        (Mode::Always, _) => starts || said.seq > since,
    }
}

impl Link {
    fn now(&self, said: &Said) -> bool {
        eligible(self.channel, self.mode, self.since, said)
    }

    fn ever(&self, said: &Said) -> bool {
        self.states
            .iter()
            .enumerate()
            .any(|(index, (_, mode, since))| {
                let ends = self
                    .states
                    .get(index + 1)
                    .map_or(u64::MAX, |(from, _, _)| *from);
                ends > said.seq && eligible(self.channel, *mode, *since, said)
            })
    }
}

impl Reference {
    fn key_of(&self, task: &TaskId, target: &Target) -> Option<Target> {
        [target.clone(), channel_key(&task.0, target)]
            .into_iter()
            .find(|key| self.links.get(key).is_some_and(|link| link.task == *task))
    }

    fn add(
        &mut self,
        id: EventId,
        task: &TaskId,
        seq: u64,
        kind: Kind,
        author: String,
        body: String,
    ) {
        let named = if kind == Kind::Status {
            Vec::new()
        } else {
            named(&body)
        };
        let linked = self.links.values().any(|link| link.task == *task);
        let dms = if linked {
            Vec::new()
        } else {
            named
                .iter()
                .filter_map(|member| self.members.get(member)?.iter().next())
                .map(|(sync, user)| Target::dm(sync, user))
                .collect()
        };
        self.orders += 1;
        let said = Said {
            task: task.clone(),
            kind,
            seq,
            order: self.orders,
            author,
            body,
            origin: None,
            controls: Vec::new(),
            deleted: false,
            version: 0,
            dms,
        };
        if kind == Kind::Status {
            self.status.insert(task.clone(), id.clone());
        } else {
            self.by_task
                .entry(task.clone())
                .or_default()
                .push(id.clone());
        }
        self.said.insert(id, said);
    }

    fn settle(&mut self, seq: u64, id: &EventId, by: &str, task: &TaskId, text: String) {
        let Some((_, terminal)) = self.tasks.get_mut(task) else {
            return;
        };
        if *terminal {
            return;
        }
        *terminal = true;
        if let Some(status) = self.status.get(task).cloned() {
            self.said
                .get_mut(&status)
                .expect("a status is indexed")
                .body
                .clone_from(&text);
        }
        self.add(
            EventId(format!("{}#final:{}", id.0, task.0)),
            task,
            seq,
            Kind::Final,
            by.to_owned(),
            text,
        );
    }

    fn of(rows: &[(u64, String)]) -> Result<Self, String> {
        let mut model = Self::default();
        let mut seen = BTreeSet::new();
        for (seq, body) in rows {
            let row: Row = serde_json::from_str(body)
                .map_err(|error| format!("row {seq} is not an event: {error}"))?;
            if !seen.insert(row.id.clone()) {
                continue;
            }
            match row.op {
                Op::Post { task } => {
                    if model.tasks.contains_key(&task.id) {
                        continue;
                    }
                    model
                        .tasks
                        .insert(task.id.clone(), (task.labels.clone(), false));
                    model.words.insert(task.id.clone(), (task.spec.clone(), 0));
                    model.add(
                        row.id.clone(),
                        &task.id,
                        *seq,
                        Kind::Root,
                        row.by.clone(),
                        task.spec.clone(),
                    );
                    model.add(
                        EventId(format!("{}#status", task.id.0)),
                        &task.id,
                        *seq,
                        Kind::Status,
                        row.by.clone(),
                        "open".to_owned(),
                    );
                }
                Op::Cancel { task, reason } => model.settle(
                    *seq,
                    &row.id,
                    &row.by,
                    &task,
                    format!("cancelled: {reason}"),
                ),
                Op::Sync { sync } => model.sync(*seq, &row.id, &row.by, &sync),
                _ => {}
            }
        }
        Ok(model)
    }

    fn located(&self, thread: &Target, message: Option<&String>) -> Result<Target, bool> {
        let Some(link) = self.threads.get(thread) else {
            return Err(!self.gone.contains(thread));
        };
        match message {
            Some(message) if !self.external.contains_key(&(link.clone(), message.clone())) => {
                Err(true)
            }
            _ => Ok(link.clone()),
        }
    }

    fn found(&mut self, seq: u64, thread: Target, key: Target) {
        self.gone.remove(&thread);
        self.threads.insert(thread.clone(), key);
        self.unpark(seq, &(thread, None));
    }

    fn unpark(&mut self, seq: u64, key: &(Target, Option<String>)) {
        for (id, by, op) in self.parked.remove(key).unwrap_or_default() {
            self.sync(seq, &id, &by, &op);
        }
    }

    fn mark(&mut self, task: &TaskId, sync: &str, author: &Author) {
        if author.bot {
            return;
        }
        let Some(member) = self
            .accounts
            .get(&(sync.to_owned(), author.user.clone()))
            .cloned()
        else {
            return;
        };
        let latest = self
            .by_task
            .get(task)
            .and_then(|ids| ids.last())
            .map_or(0, |id| self.said[id].seq);
        let held = self.read.entry((task.clone(), member)).or_default();
        *held = (*held).max(latest);
    }

    fn author(&self, sync: &str, author: &Author) -> String {
        self.accounts
            .get(&(sync.to_owned(), author.user.clone()))
            .cloned()
            .unwrap_or_else(|| format!("{sync}:{}~{}", author.user, author.name))
    }

    fn imported(&self, thread: &Target, message: &str) -> Option<EventId> {
        let link = self.threads.get(thread)?;
        let id = self.external.get(&(link.clone(), message.to_owned()))?;
        let origin = self.said[id]
            .origin
            .as_ref()
            .is_some_and(|(at, key)| at == thread || key == link);
        origin.then(|| id.clone())
    }

    fn routable(&self, said: &Said, target: &Target) -> bool {
        self.ever.contains(&(said.task.clone(), target.clone())) || said.dms.contains(target)
    }

    #[allow(clippy::too_many_lines)]
    fn sync(&mut self, seq: u64, id: &EventId, by: &str, op: &SyncOp) {
        let place = match op {
            SyncOp::Import { thread, .. } | SyncOp::Reword { thread, .. } => Some((thread, None)),
            SyncOp::Edit {
                thread, message, ..
            }
            | SyncOp::Delete { thread, message }
            | SyncOp::React {
                thread, message, ..
            }
            | SyncOp::Act {
                thread, message, ..
            } => Some((thread, Some(message))),
            _ => None,
        };
        if let Some((thread, message)) = place
            && let Err(wait) = self.located(thread, message)
        {
            if wait {
                let key = (
                    thread.clone(),
                    message
                        .cloned()
                        .filter(|_| self.threads.contains_key(thread)),
                );
                self.parked
                    .entry(key)
                    .or_default()
                    .push((id.clone(), by.to_owned(), op.clone()));
            }
            return;
        }
        match op {
            SyncOp::Link {
                task,
                target,
                join,
                mode,
            } => {
                let channel = matches!(join, Join::Channel);
                let key = if channel {
                    channel_key(&task.0, target)
                } else {
                    target.clone()
                };
                if !self.tasks.contains_key(task) || self.links.contains_key(&key) {
                    return;
                }
                let since = if channel { 0 } else { seq };
                let thread = (!channel).then(|| target.clone());
                let root = matches!(join, Join::Root { .. });
                let link = Link {
                    task: task.clone(),
                    channel,
                    to: target.clone(),
                    mode: *mode,
                    since,
                    thread: thread.clone(),
                    root,
                    states: vec![(seq, *mode, since)],
                    unlinked: None,
                };
                self.links.insert(key.clone(), link);
                self.ever.insert((task.clone(), key.clone()));
                if let Join::Root { message } = join
                    && let Some(root) = self.by_task.get(task).and_then(|ids| ids.first()).cloned()
                {
                    let said = self.said.get_mut(&root).expect("a root is indexed");
                    if said.origin.is_none() {
                        said.origin = Some((target.clone(), key.clone()));
                    }
                    self.external
                        .insert((target.clone(), message.clone()), root);
                }
                if let Some(thread) = thread {
                    self.found(seq, thread, key);
                }
            }
            SyncOp::Unlink { task, target } => {
                let Some(key) = self.key_of(task, target) else {
                    return;
                };
                let mut link = self.links.remove(&key).expect("a found link exists");
                let thread = link.thread.clone().unwrap_or_else(|| key.clone());
                self.threads.remove(&thread);
                self.gone.insert(thread.clone());
                self.gone.insert(key.clone());
                self.parked.retain(|(parked, _), _| *parked != thread);
                link.unlinked = Some(seq);
                self.unlinked.push((key, link));
            }
            SyncOp::Mode { task, target, mode } => {
                let Some(key) = self.key_of(task, target) else {
                    return;
                };
                let link = self.links.get_mut(&key).expect("a found link exists");
                if link.mode == *mode {
                    return;
                }
                link.mode = *mode;
                if *mode == Mode::Always {
                    link.since = link.since.max(seq);
                }
                link.states.push((seq, link.mode, link.since));
            }
            SyncOp::Account {
                member,
                sync,
                user,
                linked,
            } => {
                if *linked {
                    self.accounts
                        .insert((sync.clone(), user.clone()), member.clone());
                    self.members
                        .entry(member.clone())
                        .or_default()
                        .insert(sync.clone(), user.clone());
                }
            }
            SyncOp::Import {
                thread,
                message,
                author,
                body,
                ..
            } => {
                let SyncOp::Import { marker, .. } = op else {
                    return;
                };
                let key = self.threads[thread].clone();
                let task = self.links[&key].task.clone();
                if self.external.contains_key(&(key.clone(), message.clone())) {
                    return;
                }
                if author.bot {
                    let ours = marker
                        .as_ref()
                        .filter(|marker| {
                            self.said.get(*marker).is_some_and(|said| said.task == task)
                                && !self.copies.contains(&((*marker).clone(), key.clone()))
                        })
                        .cloned();
                    if let Some(marker) = ours {
                        self.copies.insert((marker.clone(), key.clone()));
                        self.dead.remove(&(marker.clone(), key.clone()));
                        self.external.insert((key, message.clone()), marker);
                        self.unpark(seq, &(thread.clone(), Some(message.clone())));
                    }
                    return;
                }
                let writer = self.author(&key.sync, author);
                self.add(
                    id.clone(),
                    &task,
                    seq,
                    Kind::Reply,
                    writer.clone(),
                    body.clone(),
                );
                self.said.get_mut(id).expect("an import is indexed").origin =
                    Some((thread.clone(), key.clone()));
                self.external
                    .insert((key.clone(), message.clone()), id.clone());
                self.imports.insert(id.clone(), writer);
                self.mark(&task, &key.sync, author);
                if body.trim().eq_ignore_ascii_case("follow")
                    && let Some(link) = self.links.get_mut(&key)
                    && link.mode == Mode::Status
                {
                    link.mode = Mode::Always;
                    link.since = link.since.max(seq);
                    link.states.push((seq, link.mode, link.since));
                }
                self.unpark(seq, &(thread.clone(), Some(message.clone())));
            }
            SyncOp::Edit {
                thread,
                message,
                body,
                version,
            } => {
                let Some(id) = self.imported(thread, message) else {
                    return;
                };
                let said = self.said.get_mut(&id).expect("an import is indexed");
                if said.deleted || *version <= said.version {
                    return;
                }
                said.version = *version;
                said.body.clone_from(body);
            }
            SyncOp::Reword {
                thread,
                author,
                title,
                body,
                version,
            } => {
                let link = &self.links[&self.threads[thread]];
                let task = link.task.clone();
                if author.bot || !link.root {
                    return;
                }
                let Some((spec, seen)) = self.words.get_mut(&task) else {
                    return;
                };
                if *version <= *seen {
                    return;
                }
                *seen = *version;
                *spec = match body {
                    Some(body) if !body.trim().is_empty() => format!("{title}\n\n{body}"),
                    _ => title.clone(),
                };
                let spec = spec.clone();
                if let Some(root) = self.by_task.get(&task).and_then(|ids| ids.first()).cloned()
                    && let Some(said) = self.said.get_mut(&root)
                    && said.kind == Kind::Root
                {
                    said.body = spec;
                }
            }
            SyncOp::Delete { thread, message } => {
                if let Some(id) = self.imported(thread, message) {
                    self.said
                        .get_mut(&id)
                        .expect("an import is indexed")
                        .deleted = true;
                }
            }
            SyncOp::React {
                thread,
                message,
                author,
                ..
            }
            | SyncOp::Act {
                thread,
                message,
                author,
                ..
            } => {
                let key = self.threads[thread].clone();
                let target = self.external[&(key.clone(), message.clone())].clone();
                let said = &self.said[&target];
                let task = said.task.clone();
                if let SyncOp::Act {
                    control, values, ..
                } = op
                {
                    let valid = said
                        .controls
                        .iter()
                        .any(|known| known.id() == control && accepts(known, values));
                    if !valid || author.bot {
                        return;
                    }
                }
                self.mark(&task, &key.sync, author);
            }
            SyncOp::Answer {
                task,
                body,
                controls,
                ..
            } => {
                if !self.tasks.contains_key(task) {
                    return;
                }
                self.add(
                    id.clone(),
                    task,
                    seq,
                    Kind::Reply,
                    by.to_owned(),
                    body.clone(),
                );
                self.said
                    .get_mut(id)
                    .expect("an answer is indexed")
                    .controls
                    .clone_from(controls);
            }
            SyncOp::Revise { task, message, .. } | SyncOp::Retract { task, message } => {
                let Some(said) = self.said.get_mut(message) else {
                    return;
                };
                let board = matches!(said.kind, Kind::Root | Kind::Reply) && said.origin.is_none();
                if said.task != *task || !board || said.deleted {
                    return;
                }
                match op {
                    SyncOp::Revise { body, .. } => said.body.clone_from(body),
                    _ => said.deleted = true,
                }
            }
            SyncOp::Label {
                task,
                set,
                unset,
                own,
                version,
            } => self.label(task, set, unset, own, *version),
            SyncOp::Settle { task, outcome } => {
                if !self
                    .links
                    .values()
                    .any(|link| link.task == *task && link.root)
                {
                    return;
                }
                let text = match outcome {
                    Settle::Done(result) => format!("done: {result}"),
                    Settle::Cancelled(reason) => format!("cancelled: {reason}"),
                };
                self.settle(seq, id, by, task, text);
            }
            SyncOp::Attempt {
                message, target, ..
            } => {
                self.attempted.insert((message.clone(), target.clone()));
            }
            SyncOp::Throttled { .. } => {}
            SyncOp::Synced {
                task,
                message,
                target,
                external,
                thread,
                ..
            } => {
                let Some(said) = self.said.get(message) else {
                    return;
                };
                let echo = said
                    .origin
                    .as_ref()
                    .is_some_and(|(at, key)| key == target || at == target);
                if said.task != *task
                    || !self.routable(said, target)
                    || echo
                    || self.copies.contains(&(message.clone(), target.clone()))
                {
                    return;
                }
                let root = said.kind == Kind::Root;
                self.copies.insert((message.clone(), target.clone()));
                self.dead.remove(&(message.clone(), target.clone()));
                self.external
                    .insert((target.clone(), external.clone()), message.clone());
                if let Some(thread) = thread
                    && root
                    && let Some(link) = self.links.get_mut(target)
                    && link.channel
                    && link.thread.is_none()
                {
                    let thread = Target::new(&target.sync, thread.clone());
                    link.thread = Some(thread.clone());
                    self.found(seq, thread, target.clone());
                }
                if let Some(thread) = self.links.get(target).and_then(|link| link.thread.clone()) {
                    self.unpark(seq, &(thread, Some(external.clone())));
                }
            }
            SyncOp::Removed { .. } => {}
            SyncOp::Undelivered {
                task,
                message,
                target,
                ..
            } => {
                let known = self
                    .said
                    .get(message)
                    .is_some_and(|said| said.task == *task && self.routable(said, target));
                let tried = self.attempted.contains(&(message.clone(), target.clone()))
                    || self.rootless(task, target);
                if known && !self.copies.contains(&(message.clone(), target.clone())) && tried {
                    self.dead.insert((message.clone(), target.clone()));
                }
            }
        }
    }

    fn label(
        &mut self,
        task: &TaskId,
        set: &BTreeMap<String, String>,
        unset: &BTreeSet<String>,
        own: &[String],
        version: u64,
    ) {
        let Some((labels, _)) = self.tasks.get(task) else {
            return;
        };
        let held = labels
            .keys()
            .filter(|key| own.iter().any(|prefix| key.starts_with(prefix.as_str())))
            .cloned()
            .collect::<Vec<_>>();
        for prefix in own {
            let seen = self
                .owned
                .entry((task.clone(), prefix.clone()))
                .or_default();
            *seen = (*seen).max(version);
        }
        let floor = |key: &str| {
            self.owned
                .iter()
                .filter(|((at, prefix), _)| at == task && key.starts_with(prefix.as_str()))
                .map(|(_, seen)| *seen)
                .max()
                .unwrap_or_default()
        };
        let floors = set
            .keys()
            .map(|key| (key.clone(), floor(key)))
            .collect::<BTreeMap<_, _>>();
        let mut added = BTreeMap::new();
        let mut removed = BTreeSet::new();
        for (key, value) in set {
            let seen = self.seen.entry((task.clone(), key.clone())).or_default();
            if version >= *seen && version >= floors[key] {
                *seen = version;
                added.insert(key.clone(), value.clone());
            }
        }
        for key in unset.iter().chain(&held) {
            if set.contains_key(key) {
                continue;
            }
            let seen = self.seen.entry((task.clone(), key.clone())).or_default();
            if version >= *seen {
                *seen = version;
                removed.insert(key.clone());
            }
        }
        let labels = &mut self.tasks.get_mut(task).expect("a labelled task exists").0;
        for key in &removed {
            labels.remove(key);
        }
        labels.extend(added);
    }

    fn rootless(&self, task: &TaskId, target: &Target) -> bool {
        self.by_task
            .get(task)
            .and_then(|ids| ids.first())
            .is_some_and(|root| {
                self.said[root].kind == Kind::Root
                    && self.dead.contains(&(root.clone(), target.clone()))
            })
    }

    fn messages(&self, task: &TaskId) -> Vec<&EventId> {
        self.by_task
            .get(task)
            .into_iter()
            .flatten()
            .chain(self.status.get(task))
            .collect()
    }
}

fn accepts(control: &Control, values: &BTreeMap<String, String>) -> bool {
    match control {
        Control::Button { .. } => values.is_empty(),
        Control::Form { fields, .. } => values.keys().all(|key| fields.contains(key)),
    }
}

fn render(model: &Reference, said: &Said, id: &EventId, target: &Target) -> Rendered {
    let mentions = named(&said.body)
        .into_iter()
        .filter(|_| said.kind != Kind::Status)
        .map(|member| {
            let user = model
                .members
                .get(&member)
                .and_then(|accounts| accounts.get(&target.sync))
                .cloned();
            (member, user)
        })
        .collect();
    let delivery = Delivery {
        task: said.task.clone(),
        message: id.clone(),
        kind: said.kind,
        target: target.clone(),
        to: target.clone(),
        sub: None,
        author: said.author.clone(),
        body: said.body.clone(),
        controls: said.controls.clone(),
        mentions,
        revision: 0,
        attempts: 0,
        throttled: 0,
        action: Action::Post { starts: false },
    };
    Plain.render(&delivery)
}

#[allow(clippy::too_many_lines)]
fn check(world: &World, rows: &[(u64, String)], fold: &Fold) -> Result<SyncStats, String> {
    let model = Reference::of(rows)?;
    let mut stats = world.progress.borrow().stats;
    if let Some(wrong) = world.progress.borrow().wrong.first() {
        return Err(wrong.clone());
    }
    if world.progress.borrow().abandoned {
        let left = fold
            .outbox()
            .iter()
            .take(3)
            .map(|delivery| {
                format!(
                    "{} to {} {:?} tries {}",
                    delivery.message, delivery.target, delivery.action, delivery.attempts
                )
            })
            .collect::<Vec<_>>();
        return Err(format!("the outbox never emptied: {left:?}"));
    }
    let observed = fold.tasks().keys().cloned().collect::<BTreeSet<_>>();
    let expected = model.tasks.keys().cloned().collect::<BTreeSet<_>>();
    if observed != expected {
        return Err(format!(
            "board tasks {observed:?} but the log posted {expected:?}"
        ));
    }
    let mut landed: BTreeMap<EventId, Vec<String>> = BTreeMap::new();
    let mut reads: BTreeMap<(TaskId, String), u64> = BTreeMap::new();
    for entry in fold.entries() {
        match &entry.change {
            Change::Replied { by, .. } if !entry.event.0.contains("#ans/") => landed
                .entry(entry.event.clone())
                .or_default()
                .push(by.clone()),
            Change::Synced(Note::Read { member, upto }) => {
                if member == BOT {
                    return Err(format!(
                        "rule 5: the bot marked {} read up to {upto}",
                        entry.task
                    ));
                }
                let held = reads
                    .entry((entry.task.clone(), member.clone()))
                    .or_default();
                if *upto <= *held {
                    return Err(format!(
                        "rule 5: read state of {member} on {} went from {held} to {upto}",
                        entry.task
                    ));
                }
                *held = *upto;
            }
            _ => {}
        }
    }
    for (id, authors) in &landed {
        let Some(writer) = model.imports.get(id) else {
            return Err(format!(
                "rule 1: {id} landed on the board as {authors:?} but should not have"
            ));
        };
        if authors.len() != 1 {
            return Err(format!("rule 1: {id} landed {} times", authors.len()));
        }
        if authors[0] != *writer {
            return Err(format!(
                "rule 6: {id} was written by {} but its author is {writer}",
                authors[0]
            ));
        }
    }
    if let Some(missing) = model.imports.keys().find(|id| !landed.contains_key(*id)) {
        return Err(format!(
            "rule 1: outside message {missing} never landed on the board"
        ));
    }
    if reads != model.read {
        return Err(format!(
            "rule 5: read state {reads:?} but expected {:?}",
            model.read
        ));
    }
    for (task, (labels, _)) in &model.tasks {
        let held = &fold.tasks()[task].spec.labels;
        if held != labels {
            return Err(format!(
                "rule 9: {task} has labels {held:?} but the newest snapshot gives {labels:?}"
            ));
        }
    }
    for (task, (spec, _)) in &model.words {
        let held = &fold.tasks()[task].spec.spec;
        if held != spec {
            return Err(format!(
                "rule 10: {task} reads {held:?} but its newest platform edit gives {spec:?}"
            ));
        }
    }
    let all = model
        .links
        .iter()
        .map(|(key, link)| (key, link, true))
        .chain(model.unlinked.iter().map(|(key, link)| (key, link, false)))
        .collect::<Vec<_>>();
    let mut held: BTreeMap<(EventId, Target), Vec<(usize, fake::Copy)>> = BTreeMap::new();
    let mut lanes: BTreeMap<Target, Vec<EventId>> = BTreeMap::new();
    for (sync, fake) in &world.fakes {
        for (index, copy) in fake.copies().into_iter().enumerate() {
            stats.copies += 1;
            stats.edits += copy.edits;
            stats.deleted += u64::from(copy.deleted);
            let Some(said) = model.said.get(&copy.marker) else {
                return Err(format!(
                    "{sync} holds {} for {} which the board never said",
                    copy.external, copy.marker
                ));
            };
            let key = if copy.to.is_dm() {
                stats.dms += 1;
                Some(copy.to.clone())
            } else {
                all.iter()
                    .find(|(key, link, _)| {
                        link.task == said.task
                            && (copy.to == **key
                                || (link.channel
                                    && (copy.to == link.to
                                        || link.thread.as_ref() == Some(&copy.to))))
                    })
                    .map(|(key, _, _)| (*key).clone())
            };
            let Some(key) = key else {
                return Err(format!(
                    "rule 2: {sync} holds {} of {} at {} which no link of {} reaches",
                    copy.external, copy.marker, copy.to, said.task
                ));
            };
            if !copy.to.is_dm() && said.kind != Kind::Status {
                lanes
                    .entry(key.clone())
                    .or_default()
                    .push(copy.marker.clone());
            }
            held.entry((copy.marker.clone(), key))
                .or_default()
                .push((index, copy));
        }
    }
    for (key, markers) in &lanes {
        for pair in markers.windows(2) {
            if model.said[&pair[0]].order >= model.said[&pair[1]].order {
                return Err(format!(
                    "rule 3: {key} shows {} after {} against board order",
                    pair[1], pair[0]
                ));
            }
        }
    }
    for (sync, fake) in &world.fakes {
        if let Some(early) = fake.lock().early.first() {
            return Err(format!(
                "rule 11: {sync} got {} to {} at {} ms before its Retry-After deadline {} ms",
                early.call, early.to, early.at_ms, early.until_ms
            ));
        }
    }
    if world.shape.storm
        && let Some(((id, key), error)) = fold
            .mirror()
            .dead()
            .iter()
            .find(|(_, error)| *error != "withdrawn")
    {
        return Err(format!(
            "rule 12: {id} was dead-lettered for {key} by 429s alone: {error}"
        ));
    }
    let poison = world
        .shape
        .poison
        .as_ref()
        .map(|key| EventId(format!("{LEAD}#{key}")));
    let reached = held.keys().cloned().collect::<BTreeSet<_>>();
    let mut judge = |id: &EventId,
                     said: &Said,
                     key: &Target,
                     live: bool,
                     now: bool,
                     ever: bool,
                     late: bool|
     -> Result<(), String> {
        let copies = held.remove(&(id.clone(), key.clone())).unwrap_or_default();
        if late {
            if !copies.is_empty() {
                return Err(format!("rule 2: {id} reached {key} after it was unlinked"));
            }
            return Ok(());
        }
        let shown = copies
            .iter()
            .filter(|(_, copy)| !copy.deleted)
            .collect::<Vec<_>>();
        let dead = model.dead.contains(&(id.clone(), key.clone()));
        stats.dead += u64::from(dead);
        if said
            .origin
            .as_ref()
            .is_some_and(|(_, origin)| origin == key)
        {
            if !copies.is_empty() {
                return Err(format!("rule 2: {id} was echoed back to its origin {key}"));
            }
            return Ok(());
        }
        if copies.len() > 1 {
            return Err(format!("rule 2: {id} reached {key} {} times", copies.len()));
        }
        if !live {
            return Ok(());
        }
        if said.deleted {
            if !shown.is_empty() {
                return Err(format!(
                    "rule 4: {id} was deleted on the board but {key} still shows it"
                ));
            }
            return Ok(());
        }
        if now && copies.is_empty() && !dead {
            let root = model.by_task.get(&said.task).and_then(|ids| ids.first());
            let shown = root.map(|root| {
                (
                    model.copies.contains(&(root.clone(), key.clone())),
                    model.dead.contains(&(root.clone(), key.clone())),
                )
            });
            let thread = model.links.get(key).map(|link| link.thread.clone());
            return Err(format!(
                "rule 2: {id} ({:?}) never reached {key} (root copy/dead {shown:?}, thread {thread:?})",
                said.kind
            ));
        }
        if !ever && !copies.is_empty() {
            return Err(format!(
                "rule 7: {id} ({:?}) reached {key} whose mode never allowed it",
                said.kind
            ));
        }
        if poison.as_ref() == Some(id) && now && !dead {
            return Err(format!(
                "rule 10: refused {id} was never dead-lettered for {key}"
            ));
        }
        for (_, copy) in shown {
            let want = render(&model, said, id, key);
            if copy.rendered != want {
                return Err(format!(
                    "rule 4: {key} shows {:?} for {id} but the board says {:?}",
                    copy.rendered.text, want.text
                ));
            }
        }
        Ok(())
    };
    for (key, link, live) in &all {
        let orphaned =
            *live && link.channel && link.thread.is_none() && model.rootless(&link.task, key);
        for id in model.messages(&link.task) {
            let said = &model.said[id];
            if orphaned && said.kind != Kind::Root && !said.deleted && link.now(said) {
                let copied = reached.contains(&(id.clone(), (*key).clone()));
                if copied || !model.dead.contains(&(id.clone(), (*key).clone())) {
                    return Err(format!(
                        "rule 11: {id} ({:?}) on {key} after its root was dead-lettered was {}",
                        said.kind,
                        if copied { "delivered" } else { "left pending" }
                    ));
                }
            }
            let late = !live && link.unlinked.is_some_and(|at| said.seq > at);
            let reached = !link.channel || said.kind == Kind::Root || link.thread.is_some();
            judge(
                id,
                said,
                key,
                *live,
                reached && link.now(said),
                reached && link.ever(said),
                late,
            )?;
        }
    }
    for (id, said) in &model.said {
        for dm in &said.dms {
            judge(id, said, dm, true, true, true, false)?;
        }
    }
    if let Some(((id, key), _)) = held.iter().next() {
        return Err(format!(
            "rule 8: {id} reached {key} which nothing on the board asked for"
        ));
    }
    stats.links = all.len() as u64;
    stats.imports = model.imports.len() as u64;
    stats.reads = model.read.len() as u64;
    stats.parked = fold.mirror().parked() as u64;
    stats.follows = all
        .iter()
        .map(|(_, link, _)| link.states.len() as u64 - 1)
        .sum();
    Ok(stats)
}

pub struct SyncOutcome {
    pub digest: u64,
    pub wrong: Option<String>,
    pub stats: SyncStats,
    pub shape: String,
}

pub fn simulate(seed: u64, mutation: SyncMutation) -> SyncOutcome {
    let shape = Shape::of(seed);
    let described = shape.describe();
    let clock = Clock::new();
    let host = Arc::new(FaultLog {
        inner: MemoryLog::new(),
        rng: Slot::new(Some(Rng::derive(seed, "board sync log faults"))),
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
        row_bytes_max: None,
        oversize: AtomicU64::new(0),
    });
    let fakes = ["github", "linear", "slack"]
        .into_iter()
        .map(|sync| {
            let fake = Fake::with_faults(sync, faults(seed, sync, &shape));
            if let Some(key) = &shape.poison {
                fake.poison(EventId(format!("{LEAD}#{key}")));
            }
            (sync.to_owned(), Arc::new(fake))
        })
        .collect();
    let world = World {
        clock: Arc::clone(&clock),
        host: Arc::clone(&host),
        fakes,
        outside: RefCell::new(Outside::default()),
        progress: RefCell::new(Progress::default()),
        shape,
    };
    board_sim::mutate(mutation.board());
    sync_sim::mutate(mutation.engine());
    // Checkpoints are off by default; the sync's boards keep saving them at the cadence they
    // shipped with, so its reopens still meet them.
    nanocode_board::checkpoint::checkpoint_every_here(Some(
        nanocode_board::checkpoint::CHECKPOINT_EVERY,
    ));
    let mut crashes = world.shape.crashes.clone();
    crashes.sort_unstable();
    let mut number = 0;
    let mut wrong = None;
    loop {
        let crash_at = crashes.get(number as usize).map(|ms| ms * MS);
        let last = crash_at.is_none();
        let deadline = clock.now_ns() + world.shape.deadline_ms * MS;
        let mut executor = Executor::new(
            Arc::clone(&clock),
            Rng::derive(seed, &format!("board sync scheduler {number}")),
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
                wrong = Some(format!(
                    "the sync did not settle within {} ms of the last restart",
                    world.shape.deadline_ms
                ));
                break;
            }
            Ended::Crashed => world.progress.borrow_mut().stats.crashes += 1,
            Ended::Stuck => {
                wrong = Some(format!("epoch {number} got stuck"));
                break;
            }
        }
        number += 1;
    }
    let rows = nanocode_sync::block_on(world.host.inner.scan(SCOPE, 0, i64::MAX as u64))
        .unwrap_or_default();
    let mut fold = Fold::new();
    for (seq, body) in &rows {
        let _ = fold.apply(*seq, body);
    }
    let split = slack_split(seed);
    board_sim::mutate(BoardRule::None);
    sync_sim::mutate(SyncRule::None);
    nanocode_board::checkpoint::checkpoint_every_here(None);
    let mut digest = fnv(FNV_OFFSET, described.as_bytes());
    for (seq, body) in &rows {
        digest = fnv(digest, &seq.to_le_bytes());
        digest = fnv(digest, body.as_bytes());
    }
    for fake in world.fakes.values() {
        for copy in fake.copies() {
            let line = format!(
                "{}|{}|{}|{}|{}",
                copy.to, copy.external, copy.marker, copy.rendered.text, copy.deleted
            );
            digest = fnv(digest, line.as_bytes());
        }
    }
    let verdict = wrong
        .or(split)
        .map_or_else(|| check(&world, &rows, &fold), Err);
    let mut stats = verdict
        .as_ref()
        .copied()
        .unwrap_or_else(|_| world.progress.borrow().stats);
    stats.seeds = 1;
    stats.rows = rows.len() as u64;
    stats.tasks = fold.tasks().len() as u64;
    stats.faults = host.faults.load(Ordering::SeqCst);
    stats.conflicts = host.conflicts.load(Ordering::SeqCst);
    stats.calls = world.fakes.values().map(|fake| fake.lock().calls).sum();
    stats.limited = world.fakes.values().map(|fake| fake.lock().limited).sum();
    stats.abandoned = u64::from(world.progress.borrow().abandoned);
    SyncOutcome {
        digest,
        wrong: verdict.err(),
        stats,
        shape: described,
    }
}

pub fn run_seed(seed: u64, mutation: SyncMutation) -> SyncOutcome {
    std::panic::catch_unwind(|| simulate(seed, mutation)).unwrap_or_else(|panic| {
        board_sim::mutate(BoardRule::None);
        sync_sim::mutate(SyncRule::None);
        nanocode_board::checkpoint::checkpoint_every_here(None);
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default();
        SyncOutcome {
            digest: 0,
            wrong: Some(format!("panic: {message}")),
            stats: SyncStats::default(),
            shape: String::new(),
        }
    })
}

pub fn judge(
    seed: u64,
    mutation: SyncMutation,
    twice: bool,
    verbose: bool,
    totals: &mut SyncStats,
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

pub fn replay(seed: u64, mutation: SyncMutation) -> String {
    let mut command = format!("nanocode-sim --scenario board_sync --seed {seed}");
    if mutation != SyncMutation::None {
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
    mutation: SyncMutation,
    totals: &SyncStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=board_sync mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        SyncMutation::None if failed == 0 => ExitCode::SUCCESS,
        SyncMutation::None => ExitCode::FAILURE,
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

const SPLIT_TEXTS: u64 = 8;
const SPLIT_BREAK_PPM: u64 = 20_000;
const SPLIT_WIDE_PPM: u64 = 100_000;

fn split_text(rng: &mut Rng) -> String {
    let section = slack::SECTION_TEXT_UTF16_MAX as u64;
    let message = slack::MESSAGE_TEXT_UTF16_MAX as u64;
    let lengths = [
        section - 1,
        section,
        section + 1,
        message - 1,
        message,
        message + 1,
        rng.range(1, message * 3),
    ];
    let length = lengths[rng.pick(lengths.len())];
    let mut text = String::new();
    let mut units = 0;
    while units < length {
        let character = if rng.chance(SPLIT_BREAK_PPM) {
            '\n'
        } else if rng.chance(SPLIT_WIDE_PPM) && units + 2 <= length {
            '\u{1F600}'
        } else if rng.chance(SPLIT_WIDE_PPM) {
            '\u{E9}'
        } else {
            'x'
        };
        units += character.len_utf16() as u64;
        text.push(character);
    }
    text
}

fn reassembled(rendered: &Rendered, controls: bool) -> Result<String, String> {
    let messages: Vec<&Rendered> = std::iter::once(rendered).chain(&rendered.rest).collect();
    let mut whole = String::new();
    for (index, message) in messages.iter().enumerate() {
        if index > 0 && !message.rest.is_empty() {
            return Err(format!("Slack message {index} nests more messages"));
        }
        if slack::units(&message.text) > slack::MESSAGE_TEXT_UTF16_MAX {
            return Err(format!(
                "Slack message {index} holds {} text units",
                slack::units(&message.text)
            ));
        }
        let Some(blocks) = &message.blocks else {
            if messages.len() > 1 || slack::units(&message.text) > slack::SECTION_TEXT_UTF16_MAX {
                return Err(format!(
                    "Slack message {index} carries its text without blocks"
                ));
            }
            whole.push_str(&message.text);
            continue;
        };
        let blocks = blocks.as_array().map(Vec::as_slice).unwrap_or_default();
        if blocks.len() > slack::MESSAGE_BLOCKS_MAX {
            return Err(format!(
                "Slack message {index} holds {} blocks",
                blocks.len()
            ));
        }
        let mut sections = String::new();
        for block in blocks {
            if block.pointer("/type").and_then(serde_json::Value::as_str) != Some("section") {
                continue;
            }
            let text = block
                .pointer("/text/text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if slack::units(text) > slack::SECTION_TEXT_UTF16_MAX {
                return Err(format!(
                    "a section of Slack message {index} holds {} units",
                    slack::units(text)
                ));
            }
            sections.push_str(text);
        }
        if sections != message.text {
            return Err(format!(
                "Slack message {index}'s text differs from its sections"
            ));
        }
        whole.push_str(&sections);
    }
    let actions = messages
        .last()
        .and_then(|last| last.blocks.as_ref())
        .is_some_and(|blocks| {
            blocks.as_array().is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block.pointer("/type").and_then(serde_json::Value::as_str) == Some("actions")
                })
            })
        });
    if controls && !actions {
        return Err("the last Slack message lost its controls".to_owned());
    }
    Ok(whole)
}

fn slack_split(seed: u64) -> Option<String> {
    let mut rng = Rng::derive(seed, "slack split");
    for _ in 0..SPLIT_TEXTS {
        let text = split_text(&mut rng);
        let controls = if rng.chance(500_000) {
            vec![Control::Button {
                id: "ok".to_owned(),
                label: "Ok".to_owned(),
            }]
        } else {
            Vec::new()
        };
        let rendered = slack::rendered(&text, &controls);
        match reassembled(&rendered, !controls.is_empty()) {
            Ok(whole) if whole == text => {}
            Ok(whole) => {
                return Some(format!(
                    "a {}-unit text mirrors to Slack as {} units",
                    slack::units(&text),
                    slack::units(&whole)
                ));
            }
            Err(error) => return Some(format!("a {}-unit text: {error}", slack::units(&text))),
        }
    }
    None
}
