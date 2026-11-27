use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use nanocode_board::{
    Board, BoardError, EventId, Fold, MessageId, NewTask, Op, Phase, Rejection, Say, TaskId,
    blocks::block,
    files::{MemoryFiles, hash},
    messages::{Attached, FILE_BYTES, PAGE, Query, Refusal, TASK_LABEL},
    middleware::{Middleware, Verdict, Write},
};
use nanocode_sync::Atom;
use serde_json::{Value, json};

use super::World;
use crate::rng::Rng;

const ALICE: usize = 0;
const BOB: usize = 1;
const CAROL: usize = 2;
pub const WRITERS: [&str; 3] = ["user:alice", "user:bob", "user:carol"];
const TALKERS: usize = 2;
const CHANNELS: [&str; 3] = ["chat/ops", "chat/dev", "notes"];
const PREFIX: &str = "chat/";
const SHOUT: &str = "[shout]";
const DROP: &str = "[drop]";
const FENCE: &str = "\x60\x60\x60json#";
const BAD_PPM: u64 = 700_000;
const READ_PAGE: usize = 4;
pub const HOST_ROW_BYTES: u64 = 8 * 1024 * 1024;
const FIRST_LIMIT_CHARS: u64 = 10_000;
const LIMIT_SPREAD_CHARS: u64 = 2_000;

#[derive(Clone, Debug)]
enum Act {
    Write { who: usize, op: Op },
    Upload { who: usize, bytes: Vec<u8> },
    Limit { chars: Option<u64> },
    Squat,
    Read,
}

#[derive(Clone, Debug)]
enum Expect {
    Any,
    Lands,
    Text { chars: u64, max: u64 },
    Row,
}

#[derive(Clone, Debug)]
struct Step {
    key: String,
    id: String,
    act: Act,
    bad: Option<&'static str>,
    expect: Expect,
}

#[derive(Default)]
struct Heard {
    landed_at_open: BTreeMap<u64, BTreeSet<String>>,
    dropped: BTreeSet<String>,
    refused: BTreeSet<String>,
    wrong: Vec<String>,
    reads: u64,
    squats: u64,
}

pub struct Chat {
    steps: Vec<Step>,
    pub tasks: usize,
    file: Vec<u8>,
    files: MemoryFiles,
    calls: Arc<Atom<Vec<(String, u64)>>>,
    heard: RefCell<Heard>,
}

struct Script {
    rng: Rng,
    steps: Vec<Step>,
    tasks: usize,
}

fn post(channel: &str, re: Option<&MessageId>, reply_to: Option<&MessageId>, text: String) -> Say {
    Say::Post {
        channel: channel.to_owned(),
        re: re.cloned(),
        reply_to: reply_to.cloned(),
        text,
        labels: BTreeMap::new(),
        files: Vec::new(),
    }
}

fn probe() -> Value {
    json!({
        "type": "object",
        "required": ["n"],
        "additionalProperties": false,
        "properties": { "n": { "type": "integer", "minimum": 0 }, "tag": { "enum": ["a", "b"] } },
    })
}

fn chat_task(id: &str) -> NewTask {
    NewTask {
        id: TaskId(id.to_owned()),
        parent: None,
        spec: "chat task".to_owned(),
        labels: BTreeMap::new(),
        needs: Vec::new(),
        terms: nanocode_board::tasks::Terms::default(),
    }
}

impl Script {
    fn push(&mut self, act: Act, bad: Option<&'static str>) -> MessageId {
        self.expect(act, bad, Expect::Any)
    }

    fn expect(&mut self, act: Act, bad: Option<&'static str>, expect: Expect) -> MessageId {
        let key = format!("chat.{}", self.steps.len());
        let by = match &act {
            Act::Write { who, .. } | Act::Upload { who, .. } => WRITERS[*who],
            Act::Limit { .. } => super::access::OWNER,
            Act::Squat | Act::Read => WRITERS[ALICE],
        };
        let id = format!("{by}#{key}");
        self.steps.push(Step {
            key,
            id: id.clone(),
            act,
            bad,
            expect,
        });
        MessageId(id)
    }

    fn text(&mut self, channel: &str, chars: u64, bad: Option<&'static str>, expect: Expect) {
        let text = "x".repeat(usize::try_from(chars).expect("a simulated text fits in memory"));
        let op = Op::Say {
            say: post(channel, None, None, text),
        };
        self.expect(Act::Write { who: BOB, op }, bad, expect);
    }

    fn limits(&mut self, channel: &str) {
        let first = self
            .rng
            .range(FIRST_LIMIT_CHARS, FIRST_LIMIT_CHARS + LIMIT_SPREAD_CHARS);
        let raised = first + self.rng.range(1, LIMIT_SPREAD_CHARS);
        let over = |max: u64| Expect::Text {
            chars: max + 1,
            max,
        };
        self.push(Act::Limit { chars: Some(first) }, None);
        self.text(channel, first, None, Expect::Lands);
        self.text(channel, first + 1, Some("text_limit_ignored"), over(first));
        let unmanaged = Op::TextLimit {
            chars: Some(raised),
        };
        self.push(
            Act::Write {
                who: BOB,
                op: unmanaged,
            },
            Some("text_limit_unmanaged"),
        );
        self.push(
            Act::Limit {
                chars: Some(raised),
            },
            None,
        );
        self.text(channel, first + 1, None, Expect::Lands);
        self.text(channel, raised, None, Expect::Lands);
        self.text(
            channel,
            raised + 1,
            Some("text_limit_ignored"),
            over(raised),
        );
        self.push(Act::Limit { chars: None }, None);
        self.text(channel, raised + 1, None, Expect::Lands);
        self.text(
            channel,
            HOST_ROW_BYTES,
            Some("row_bound_ignored"),
            Expect::Row,
        );
    }

    fn say(&mut self, who: usize, say: Say) -> MessageId {
        self.push(
            Act::Write {
                who,
                op: Op::Say { say },
            },
            None,
        )
    }

    fn bad(&mut self, rule: &'static str, who: usize, say: Say) {
        if self.rng.chance(BAD_PPM) {
            self.push(
                Act::Write {
                    who,
                    op: Op::Say { say },
                },
                Some(rule),
            );
        }
    }

    fn sometimes(&mut self) {
        match self.rng.below(8) {
            0 => {
                self.push(Act::Squat, None);
            }
            1 => {
                self.push(Act::Read, None);
            }
            2 => {
                let say = post(CHANNELS[0], None, None, "not mine to say".to_owned());
                self.push(
                    Act::Write {
                        who: CAROL,
                        op: Op::Say { say },
                    },
                    Some("says_unchecked"),
                );
            }
            _ => {}
        }
    }
}

impl Chat {
    pub fn of(seed: u64) -> Self {
        let mut script = Script {
            rng: Rng::derive(seed, "board chat"),
            steps: Vec::new(),
            tasks: 0,
        };
        let file = format!("log of seed {seed}").into_bytes();
        let file_hash = hash(&file);
        let schema = |definition| Say::Schema {
            name: "probe".to_owned(),
            version: 1,
            description: "a probe".to_owned(),
            definition,
        };
        script.say(ALICE, schema(probe()));
        script.push(
            Act::Upload {
                who: ALICE,
                bytes: file.clone(),
            },
            None,
        );
        let mut roots = Vec::new();
        let mut replies: Vec<(MessageId, MessageId, String)> = Vec::new();
        for index in 0..script.rng.range(6, 14) {
            let channel = CHANNELS[script.rng.pick(CHANNELS.len())];
            let who = script.rng.pick(TALKERS);
            let flavor = script.rng.below(6);
            let mut text = format!("note {index}");
            match flavor {
                0 => text.push_str(&format!(" {SHOUT}")),
                1 => text.push_str(&format!(" {DROP}")),
                2 => text.push_str(&format!(
                    "\n\n{}",
                    block("probe@1", &json!({ "n": index, "tag": "a" }))
                )),
                3 if script.rng.chance(500_000) => text.clear(),
                _ => {}
            }
            let mut say = post(channel, None, None, text);
            if let Say::Post { labels, files, .. } = &mut say {
                if script.rng.chance(500_000) {
                    labels.insert("status".to_owned(), "open".to_owned());
                }
                if flavor == 3 {
                    files.push(Attached {
                        file: file_hash.clone(),
                        name: "log.txt".to_owned(),
                    });
                }
            }
            let root = script.say(who, say);
            if flavor != 1 {
                roots.push((root.clone(), channel.to_owned(), who));
            }
            for _ in 0..script.rng.below(3) {
                let target = replies
                    .iter()
                    .filter(|(_, of, _)| *of == root)
                    .map(|(id, _, _)| id.clone())
                    .next_back()
                    .filter(|_| script.rng.chance(500_000))
                    .unwrap_or_else(|| root.clone());
                let writer = script.rng.pick(TALKERS);
                let reply = script.say(
                    writer,
                    post(channel, Some(&root), Some(&target), format!("re {index}")),
                );
                replies.push((reply, root.clone(), channel.to_owned()));
            }
            script.sometimes();
        }
        for (root, _, who) in roots.clone() {
            match script.rng.below(5) {
                0 => {
                    for writer in [ALICE, BOB] {
                        script.say(
                            writer,
                            Say::React {
                                message: root.clone(),
                                emoji: "eyes".to_owned(),
                                on: true,
                            },
                        );
                    }
                    script.say(
                        BOB,
                        Say::React {
                            message: root,
                            emoji: "eyes".to_owned(),
                            on: false,
                        },
                    );
                }
                1 => {
                    script.say(
                        BOB,
                        Say::Label {
                            message: root.clone(),
                            key: "status".to_owned(),
                            value: Some("done".to_owned()),
                        },
                    );
                    if script.rng.chance(500_000) {
                        script.say(
                            ALICE,
                            Say::Label {
                                message: root,
                                key: "status".to_owned(),
                                value: None,
                            },
                        );
                    }
                }
                2 => {
                    script.say(
                        who,
                        Say::Edit {
                            message: root.clone(),
                            text: format!("edited {SHOUT}"),
                        },
                    );
                    script.bad(
                        "edits_by_anyone",
                        1 - who,
                        Say::Edit {
                            message: root,
                            text: "hijacked".to_owned(),
                        },
                    );
                }
                3 => {
                    script.bad(
                        "edits_by_anyone",
                        1 - who,
                        Say::Delete {
                            message: root.clone(),
                        },
                    );
                    script.say(
                        who,
                        Say::Delete {
                            message: root.clone(),
                        },
                    );
                    script.bad(
                        "tombstones_editable",
                        who,
                        Say::Edit {
                            message: root.clone(),
                            text: "back".to_owned(),
                        },
                    );
                    script.bad(
                        "tombstones_editable",
                        1 - who,
                        Say::React {
                            message: root,
                            emoji: "eyes".to_owned(),
                            on: true,
                        },
                    );
                }
                _ => {}
            }
            script.sometimes();
        }
        let live = roots
            .iter()
            .filter(|(root, _, _)| {
                !script.steps.iter().any(|step| matches!(&step.act, Act::Write { op: Op::Say { say: Say::Delete { message } }, .. } if message == root) && step.bad.is_none())
            })
            .cloned()
            .collect::<Vec<_>>();
        if let Some((root, _, _)) = live.first() {
            script.push(
                Act::Write {
                    who: ALICE,
                    op: Op::Post {
                        task: chat_task(&root.0),
                    },
                },
                None,
            );
            script.tasks += 1;
        }
        for number in 0..script.rng.range(1, 3) {
            let id = format!("chat-task.{number}");
            script.push(
                Act::Write {
                    who: ALICE,
                    op: Op::Post {
                        task: chat_task(&id),
                    },
                },
                None,
            );
            script.tasks += 1;
            let root = MessageId(id);
            script.say(
                BOB,
                post("tasks", Some(&root), Some(&root), "on it".to_owned()),
            );
            script.bad(
                "task_roots_editable",
                ALICE,
                Say::Edit {
                    message: root.clone(),
                    text: "do something else".to_owned(),
                },
            );
            if script.rng.chance(500_000) {
                script.say(ALICE, Say::Delete { message: root });
            }
            script.push(Act::Squat, None);
        }
        let (root, channel) = live.last().map_or_else(
            || {
                (
                    MessageId(format!("{}#chat.0", WRITERS[ALICE])),
                    "notes".to_owned(),
                )
            },
            |(root, channel, _)| (root.clone(), channel.clone()),
        );
        if let Some((reply, of, at)) = replies.first().cloned() {
            script.bad(
                "replies_nest",
                BOB,
                post(&at, Some(&reply), Some(&reply), "nested".to_owned()),
            );
            if of != root {
                script.bad(
                    "replies_nest",
                    BOB,
                    post(&at, Some(&of), Some(&root), "crossed".to_owned()),
                );
            }
        }
        script.bad(
            "replies_nest",
            BOB,
            post(
                "notes/elsewhere",
                Some(&root),
                Some(&root),
                "wrong channel".to_owned(),
            ),
        );
        script.bad(
            "reserved_labels_writable",
            BOB,
            Say::Label {
                message: root.clone(),
                key: TASK_LABEL.to_owned(),
                value: Some("ready".to_owned()),
            },
        );
        let mut reserved = post(&channel, None, None, "reserved".to_owned());
        if let Say::Post { labels, .. } = &mut reserved {
            labels.insert("board.owner".to_owned(), "bob".to_owned());
        }
        script.bad("reserved_labels_writable", BOB, reserved);
        script.bad(
            "names_unchecked",
            BOB,
            Say::Label {
                message: root.clone(),
                key: "Bad Key".to_owned(),
                value: Some("x".to_owned()),
            },
        );
        script.bad(
            "names_unchecked",
            BOB,
            Say::Label {
                message: root.clone(),
                key: "ok".to_owned(),
                value: Some("line\nbreak".to_owned()),
            },
        );
        script.bad(
            "names_unchecked",
            BOB,
            Say::React {
                message: root,
                emoji: "constructor".to_owned(),
                on: true,
            },
        );
        script.bad(
            "names_unchecked",
            BOB,
            post("Chat//Ops", None, None, "bad path".to_owned()),
        );
        let mut unknown = post(&channel, None, None, "see file".to_owned());
        if let Say::Post { files, .. } = &mut unknown {
            files.push(Attached {
                file: "0".repeat(64),
                name: "ghost.txt".to_owned(),
            });
        }
        script.bad("unknown_files_accepted", BOB, unknown);
        script.bad(
            "file_limit_ignored",
            BOB,
            Say::File {
                hash: "ab".repeat(32),
                size: FILE_BYTES + 1,
                media: "video/mp4".to_owned(),
            },
        );
        let mut many = post(&channel, None, None, "many files".to_owned());
        if let Say::Post { files, .. } = &mut many {
            for index in 0..11 {
                files.push(Attached {
                    file: file_hash.clone(),
                    name: format!("copy-{index}.txt"),
                });
            }
        }
        script.bad("file_limit_ignored", BOB, many);
        script.limits(&channel);
        script.bad(
            "text_limit_ignored",
            BOB,
            post(&channel, None, None, String::new()),
        );
        script.bad("schemas_mutable", BOB, schema(json!({ "type": "object" })));
        script.bad(
            "schemas_mutable",
            BOB,
            Say::Schema {
                name: "git-commit".to_owned(),
                version: 2,
                description: String::new(),
                definition: json!({ "type": "object" }),
            },
        );
        script.bad("schema_shape_ignored", BOB, Say::Schema {
            name: "loose".to_owned(),
            version: 1,
            description: String::new(),
            definition: json!({ "type": "object", "properties": { "a": { "type": "string", "pattern": "x" } } }),
        });
        script.bad(
            "schema_shape_ignored",
            BOB,
            Say::Schema {
                name: "Bad Name".to_owned(),
                version: 1,
                description: String::new(),
                definition: json!({ "type": "object" }),
            },
        );
        script.bad(
            "blocks_unchecked",
            BOB,
            post(&channel, None, None, block("probe@1", &json!({ "n": -1 }))),
        );
        script.bad(
            "blocks_unchecked",
            BOB,
            post(&channel, None, None, block("nope@1", &json!({ "n": 1 }))),
        );
        script.bad(
            "blocks_unchecked",
            BOB,
            post(
                &channel,
                None,
                None,
                format!("{FENCE}probe@1\n{{\"n\": 1}}"),
            ),
        );
        script.push(Act::Read, None);
        Self {
            steps: script.steps,
            tasks: script.tasks,
            file,
            files: MemoryFiles::new(),
            calls: Arc::new(Atom::new(Vec::new())),
            heard: RefCell::new(Heard::default()),
        }
    }

    fn wrong(&self, what: String) {
        self.heard.borrow_mut().wrong.push(what);
    }
}

fn layer(calls: Arc<Atom<Vec<(String, u64)>>>, epoch: u64) -> Middleware {
    Arc::new(move |write: &Write<'_>| {
        calls.update(|calls| calls.push((write.id.0.clone(), epoch)));
        let text = write.text().unwrap_or_default();
        if text.contains(DROP) {
            return Verdict::Drop("the probe drops it".to_owned());
        }
        if !text.contains(SHOUT) {
            return Verdict::Pass;
        }
        let labels = match write.op {
            Op::Say {
                say: Say::Post { labels, .. },
            } => {
                let mut labels = labels.clone();
                labels.insert("vetted".to_owned(), "yes".to_owned());
                Some(labels)
            }
            _ => None,
        };
        Verdict::Patch {
            text: Some(text.replace(SHOUT, "SHOUT")),
            labels,
        }
    })
}

fn expected(chat: &Chat, step: &Step, refusal: Option<&BoardError>) {
    let fine = match (&step.expect, refusal) {
        (Expect::Any, _) | (Expect::Lands, None) => true,
        (
            Expect::Text { chars, max },
            Some(BoardError::Rejected(Rejection::Said(Refusal::Text {
                chars: said,
                max: limit,
            }))),
        ) => said == chars && limit == max,
        (Expect::Row, Some(BoardError::Row { bytes, max })) => {
            *max == HOST_ROW_BYTES && bytes > max
        }
        _ => false,
    };
    if !fine {
        chat.wrong(format!(
            "{} expected {:?}, the board answered {refusal:?}",
            step.key, step.expect
        ));
    }
}

fn agree(chat: &Chat, boards: &[Board]) {
    let Some((first, rest)) = boards.split_first() else {
        return;
    };
    for board in rest {
        if board.fold().text_limit() != first.fold().text_limit() {
            chat.wrong(format!(
                "{} reads the text limit {:?}, {} reads {:?}",
                board.writer(),
                board.fold().text_limit(),
                first.writer(),
                first.fold().text_limit()
            ));
        }
        for step in &chat.steps {
            let id = EventId(step.id.clone());
            if let (Some(theirs), Some(ours)) =
                (board.fold().outcome(&id), first.fold().outcome(&id))
                && theirs != ours
            {
                chat.wrong(format!(
                    "{} folds {} as {theirs:?}, {} as {ours:?}",
                    board.writer(),
                    step.id,
                    first.writer()
                ));
            }
        }
    }
}

async fn write(world: &World, board: &mut Board, key: &str, op: &Op) -> Option<BoardError> {
    let chat = &world.chat;
    loop {
        match board.submit_keyed(key, world.now_ms(), op.clone()).await {
            Ok(_) => return None,
            Err(error @ (BoardError::Rejected(_) | BoardError::Row { .. })) => {
                chat.heard
                    .borrow_mut()
                    .refused
                    .insert(format!("{}#{key}", board.writer()));
                return Some(error);
            }
            Err(error @ BoardError::Dropped(_)) => {
                chat.heard
                    .borrow_mut()
                    .dropped
                    .insert(format!("{}#{key}", board.writer()));
                return Some(error);
            }
            Err(error) => {
                world.note(&error);
                world.nap(world.shape.poll_ms).await;
            }
        }
    }
}

fn expected_state(fold: &Fold, task: &TaskId, at_ms: u64) -> Option<&'static str> {
    let now = fold.now_ms().max(at_ms);
    let found = fold.task(task)?;
    Some(match &found.phase {
        Phase::Open
            if found.spec.needs.iter().all(|need| {
                matches!(
                    fold.task(need).map(|need| &need.phase),
                    Some(Phase::Done { .. })
                )
            }) =>
        {
            "ready"
        }
        Phase::Open => "waiting",
        Phase::Blocked { .. } => "blocked",
        Phase::Claimed { until_ms, .. } if *until_ms > now => "claimed",
        Phase::Claimed { .. } => "ready",
        Phase::Done { .. } => "completed",
        Phase::Failed { .. } => "failed",
        Phase::Cancelled { .. } => "canceled",
    })
}

fn labels_check(chat: &Chat, fold: &Fold, at_ms: u64) {
    let mut ready = BTreeSet::new();
    for id in fold.tasks().keys() {
        let Some(root) = fold.message(&MessageId(id.0.clone())) else {
            chat.wrong(format!("task {id} has no root message"));
            continue;
        };
        if root.deleted {
            continue;
        }
        let expect = expected_state(fold, id, at_ms);
        let got = fold.labels(root, at_ms).get(TASK_LABEL).cloned();
        if got.as_deref() != expect {
            chat.wrong(format!(
                "task {id} reads {TASK_LABEL}={got:?}, its state says {expect:?}"
            ));
        }
        if expect == Some("ready") {
            ready.insert(root.id.clone());
        }
    }
    let query = Query {
        labels: vec![(TASK_LABEL.to_owned(), Some("ready".to_owned()))],
        limit: PAGE,
        at_ms,
        ..Query::default()
    };
    let found = fold
        .read(&query)
        .messages
        .iter()
        .map(|view| view.message.id.clone())
        .collect::<BTreeSet<_>>();
    if found != ready {
        chat.wrong(format!(
            "the {TASK_LABEL}=ready filter found {} tasks of {}",
            found.len(),
            ready.len()
        ));
    }
}

fn read(chat: &Chat, fold: &Fold, at_ms: u64) {
    chat.heard.borrow_mut().reads += 1;
    let mut after = 0;
    let mut seen = Vec::new();
    for _ in 0..PAGE {
        let query = Query {
            channel: Some(PREFIX.to_owned()),
            after,
            limit: READ_PAGE,
            at_ms,
            ..Query::default()
        };
        let page = fold.read(&query);
        for view in &page.messages {
            if view.message.seq <= after {
                chat.wrong(format!(
                    "a read after {after} returned {} at {}",
                    view.message.id, view.message.seq
                ));
            }
            seen.push(view.message.id.clone());
        }
        if page.messages.is_empty() || page.cursor == after {
            break;
        }
        after = page.cursor;
    }
    let expect = fold
        .messages()
        .filter(|message| message.channel.starts_with(PREFIX))
        .map(|message| message.id.clone())
        .collect::<Vec<_>>();
    if seen != expect {
        chat.wrong(format!(
            "paging {PREFIX} read {} messages, the channel holds {}",
            seen.len(),
            expect.len()
        ));
    }
    for root in fold.messages().filter(|message| message.re.is_none()) {
        let thread = fold
            .thread(&root.id)
            .iter()
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        let expect = fold
            .messages()
            .filter(|message| message.id == root.id || message.re.as_ref() == Some(&root.id))
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        if thread != expect {
            chat.wrong(format!(
                "thread {} reads {thread:?}, the log holds {expect:?}",
                root.id
            ));
        }
    }
    labels_check(chat, fold, at_ms);
}

pub(super) async fn chatter(world: &World, epoch: u64) {
    let chat = &world.chat;
    let mut boards = Vec::new();
    for writer in WRITERS {
        let mut board = world.open(writer).await;
        board.add_middleware(layer(Arc::clone(&chat.calls), epoch));
        boards.push(board);
    }
    let landed = chat
        .steps
        .iter()
        .filter(|step| match &step.act {
            Act::Write { who, .. } => boards[*who]
                .fold()
                .outcome(&EventId(step.id.clone()))
                .is_some(),
            _ => false,
        })
        .map(|step| step.id.clone())
        .collect();
    chat.heard.borrow_mut().landed_at_open.insert(epoch, landed);
    let grant = super::access::keyed("grant:chat");
    for board in &mut boards[..TALKERS] {
        while board.refresh().await.is_err()
            || !board.fold().outcome(&grant).is_some_and(Result::is_ok)
        {
            world.nap(world.shape.poll_ms).await;
        }
    }
    let mut owner = world.open(super::access::OWNER).await;
    for step in &chat.steps {
        match &step.act {
            Act::Write { who, op } => {
                if let Expect::Text { max, .. } = &step.expect
                    && boards[*who].fold().text_limit() != Some(*max)
                {
                    continue;
                }
                let refusal = write(world, &mut boards[*who], &step.key, op).await;
                expected(chat, step, refusal.as_ref());
            }
            Act::Limit { chars } => {
                let op = Op::TextLimit { chars: *chars };
                let refusal = write(world, &mut owner, &step.key, &op).await;
                expected(chat, step, refusal.as_ref());
                for board in &mut boards {
                    while board.refresh().await.is_err() {
                        world.nap(world.shape.poll_ms).await;
                    }
                }
            }
            Act::Upload { who, bytes } => loop {
                match boards[*who]
                    .upload(&chat.files, world.now_ms(), bytes, "text/plain")
                    .await
                {
                    Ok(_) => break,
                    Err(error) => {
                        world.note(&error);
                        world.nap(world.shape.poll_ms).await;
                    }
                }
            },
            Act::Squat => {
                let board = &mut boards[ALICE];
                if board.refresh().await.is_err() {
                    continue;
                }
                let Some(task) = board.fold().ready(world.now_ms()).next().cloned() else {
                    continue;
                };
                chat.heard.borrow_mut().squats += 1;
                let claim = Op::Claim {
                    task,
                    lease_ms: 1,
                    fence: None,
                    cap: None,
                };
                write(world, board, &step.key, &claim).await;
                world.nap(3).await;
                if board.refresh().await.is_ok() {
                    labels_check(chat, board.fold(), world.now_ms());
                }
            }
            Act::Read => {
                let board = &mut boards[BOB];
                if board.refresh().await.is_ok() {
                    read(chat, board.fold(), world.now_ms());
                }
            }
        }
    }
    for board in &mut boards {
        while board.refresh().await.is_err() {
            world.nap(world.shape.poll_ms).await;
        }
    }
    agree(chat, &boards);
    let board = &mut boards[BOB];
    let file = hash(&chat.file);
    match board.download(&chat.files, &file).await {
        Ok(Some(bytes)) if bytes == chat.file => {}
        other => chat.wrong(format!(
            "downloading {file} gave {:?}",
            other.map(|bytes| bytes.map(|bytes| bytes.len()))
        )),
    }
    if board.heard().next().is_none() {
        chat.wrong(format!("{} hears none of the chat", WRITERS[BOB]));
    }
    let mut outsider = world.open(WRITERS[CAROL]).await;
    while outsider.refresh().await.is_err() {
        world.nap(world.shape.poll_ms).await;
    }
    if let Some(message) = outsider.heard().next() {
        chat.wrong(format!(
            "{} hears {} without a grant",
            WRITERS[CAROL], message.id
        ));
    }
    if let Some(message) = outsider
        .read(&Query {
            limit: PAGE,
            ..Query::default()
        })
        .messages
        .first()
    {
        chat.wrong(format!(
            "{} reads {} without a grant",
            WRITERS[CAROL], message.message.id
        ));
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Kept {
    by: String,
    channel: String,
    re: Option<String>,
    reply_to: Option<String>,
    text: String,
    labels: BTreeMap<String, String>,
    reactions: BTreeMap<String, BTreeSet<String>>,
    files: Vec<Attached>,
    task: bool,
    deleted: bool,
}

fn keep_root(kept: &mut BTreeMap<String, Kept>, task: &NewTask, mut root: Kept) {
    if let Some(found) = kept.get_mut(&task.id.0) {
        found.task = true;
        return;
    }
    root.channel = task
        .parent
        .as_ref()
        .and_then(|parent| kept.get(&parent.0))
        .map_or_else(|| "tasks".to_owned(), |parent| parent.channel.clone());
    root.labels.clone_from(&task.labels);
    root.task = true;
    kept.insert(task.id.0.clone(), root);
}

fn reference(fold: &Fold, events: &[nanocode_board::Event]) -> BTreeMap<String, Kept> {
    let mut kept: BTreeMap<String, Kept> = BTreeMap::new();
    for event in events {
        if !matches!(fold.outcome(&event.id), Some(Ok(_))) {
            continue;
        }
        let fresh = |channel: String, text: &str| Kept {
            by: event.by.clone(),
            channel,
            text: text.to_owned(),
            ..Kept::default()
        };
        match &event.op {
            Op::Post { task } => keep_root(&mut kept, task, fresh(String::new(), &task.spec)),
            Op::Tasks(nanocode_board::tasks::TaskOp::Batch { name, tasks }) => {
                for task in nanocode_board::tasks::resolve(name, tasks) {
                    keep_root(&mut kept, &task, fresh(String::new(), &task.spec));
                }
            }
            Op::Reply { task, body } => {
                let Some(channel) = kept.get(&task.0).map(|root| root.channel.clone()) else {
                    continue;
                };
                let mut reply = fresh(channel, body);
                reply.re = Some(task.0.clone());
                reply.reply_to = Some(task.0.clone());
                kept.insert(event.id.0.clone(), reply);
            }
            Op::Say { say } => match say {
                Say::Post {
                    channel,
                    re,
                    reply_to,
                    text,
                    labels,
                    files,
                } => {
                    let mut message = fresh(channel.clone(), text);
                    message.re = re.as_ref().map(|re| re.0.clone());
                    message.reply_to = reply_to
                        .as_ref()
                        .filter(|_| re.is_some())
                        .map(|target| target.0.clone());
                    message.labels.clone_from(labels);
                    message.files.clone_from(files);
                    kept.insert(event.id.0.clone(), message);
                }
                Say::Edit { message, text } => {
                    if let Some(found) = kept.get_mut(&message.0) {
                        found.text.clone_from(text);
                    }
                }
                Say::Delete { message } => {
                    if let Some(found) = kept.get_mut(&message.0) {
                        found.deleted = true;
                        found.text.clear();
                        found.labels.clear();
                        found.reactions.clear();
                        found.files.clear();
                    }
                }
                Say::React { message, emoji, on } => {
                    if let Some(found) = kept.get_mut(&message.0) {
                        let actors = found.reactions.entry(emoji.clone()).or_default();
                        if *on {
                            actors.insert(event.by.clone());
                        } else {
                            actors.remove(&event.by);
                        }
                        found.reactions.retain(|_, actors| !actors.is_empty());
                    }
                }
                Say::Label {
                    message,
                    key,
                    value,
                } => {
                    if let Some(found) = kept.get_mut(&message.0) {
                        match value {
                            Some(value) => found.labels.insert(key.clone(), value.clone()),
                            None => found.labels.remove(key),
                        };
                    }
                }
                Say::Schema { .. } | Say::File { .. } => {}
            },
            _ => {}
        }
    }
    kept
}

fn refold(world: &World, rows: &[(u64, String)]) -> Fold {
    nanocode_board::sim::mutate(world.mutation.rule());
    let mut live = Fold::new();
    for (seq, body) in rows {
        let _ = live.apply(*seq, body);
    }
    nanocode_board::sim::mutate(nanocode_board::sim::Mutation::None);
    live
}

pub(super) fn check(
    world: &World,
    fold: &Fold,
    rows: &[(u64, String)],
) -> Result<(u64, u64), String> {
    let chat = &world.chat;
    let heard = chat.heard.borrow();
    if let Some(wrong) = heard.wrong.first() {
        return Err(wrong.clone());
    }
    let mut events = Vec::new();
    let mut landed = BTreeMap::new();
    for (seq, body) in rows {
        let event: nanocode_board::Event =
            serde_json::from_str(body).map_err(|error| format!("row {seq}: {error}"))?;
        if nanocode_board::access::owns(&event.by, &event.id)
            && landed.insert(event.id.0.clone(), event.clone()).is_none()
        {
            events.push(event);
        }
    }
    let mut refused = 0;
    for step in &chat.steps {
        let found = landed.get(&step.id);
        if let Some(rule) = step.bad {
            if found.is_some() {
                return Err(format!("{} breaks {rule} but landed", step.id));
            }
            refused += 1;
        }
        let Act::Write { op, .. } = &step.act else {
            continue;
        };
        let text = |op: &Op| match op {
            Op::Say {
                say: Say::Post { text, .. } | Say::Edit { text, .. },
            } => Some(text.clone()),
            _ => None,
        };
        let Some(original) = text(op) else {
            continue;
        };
        if original.contains(DROP) && (found.is_some() || !heard.dropped.contains(&step.id)) {
            return Err(format!(
                "{} should have been dropped by the middleware",
                step.key
            ));
        }
        if original.contains(SHOUT)
            && let Some(event) = found
        {
            let patched = text(&event.op).unwrap_or_default();
            let labelled = match &event.op {
                Op::Say {
                    say: Say::Post { labels, .. },
                } => labels.get("vetted").is_some_and(|value| value == "yes"),
                _ => true,
            };
            if patched.contains(SHOUT) || !patched.contains("SHOUT") || !labelled {
                return Err(format!(
                    "{} landed without the middleware's patch: {patched:?}",
                    step.key
                ));
            }
        }
    }
    let calls = chat.calls.get();
    let mut runs: BTreeMap<&(String, u64), u64> = BTreeMap::new();
    for call in calls.iter() {
        *runs.entry(call).or_default() += 1;
    }
    for ((key, epoch), count) in runs {
        if count > 1 {
            return Err(format!(
                "the middleware ran {count} times for {key} in epoch {epoch}"
            ));
        }
        if heard
            .landed_at_open
            .get(epoch)
            .is_some_and(|landed| landed.contains(key))
        {
            return Err(format!(
                "the middleware ran again for {key}, which had landed before epoch {epoch}"
            ));
        }
    }
    let kept = reference(fold, &events);
    let live = refold(world, rows);
    limits_check(chat, fold, &live)?;
    for (id, task) in fold.tasks() {
        let phase = live.task(id).map(|task| &task.phase);
        if phase != Some(&task.phase) {
            return Err(format!(
                "{id} reads {phase:?} to a live reader, {:?} on a clean refold",
                task.phase
            ));
        }
    }
    let mut said = 0;
    for message in live.messages() {
        said += 1;
        let Some(expect) = kept.get(&message.id.0) else {
            return Err(format!(
                "{} is on the board but not in the reference",
                message.id
            ));
        };
        let actual = Kept {
            by: message.by.clone(),
            channel: message.channel.clone(),
            re: message.re.as_ref().map(|re| re.0.clone()),
            reply_to: message.reply_to.as_ref().map(|target| target.0.clone()),
            text: message.text.clone(),
            labels: message.labels.clone(),
            reactions: message.reactions.clone(),
            files: message.files.clone(),
            task: message.task,
            deleted: message.deleted,
        };
        if actual != *expect {
            return Err(format!(
                "{}: the board holds {actual:?}, the reference {expect:?}",
                message.id
            ));
        }
        if message.blocks.len() != message.text.matches(FENCE).count() {
            return Err(format!(
                "{} carries {} blocks for its text {:?}",
                message.id,
                message.blocks.len(),
                message.text
            ));
        }
    }
    if said != kept.len() as u64 {
        return Err(format!(
            "the board holds {said} messages, the reference {}",
            kept.len()
        ));
    }
    if live
        .file(&hash(&chat.file))
        .is_none_or(|file| file.size != chat.file.len() as u64)
    {
        return Err("the uploaded file is not registered with its size".to_owned());
    }
    Ok((said, refused))
}

fn limits_check(chat: &Chat, fold: &Fold, live: &Fold) -> Result<(), String> {
    if live.text_limit() != fold.text_limit() {
        return Err(format!(
            "a live reader folds the text limit {:?}, a clean refold {:?}",
            live.text_limit(),
            fold.text_limit()
        ));
    }
    let mut last = None;
    for step in &chat.steps {
        let id = EventId(step.id.clone());
        if live.outcome(&id) != fold.outcome(&id) {
            return Err(format!(
                "{} folds as {:?} to a live reader, {:?} on a clean refold",
                step.id,
                live.outcome(&id),
                fold.outcome(&id)
            ));
        }
        if let (Act::Limit { chars }, Some(Ok(_))) = (&step.act, fold.outcome(&id)) {
            last = Some(*chars);
        }
        if matches!(step.expect, Expect::Lands) && !matches!(fold.outcome(&id), Some(Ok(_))) {
            return Err(format!(
                "{} fits the board's text limit but folds as {:?}",
                step.id,
                fold.outcome(&id)
            ));
        }
    }
    if let Some(chars) = last
        && fold.text_limit() != chars
    {
        return Err(format!(
            "the last text limit set {chars:?}, the board reads {:?}",
            fold.text_limit()
        ));
    }
    Ok(())
}
