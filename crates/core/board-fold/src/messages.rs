use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    Event, Fold, NewTask, Op, Phase, Rejection, TaskId,
    access::{Actor, Can},
    blocks,
    sim::{Mutation, mutated},
    tasks::{TaskOp, resolve},
};

id!(MessageId);

pub const TEXT_CHARS: usize = 10_000;
pub const FILE_BYTES: u64 = 4 * 1024 * 1024;
pub const FILES: usize = 10;
pub const LABEL_KEY: usize = 64;
pub const LABEL_VALUE: usize = 256;
pub const EMOJI: usize = 64;
pub const CHANNEL: usize = 2048;
pub const FILE_NAME: usize = 255;
pub const SCHEMA_NAME: usize = 80;
pub const SCHEMA_VERSION: u64 = 99_999;
pub const SCHEMA_BYTES: usize = 32 * 1024;
pub const SCHEMA_DESCRIPTION: usize = 1_000;
pub const SCHEMAS: usize = 500;
pub const PAGE: usize = 1_000;
pub const TASKS: &str = "tasks";
pub const RESERVED: &str = "board.";
pub const TASK_LABEL: &str = "board.task";
pub const SCHEMAS_CHANNEL: &str = "schemas";
pub const FILES_CHANNEL: &str = "files";
const SHADOWS: [&str; 3] = ["constructor", "__proto__", "prototype"];
const BUILT_INS: [&str; 2] = ["git-commit", "deployment"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attached {
    pub file: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Say {
    Post {
        channel: String,
        re: Option<MessageId>,
        reply_to: Option<MessageId>,
        text: String,
        labels: BTreeMap<String, String>,
        files: Vec<Attached>,
    },
    Edit {
        message: MessageId,
        text: String,
    },
    Delete {
        message: MessageId,
    },
    React {
        message: MessageId,
        emoji: String,
        on: bool,
    },
    Label {
        message: MessageId,
        key: String,
        value: Option<String>,
    },
    Schema {
        name: String,
        version: u64,
        description: String,
        definition: Value,
    },
    File {
        hash: String,
        size: u64,
        media: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub schema: String,
    pub version: u64,
    pub data: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub seq: u64,
    pub version: u64,
    pub by: String,
    pub at_ms: u64,
    pub channel: String,
    pub re: Option<MessageId>,
    pub reply_to: Option<MessageId>,
    pub text: String,
    pub labels: BTreeMap<String, String>,
    pub reactions: BTreeMap<String, BTreeSet<String>>,
    pub files: Vec<Attached>,
    pub blocks: Vec<Block>,
    pub task: bool,
    pub deleted: bool,
    pub edits: u64,
}

impl Message {
    pub fn root(&self) -> &MessageId {
        self.re.as_ref().unwrap_or(&self.id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
    pub version: u64,
    pub description: String,
    pub definition: Value,
    pub by: Option<String>,
    pub seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    pub hash: String,
    pub size: u64,
    pub media: String,
    pub by: String,
    pub seq: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub last: u64,
    pub messages: u64,
    pub threads: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    UnknownMessage(MessageId),
    NotAuthor,
    Deleted(MessageId),
    NotRoot(MessageId),
    OtherThread(MessageId),
    OtherChannel(String),
    Channel(String),
    Text(usize),
    Empty,
    Label(String),
    Reserved(String),
    Emoji(String),
    Files(usize),
    UnknownFile(String),
    FileSize(u64),
    FileName(String),
    FileConflict(String),
    Schema(String),
    SchemaImmutable(String),
    ReservedSchema(String),
    TooManySchemas,
    UnknownSchema(String),
    Block(String),
    TaskRoot(TaskId),
}

impl From<Refusal> for Rejection {
    fn from(refusal: Refusal) -> Self {
        Self::Said(refusal)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Said {
    messages: BTreeMap<MessageId, Box<Message>>,
    order: BTreeSet<(u64, MessageId)>,
    replies: BTreeMap<MessageId, Vec<MessageId>>,
    channels: BTreeMap<String, Channel>,
    #[serde(with = "crate::pairs")]
    schemas: BTreeMap<(String, u64), Schema>,
    files: BTreeMap<String, File>,
}

impl Said {
    pub(crate) fn forget(&mut self, task: &str) {
        let root = MessageId(task.to_owned());
        if !self.messages.get(&root).is_some_and(|message| message.task) {
            return;
        }
        let mut doomed = vec![root];
        while let Some(id) = doomed.pop() {
            doomed.extend(self.replies.remove(&id).unwrap_or_default());
            if let Some(message) = self.messages.remove(&id) {
                self.order.remove(&(message.seq, id));
            }
        }
    }

    /// Messages held.
    pub(crate) fn len(&self) -> usize {
        self.messages.len()
    }

    /// Replies held under task's root message.
    #[cfg(test)]
    pub(crate) fn replies_to(&self, task: &str) -> usize {
        self.replies.get(&MessageId(task.to_owned())).map_or(0, Vec::len)
    }

    /// Keep the newest keep replies under task's root message, dropping older ones and their own
    /// replies (Retain::events).
    pub(crate) fn trim(&mut self, task: &str, keep: usize) {
        let root = MessageId(task.to_owned());
        if !self.messages.get(&root).is_some_and(|message| message.task) {
            return;
        }
        let Some(list) = self.replies.get_mut(&root) else {
            return;
        };
        let over = list.len().saturating_sub(keep);
        let mut doomed = list.drain(..over).collect::<Vec<_>>();
        while let Some(id) = doomed.pop() {
            doomed.extend(self.replies.remove(&id).unwrap_or_default());
            if let Some(message) = self.messages.remove(&id) {
                self.order.remove(&(message.seq, id));
            }
        }
    }

    pub const fn new() -> Self {
        Self {
            messages: BTreeMap::new(),
            order: BTreeSet::new(),
            replies: BTreeMap::new(),
            channels: BTreeMap::new(),
            schemas: BTreeMap::new(),
            files: BTreeMap::new(),
        }
    }

    fn post(&mut self, message: Message) {
        assert!(
            !self.messages.contains_key(&message.id),
            "a message is posted once"
        );
        assert!(
            message.re.is_none() || message.reply_to.is_some(),
            "a reply names its target"
        );
        let channel = self.channels.entry(message.channel.clone()).or_default();
        assert!(channel.last <= message.seq, "a channel grows in log order");
        channel.last = message.seq;
        channel.messages += 1;
        match &message.re {
            Some(root) => self
                .replies
                .entry(root.clone())
                .or_default()
                .push(message.id.clone()),
            None => channel.threads += 1,
        }
        self.order.insert((message.seq, message.id.clone()));
        self.messages.insert(message.id.clone(), Box::new(message));
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub channel: Option<String>,
    pub re: Option<MessageId>,
    pub by: Option<String>,
    pub labels: Vec<(String, Option<String>)>,
    pub after: u64,
    pub limit: usize,
    pub at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View<'a> {
    pub message: &'a Message,
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<'a> {
    pub messages: Vec<View<'a>>,
    pub cursor: u64,
}

fn segment(segment: &str, more: &[u8]) -> bool {
    let mut bytes = segment.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || more.contains(&byte))
}

pub fn channel_ok(path: &str) -> bool {
    path.len() <= CHANNEL && path.split('/').all(|part| segment(part, b"_-"))
}

pub fn key_ok(key: &str) -> bool {
    key.len() <= LABEL_KEY && segment(key, b"_.-") && !SHADOWS.contains(&key)
}

pub fn value_ok(value: &str) -> bool {
    value.chars().count() <= LABEL_VALUE && !value.chars().any(char::is_control)
}

pub fn emoji_ok(emoji: &str) -> bool {
    emoji.len() <= EMOJI && segment(emoji, b"_+-") && !SHADOWS.contains(&emoji)
}

pub fn hash_ok(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn plain(text: &str, most: usize) -> bool {
    !text.is_empty() && text.len() <= most && !text.chars().any(char::is_control)
}

pub fn schema_name_ok(name: &str) -> bool {
    name.len() <= SCHEMA_NAME
        && name
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
        && segment(name, b"._-")
}

pub fn built_in(name: &str, version: u64) -> Option<Value> {
    match (name, version) {
        ("git-commit", 1) => Some(json!({
            "type": "object",
            "required": ["sha", "title"],
            "properties": { "sha": { "type": "string" }, "title": { "type": "string" } },
        })),
        ("deployment", 1) => Some(json!({
            "type": "object",
            "required": ["url"],
            "properties": { "url": { "type": "string", "format": "uri" }, "sha": { "type": "string" } },
        })),
        _ => None,
    }
}

fn text_ok(text: &str, may_be_empty: bool) -> Result<(), Refusal> {
    if mutated(Mutation::TextLimitIgnored) {
        return Ok(());
    }
    let chars = text.chars().count();
    if chars > TEXT_CHARS {
        return Err(Refusal::Text(chars));
    }
    if chars == 0 && !may_be_empty {
        return Err(Refusal::Empty);
    }
    Ok(())
}

fn label_ok(key: &str, value: Option<&str>) -> Result<(), Refusal> {
    if key.starts_with(RESERVED) && !mutated(Mutation::ReservedLabelsWritable) {
        return Err(Refusal::Reserved(key.to_owned()));
    }
    if mutated(Mutation::NamesUnchecked) || (key_ok(key) && value.is_none_or(value_ok)) {
        return Ok(());
    }
    Err(Refusal::Label(key.to_owned()))
}

fn labels_ok(labels: &BTreeMap<String, String>) -> Result<(), Refusal> {
    labels
        .iter()
        .try_for_each(|(key, value)| label_ok(key, Some(value)))
}

impl Fold {
    pub const fn said(&self) -> &Said {
        &self.said
    }

    pub(crate) fn judge(&self, event: &Event) -> Result<(), Rejection> {
        self.admit(&event.by, event.at_ms, &event.op)?;
        self.said_ok(event)
    }

    fn said_ok(&self, event: &Event) -> Result<(), Rejection> {
        match &event.op {
            Op::Post { task } => self.root_ok(task),
            Op::Tasks(TaskOp::Batch { name, tasks }) => resolve(name, tasks)
                .iter()
                .try_for_each(|task| self.root_ok(task)),
            Op::Reply { body, .. } => {
                if self
                    .said
                    .messages
                    .contains_key(&MessageId(event.id.0.clone()))
                {
                    return Err(Rejection::Exists);
                }
                Ok(text_ok(body, true)?)
            }
            Op::Say { say } => self.say_ok(event, say),
            _ => Ok(()),
        }
    }

    fn root_ok(&self, task: &NewTask) -> Result<(), Rejection> {
        labels_ok(&task.labels)?;
        match self.said.messages.get(&MessageId(task.id.0.clone())) {
            Some(root) if root.re.is_some() => Err(Refusal::NotRoot(root.id.clone()).into()),
            Some(root) if root.deleted => Err(Refusal::Deleted(root.id.clone()).into()),
            Some(_) => Ok(()),
            None => Ok(text_ok(&task.spec, true)?),
        }
    }

    fn live(&self, id: &MessageId) -> Result<&Message, Refusal> {
        let message = self
            .said
            .messages
            .get(id)
            .ok_or_else(|| Refusal::UnknownMessage(id.clone()))?;
        if message.deleted && !mutated(Mutation::TombstonesEditable) {
            return Err(Refusal::Deleted(id.clone()));
        }
        Ok(message)
    }

    fn mine(&self, event: &Event, id: &MessageId) -> Result<&Message, Refusal> {
        let message = self.live(id)?;
        if message.by != event.by && !mutated(Mutation::EditsByAnyone) {
            return Err(Refusal::NotAuthor);
        }
        Ok(message)
    }

    pub fn definition(&self, name: &str, version: u64) -> Option<Value> {
        built_in(name, version).or_else(|| {
            self.said
                .schemas
                .get(&(name.to_owned(), version))
                .map(|schema| schema.definition.clone())
        })
    }

    fn blocks(&self, text: &str) -> Result<Vec<Block>, Refusal> {
        blocks::read(text, |name, version| self.definition(name, version))
    }

    fn checked(&self, text: &str) -> Result<(), Refusal> {
        if mutated(Mutation::BlocksUnchecked) {
            return Ok(());
        }
        self.blocks(text).map(drop)
    }

    fn files_ok(&self, files: &[Attached]) -> Result<(), Refusal> {
        if files.len() > FILES && !mutated(Mutation::FileLimitIgnored) {
            return Err(Refusal::Files(files.len()));
        }
        for attached in files {
            if !self.said.files.contains_key(&attached.file)
                && !mutated(Mutation::UnknownFilesAccepted)
            {
                return Err(Refusal::UnknownFile(attached.file.clone()));
            }
            if !mutated(Mutation::NamesUnchecked)
                && (!plain(&attached.name, FILE_NAME) || attached.name.contains('/'))
            {
                return Err(Refusal::FileName(attached.name.clone()));
            }
        }
        Ok(())
    }

    fn thread_ok(
        &self,
        channel: &str,
        re: Option<&MessageId>,
        reply_to: Option<&MessageId>,
    ) -> Result<(), Refusal> {
        let nests = mutated(Mutation::RepliesNest);
        let Some(root) = re else {
            return match reply_to {
                Some(target) => Err(Refusal::OtherThread(target.clone())),
                None => Ok(()),
            };
        };
        let found = self.live(root)?;
        if found.re.is_some() && !nests {
            return Err(Refusal::NotRoot(root.clone()));
        }
        if found.channel != channel && !nests {
            return Err(Refusal::OtherChannel(found.channel.clone()));
        }
        let Some(target) = reply_to else {
            return Err(Refusal::OtherThread(root.clone()));
        };
        let aim = self
            .said
            .messages
            .get(target)
            .ok_or_else(|| Refusal::UnknownMessage(target.clone()))?;
        if aim.root() != root && !nests {
            return Err(Refusal::OtherThread(target.clone()));
        }
        Ok(())
    }

    fn say_ok(&self, event: &Event, say: &Say) -> Result<(), Rejection> {
        let names = !mutated(Mutation::NamesUnchecked);
        match say {
            Say::Post {
                channel,
                re,
                reply_to,
                text,
                labels,
                files,
            } => {
                if self
                    .said
                    .messages
                    .contains_key(&MessageId(event.id.0.clone()))
                    || self.tasks.contains_key(&TaskId(event.id.0.clone()))
                {
                    return Err(Rejection::Exists);
                }
                if names && !channel_ok(channel) {
                    return Err(Refusal::Channel(channel.clone()).into());
                }
                text_ok(text, !files.is_empty())?;
                labels_ok(labels)?;
                self.files_ok(files)?;
                self.thread_ok(channel, re.as_ref(), reply_to.as_ref())?;
                Ok(self.checked(text)?)
            }
            Say::Edit { message, text } => {
                let found = self.mine(event, message)?;
                if found.task && !mutated(Mutation::TaskRootsEditable) {
                    return Err(Refusal::TaskRoot(TaskId(message.0.clone())).into());
                }
                text_ok(text, !found.files.is_empty())?;
                Ok(self.checked(text)?)
            }
            Say::Delete { message } => self.mine(event, message).map(drop).map_err(Rejection::Said),
            Say::React { message, emoji, .. } => {
                self.live(message)?;
                if names && !emoji_ok(emoji) {
                    return Err(Refusal::Emoji(emoji.clone()).into());
                }
                Ok(())
            }
            Say::Label {
                message,
                key,
                value,
            } => {
                self.live(message)?;
                Ok(label_ok(key, value.as_deref())?)
            }
            Say::Schema {
                name,
                version,
                description,
                definition,
            } => Ok(self.schema_ok(name, *version, description, definition)?),
            Say::File { hash, size, media } => {
                if names && (!hash_ok(hash) || !plain(media, FILE_NAME)) {
                    return Err(Refusal::FileName(hash.clone()).into());
                }
                if *size > FILE_BYTES && !mutated(Mutation::FileLimitIgnored) {
                    return Err(Refusal::FileSize(*size).into());
                }
                match self.said.files.get(hash) {
                    Some(file) if file.size != *size => {
                        Err(Refusal::FileConflict(hash.clone()).into())
                    }
                    _ => Ok(()),
                }
            }
        }
    }

    fn schema_ok(
        &self,
        name: &str,
        version: u64,
        description: &str,
        definition: &Value,
    ) -> Result<(), Refusal> {
        let shape = !mutated(Mutation::SchemaShapeIgnored);
        let mutable = mutated(Mutation::SchemasMutable);
        let id = format!("{name}@{version}");
        if shape && (!schema_name_ok(name) || !(1..=SCHEMA_VERSION).contains(&version)) {
            return Err(Refusal::Schema(format!(
                "{id} is not a schema name and version"
            )));
        }
        if BUILT_INS.contains(&name) && !mutable {
            return Err(Refusal::ReservedSchema(name.to_owned()));
        }
        match self.said.schemas.get(&(name.to_owned(), version)) {
            Some(old) if old.definition == *definition && old.description == description => {
                return Ok(());
            }
            Some(_) if !mutable => return Err(Refusal::SchemaImmutable(id)),
            _ => {}
        }
        if !shape {
            return Ok(());
        }
        if self.said.schemas.len() >= SCHEMAS {
            return Err(Refusal::TooManySchemas);
        }
        if description.chars().count() > SCHEMA_DESCRIPTION
            || description.chars().any(char::is_control)
        {
            return Err(Refusal::Schema(format!(
                "{id} has a description that is too long"
            )));
        }
        let bytes = serde_json::to_string(definition).map_or(usize::MAX, |json| json.len());
        if bytes > SCHEMA_BYTES {
            return Err(Refusal::Schema(format!("{id} is {bytes} bytes of JSON")));
        }
        blocks::definition(definition).map_err(|error| Refusal::Schema(format!("{id}: {error}")))
    }

    pub(crate) fn hear(&mut self, seq: u64, event: &Event) {
        let message =
            |id: MessageId, channel: String, re: Option<MessageId>, text: &str, labels, blocks| {
                Message {
                    id,
                    seq,
                    version: seq,
                    by: event.by.clone(),
                    at_ms: event.at_ms,
                    channel,
                    reply_to: re.clone(),
                    re,
                    text: text.to_owned(),
                    labels,
                    reactions: BTreeMap::new(),
                    files: Vec::new(),
                    blocks,
                    task: false,
                    deleted: false,
                    edits: 0,
                }
            };
        match &event.op {
            Op::Post { task } => self.root(task, seq, &message),
            Op::Tasks(TaskOp::Batch { name, tasks }) => {
                for task in resolve(name, tasks) {
                    self.root(&task, seq, &message);
                }
            }
            Op::Reply { task, body } => {
                let root = MessageId(task.0.clone());
                let Some(channel) = self
                    .said
                    .messages
                    .get(&root)
                    .map(|root| root.channel.clone())
                else {
                    return;
                };
                let reply = message(
                    MessageId(event.id.0.clone()),
                    channel,
                    Some(root),
                    body,
                    BTreeMap::new(),
                    Vec::new(),
                );
                self.said.post(reply);
            }
            Op::Say { say } => self.said_apply(seq, event, say, message),
            _ => {}
        }
    }

    fn root(
        &mut self,
        task: &NewTask,
        seq: u64,
        message: &impl Fn(
            MessageId,
            String,
            Option<MessageId>,
            &str,
            BTreeMap<String, String>,
            Vec<Block>,
        ) -> Message,
    ) {
        let id = MessageId(task.id.0.clone());
        if let Some(root) = self.said.messages.get_mut(&id) {
            root.task = true;
            root.version = seq;
            return;
        }
        let channel = task
            .parent
            .as_ref()
            .and_then(|parent| self.said.messages.get(&MessageId(parent.0.clone())))
            .map_or_else(|| TASKS.to_owned(), |parent| parent.channel.clone());
        let mut root = message(
            id,
            channel,
            None,
            &task.spec,
            task.labels.clone(),
            Vec::new(),
        );
        root.task = true;
        self.said.post(root);
    }

    fn said_apply(
        &mut self,
        seq: u64,
        event: &Event,
        say: &Say,
        message: impl Fn(
            MessageId,
            String,
            Option<MessageId>,
            &str,
            BTreeMap<String, String>,
            Vec<Block>,
        ) -> Message,
    ) {
        let blocks = match say {
            Say::Post { text, .. } | Say::Edit { text, .. } => {
                self.blocks(text).unwrap_or_default()
            }
            _ => Vec::new(),
        };
        let target = match say {
            Say::Edit { message, .. }
            | Say::Delete { message }
            | Say::React { message, .. }
            | Say::Label { message, .. } => self.said.messages.get_mut(message),
            _ => None,
        };
        match (say, target) {
            (
                Say::Post {
                    channel,
                    re,
                    reply_to,
                    text,
                    labels,
                    files,
                },
                _,
            ) => {
                let mut posted = message(
                    MessageId(event.id.0.clone()),
                    channel.clone(),
                    re.clone(),
                    text,
                    labels.clone(),
                    blocks,
                );
                if re.is_some() {
                    posted.reply_to.clone_from(reply_to);
                }
                posted.files.clone_from(files);
                self.said.post(posted);
            }
            (Say::Edit { text, .. }, Some(found)) => {
                found.text.clone_from(text);
                found.blocks = blocks;
                found.edits += 1;
                found.version = seq;
            }
            (Say::Delete { message }, Some(found)) => {
                found.deleted = true;
                found.text.clear();
                found.labels.clear();
                found.reactions.clear();
                found.files.clear();
                found.blocks.clear();
                found.version = seq;
                let task = TaskId(message.0.clone());
                if found.task && !mutated(Mutation::DeleteKeepsTask) {
                    self.cancel(seq, event, &task, "deleted".to_owned());
                }
            }
            (Say::React { emoji, on, .. }, Some(found)) => {
                found.version = seq;
                if *on {
                    found
                        .reactions
                        .entry(emoji.clone())
                        .or_default()
                        .insert(event.by.clone());
                    return;
                }
                if mutated(Mutation::ReactionsShared) {
                    found.reactions.remove(emoji);
                    return;
                }
                if let Some(actors) = found.reactions.get_mut(emoji) {
                    actors.remove(&event.by);
                    if actors.is_empty() {
                        found.reactions.remove(emoji);
                    }
                }
            }
            (Say::Label { key, value, .. }, Some(found)) => {
                found.version = seq;
                match value {
                    Some(value) => found.labels.insert(key.clone(), value.clone()),
                    None => found.labels.remove(key),
                };
            }
            (
                Say::Schema {
                    name,
                    version,
                    description,
                    definition,
                },
                _,
            ) => {
                let key = (name.clone(), *version);
                if self.said.schemas.contains_key(&key) && !mutated(Mutation::SchemasMutable) {
                    return;
                }
                self.said.schemas.insert(
                    key,
                    Schema {
                        name: name.clone(),
                        version: *version,
                        description: description.clone(),
                        definition: definition.clone(),
                        by: Some(event.by.clone()),
                        seq,
                    },
                );
            }
            (Say::File { hash, size, media }, _) => {
                self.said.files.entry(hash.clone()).or_insert_with(|| File {
                    hash: hash.clone(),
                    size: *size,
                    media: media.clone(),
                    by: event.by.clone(),
                    seq,
                });
            }
            (
                Say::Edit { .. } | Say::Delete { .. } | Say::React { .. } | Say::Label { .. },
                None,
            ) => {}
        }
    }

    pub fn channel_of<'a>(&'a self, say: &'a Say) -> Option<&'a str> {
        match say {
            Say::Post { channel, .. } => Some(channel),
            Say::Edit { message, .. }
            | Say::Delete { message }
            | Say::React { message, .. }
            | Say::Label { message, .. } => self
                .said
                .messages
                .get(message)
                .map(|found| found.channel.as_str()),
            Say::Schema { .. } | Say::File { .. } => None,
        }
    }

    pub fn message(&self, id: &MessageId) -> Option<&Message> {
        self.said.messages.get(id).map(|message| &**message)
    }

    pub fn messages(&self) -> impl Iterator<Item = &Message> + '_ {
        self.said
            .order
            .iter()
            .map(|(_, id)| &*self.said.messages[id])
    }

    pub fn thread(&self, root: &MessageId) -> Vec<&Message> {
        let Some(found) = self
            .said
            .messages
            .get(root)
            .filter(|found| found.re.is_none())
        else {
            return Vec::new();
        };
        let replies = self.said.replies.get(root).into_iter().flatten();
        std::iter::once(&**found)
            .chain(replies.map(|id| &*self.said.messages[id]))
            .collect()
    }

    pub const fn channels(&self) -> &BTreeMap<String, Channel> {
        &self.said.channels
    }

    pub fn schema(&self, name: &str, version: u64) -> Option<&Schema> {
        self.said.schemas.get(&(name.to_owned(), version))
    }

    pub fn schemas(&self) -> impl Iterator<Item = &Schema> + '_ {
        self.said.schemas.values()
    }

    pub fn file(&self, hash: &str) -> Option<&File> {
        self.said.files.get(hash)
    }

    pub fn task_state(&self, id: &TaskId, at_ms: u64) -> Option<&'static str> {
        let task = self.tasks.get(id)?;
        Some(match &task.phase {
            Phase::Claimed { .. } if mutated(Mutation::TaskLabelStale) => "claimed",
            Phase::Open | Phase::Claimed { .. } => match self.claimable(id, at_ms) {
                Ok(_) => "ready",
                Err(Rejection::Waiting(_)) => "waiting",
                Err(_) => "claimed",
            },
            Phase::Done { .. } => "completed",
            Phase::Failed { .. } => "failed",
            Phase::Cancelled { .. } => "canceled",
            Phase::Blocked { .. } => "blocked",
        })
    }

    pub fn labels(&self, message: &Message, at_ms: u64) -> BTreeMap<String, String> {
        if message.deleted {
            return BTreeMap::new();
        }
        let mut labels = message.labels.clone();
        if message.task
            && let Some(state) = self.task_state(&TaskId(message.id.0.clone()), at_ms)
        {
            labels.insert(TASK_LABEL.to_owned(), state.to_owned());
        }
        labels
    }

    pub fn read(&self, query: &Query) -> Page<'_> {
        self.read_as(None, query)
    }

    pub fn read_as(&self, actor: Option<&Actor>, query: &Query) -> Page<'_> {
        assert!(query.limit > 0, "a page holds 1 to {PAGE} messages");
        assert!(query.limit <= PAGE, "a page holds 1 to {PAGE} messages");
        let from = if mutated(Mutation::CursorInclusive) {
            query.after
        } else {
            query.after.saturating_add(1)
        };
        let mut messages = Vec::new();
        let mut cursor = self.next - 1;
        for (_, id) in self.said.order.range((from, MessageId(String::new()))..) {
            let message = &self.said.messages[id];
            let place =
                query
                    .channel
                    .as_ref()
                    .is_none_or(|channel| match channel.strip_suffix('/') {
                        Some(prefix) => message
                            .channel
                            .strip_prefix(prefix)
                            .is_some_and(|rest| rest.starts_with('/')),
                        None => message.channel == *channel,
                    });
            if !place
                || actor.is_some_and(|actor| !self.hears(actor, message))
                || query.re.as_ref().is_some_and(|root| message.root() != root)
                || query.by.as_ref().is_some_and(|by| message.by != *by)
            {
                continue;
            }
            let labels = self.labels(message, query.at_ms);
            let wanted = query.labels.iter().all(|(key, value)| match value {
                Some(value) => labels.get(key) == Some(value),
                None => labels.contains_key(key),
            });
            if !wanted {
                continue;
            }
            if messages.len() == query.limit {
                let split = messages
                    .iter()
                    .position(|view: &View<'_>| view.message.seq == message.seq);
                if let Some(at) = split.filter(|at| *at > 0) {
                    messages.truncate(at);
                }
                cursor = messages
                    .last()
                    .map_or(cursor, |view: &View<'_>| view.message.seq);
                break;
            }
            messages.push(View { message, labels });
        }
        Page { messages, cursor }
    }

    pub fn changes(&self, after: u64, limit: usize) -> Vec<&Message> {
        assert!(limit > 0, "a page holds 1 to {PAGE} messages");
        assert!(limit <= PAGE, "a page holds 1 to {PAGE} messages");
        let mut changed = self
            .said
            .messages
            .values()
            .map(|message| &**message)
            .filter(|message| message.version > after)
            .collect::<Vec<_>>();
        changed.sort_by_key(|message| message.version);
        changed.truncate(limit);
        changed
    }
}

impl Fold {
    fn home<'a>(&'a self, message: &'a Message) -> &'a str {
        match self.said.messages.get(message.root()) {
            Some(root) if root.task => &root.id.0,
            _ => &message.channel,
        }
    }

    pub fn hears(&self, actor: &Actor, message: &Message) -> bool {
        mutated(Mutation::MessagesLeak) || self.access.reads(actor, self.home(message))
    }

    pub(crate) fn say_allowed(&self, actor: &Actor, say: &Say) -> Result<(), Rejection> {
        if mutated(Mutation::SaysUnchecked) {
            return Ok(());
        }
        let (place, can) = match say {
            Say::Post { channel, re, .. } => {
                let place = re
                    .as_ref()
                    .and_then(|root| self.said.messages.get(root))
                    .map_or(channel.as_str(), |root| self.home(root));
                (place.to_owned(), Can::Post)
            }
            Say::Edit { message, .. }
            | Say::Delete { message }
            | Say::React { message, .. }
            | Say::Label { message, .. } => {
                let found = self
                    .said
                    .messages
                    .get(message)
                    .filter(|found| self.access.reads(actor, self.home(found)))
                    .ok_or_else(|| Refusal::UnknownMessage(message.clone()))?;
                let can = match say {
                    Say::React { .. } => Can::React,
                    Say::Label { .. } => Can::Label,
                    _ => Can::Post,
                };
                (self.home(found).to_owned(), can)
            }
            Say::Schema { name, .. } => (format!("{SCHEMAS_CHANNEL}/{name}"), Can::Post),
            Say::File { .. } => (FILES_CHANNEL.to_owned(), Can::Post),
        };
        self.access.need(actor, &place, Can::Read)?;
        self.access.need(actor, &place, can)
    }

    pub fn heard_by<'a>(&'a self, actor: &'a Actor) -> impl Iterator<Item = &'a Message> + 'a {
        self.messages()
            .filter(move |message| self.hears(actor, message))
    }
}
