use crate::{
    ClaudeError, ContentBlock, Encoded, Message, MessageLayout, MessageView, OutputConfig, Role,
    ToolResultContent, WireMessage,
};
use nanocode_durability::EncodedPayload;
use nanocode_sync::SwapOption;

pub(super) type PagedParts<'a> = (&'a [(usize, u64)], Arc<str>);
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    ops::{Deref, DerefMut},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering},
    },
};

#[derive(Clone)]
pub(super) struct HistoryMessage(Arc<Entry>, u64);

static WIRE_IDS: AtomicU64 = AtomicU64::new(0);

const CLEARED_WIRE: u64 = 1 << 63;

fn wire_id() -> u64 {
    WIRE_IDS.fetch_add(1, Ordering::Relaxed)
}

const SAME_WIRE_KNOWN: u8 = 1;
const SAME_WIRE: u8 = 2;
const COMPANIONS_KNOWN: u8 = 4;
const COMPANIONS: u8 = 8;
const STORED: u8 = 16;
const WIRE_CLEARED: u8 = 32;
const IMAGES_UNKNOWN: u32 = u32::MAX;

enum Content {
    Full(EncodedPayload),
    Paged {
        original: Arc<str>,
        cleared: EncodedPayload,
    },
}

impl Content {
    const fn payload(&self) -> &EncodedPayload {
        match self {
            Self::Full(record) => record,
            Self::Paged { cleared, .. } => cleared,
        }
    }

    fn json(&self) -> Option<&str> {
        self.payload().json().ok()
    }

    const fn original(&self) -> Option<&Arc<str>> {
        match self {
            Self::Full(_) => None,
            Self::Paged { original, .. } => Some(original),
        }
    }
}

struct Entry {
    typed: SwapOption<Message>,
    content: SwapOption<Content>,
    results: OnceLock<Box<[(usize, u64)]>>,
    images: AtomicU32,
    role: Role,
    output: Option<OutputConfig>,
    flags: AtomicU8,
}

impl Entry {
    fn new(message: &Message, typed: Option<Arc<Message>>, record: Option<EncodedPayload>) -> Self {
        Self {
            typed: SwapOption::new(typed),
            content: SwapOption::new(record.map(|record| Arc::new(Content::Full(record)))),
            results: OnceLock::new(),
            images: AtomicU32::new(IMAGES_UNKNOWN),
            role: message.role,
            output: message.output_config.clone(),
            flags: AtomicU8::new(0),
        }
    }

    fn content(&self) -> Option<Arc<Content>> {
        self.content.load_full()
    }

    fn typed(&self) -> Arc<Message> {
        if let Some(message) = self.typed.load_full() {
            return message;
        }
        let content = self
            .content()
            .expect("a message keeps its typed form or its bytes");
        let json = content.json().expect("a message's bytes are resolved");
        Arc::new(serde_json::from_str(json).expect("a recorded message parses"))
    }

    fn original(&self) -> Option<Arc<str>> {
        self.content()
            .and_then(|content| content.original().map(Arc::clone))
    }

    fn flag(&self, known: u8, set: u8, check: impl FnOnce() -> bool) -> bool {
        let flags = self.flags.load(Ordering::Acquire);
        if flags & known != 0 {
            return flags & set != 0;
        }
        let value = check();
        self.flags
            .fetch_or(known | if value { set } else { 0 }, Ordering::AcqRel);
        value
    }

    fn page(&self) {
        let Some(content) = self.content() else {
            return;
        };
        let Content::Full(record) = &*content else {
            return;
        };
        let Some(results) = self.results.get() else {
            return;
        };
        if self.role != Role::User || results.is_empty() {
            return;
        }
        let mut cleared = Message::clone(&self.typed());
        if results.len() != cleared.content.len() {
            return;
        }
        for block in &mut cleared.content {
            match block {
                ContentBlock::ToolResult { content, .. } => {
                    *content = ToolResultContent::Text(super::clearing::CLEARED.to_owned());
                }
                _ => return,
            }
        }
        let json = serde_json::to_string(&cleared).expect("a cleared message serializes");
        let paged = Content::Paged {
            original: Arc::from(record.key()),
            cleared: EncodedPayload::from_json(json.into()),
        };
        self.content
            .compare_and_swap(&content, Some(Arc::new(paged)));
        self.typed.store(None);
    }
}

impl Clone for Entry {
    fn clone(&self) -> Self {
        let typed = self.typed();
        Self::new(&typed, Some(Arc::clone(&typed)), None)
    }
}

pub(super) struct MessageMut<'a> {
    entry: &'a mut Entry,
    message: Option<Message>,
}

impl Deref for MessageMut<'_> {
    type Target = Message;

    fn deref(&self) -> &Message {
        self.message.as_ref().expect("a message being changed")
    }
}

impl DerefMut for MessageMut<'_> {
    fn deref_mut(&mut self) -> &mut Message {
        self.message.as_mut().expect("a message being changed")
    }
}

impl Drop for MessageMut<'_> {
    fn drop(&mut self) {
        if let Some(message) = self.message.take() {
            *self.entry = Entry::new(&message, None, None);
            self.entry.typed = SwapOption::new(Some(Arc::new(message)));
        }
    }
}

#[derive(Serialize)]
struct PagedOut<'a> {
    role: Role,
    content: &'a [ContentBlock],
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<&'a OutputConfig>,
    paged: &'a str,
    results: &'a [(usize, u64)],
}

#[derive(Deserialize)]
struct StoredMessage {
    role: Role,
    content: Vec<ContentBlock>,
    #[serde(default)]
    output_config: Option<OutputConfig>,
    #[serde(default)]
    paged: Option<Arc<str>>,
    #[serde(default)]
    results: Option<Box<[(usize, u64)]>>,
}

impl HistoryMessage {
    pub(super) const fn wire(&self) -> u64 {
        self.1
    }

    pub(super) fn make_mut(&mut self) -> MessageMut<'_> {
        let message = Arc::unwrap_or_clone(self.typed());
        self.1 = wire_id();
        let entry = Arc::make_mut(&mut self.0);
        *entry = Entry::new(&message, None, None);
        MessageMut {
            entry,
            message: Some(message),
        }
    }

    pub(super) fn role(&self) -> Role {
        self.0.role
    }

    const fn has_cleared_bit(&self) -> bool {
        self.1 & CLEARED_WIRE != 0
    }

    fn is_cleared(&self) -> bool {
        self.has_cleared_bit() && self.0.original().is_none()
    }

    pub(super) fn paged_original(&self) -> Option<Arc<str>> {
        self.0.original()
    }

    pub(super) fn paged_parts(&self) -> Option<PagedParts<'_>> {
        let content = self.0.content()?;
        let Content::Paged { cleared, .. } = &*content else {
            return None;
        };
        let results = self.0.results.get()?;
        Some((results, cleared.shared_json().ok()?))
    }

    pub(super) fn reaches_wire_uncleared(&self) -> bool {
        !self.has_cleared_bit() && self.0.original().is_some()
    }

    fn typed(&self) -> Arc<Message> {
        let typed = self.0.typed();
        if !self.is_cleared() {
            return typed;
        }
        let mut message = Message::clone(&typed);
        for &(block, _) in self.0.results.get().map_or(&[][..], |results| &results[..]) {
            if let Some(ContentBlock::ToolResult { content, .. }) = message.content.get_mut(block) {
                *content = ToolResultContent::Text(super::clearing::CLEARED.to_owned());
            }
        }
        Arc::new(message)
    }

    pub(super) fn view(&self) -> MessageView<'static> {
        MessageView::Shared(self.typed())
    }

    pub(super) fn any_block(&self, check: impl FnMut(&ContentBlock) -> bool) -> bool {
        self.typed().content.iter().any(check)
    }

    pub(super) fn is_effort_update(&self) -> bool {
        self.0.role == Role::System && self.0.output.is_some()
    }

    pub(super) fn output_config(&self) -> Option<&OutputConfig> {
        self.0.output.as_ref()
    }

    pub(super) fn mentions(&self, text: &str) -> bool {
        if self.is_cleared() || self.0.typed.load().is_some() {
            return true;
        }
        let Some(content) = self.0.content() else {
            return true;
        };
        let Some(json) = content.json() else {
            return true;
        };
        match serde_json::to_string(text) {
            Ok(escaped) => json.contains(&escaped[1..escaped.len() - 1]),
            Err(_) => true,
        }
    }

    fn settled(message: Message) -> Self {
        match serde_json::to_string(&message) {
            Ok(json) => Self(
                Arc::new(Entry::new(
                    &message,
                    None,
                    Some(EncodedPayload::from_json(json.into())),
                )),
                wire_id(),
            ),
            Err(_) => Self::from(message),
        }
    }

    pub(super) fn loaded(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str::<Message>(json).map(Self::settled)
    }

    fn stored(stored: StoredMessage) -> std::result::Result<Self, String> {
        let message = Message {
            role: stored.role,
            content: stored.content,
            output_config: stored.output_config,
        };
        let Some(original) = stored.paged else {
            return Ok(Self::settled(message));
        };
        let results = stored
            .results
            .ok_or("a paged message lists its cleared results")?;
        if !EncodedPayload::is_key(&original) {
            return Err(format!(
                "a paged message names {original:?}, which is not a record key"
            ));
        }
        let json = serde_json::to_string(&message).map_err(|error| error.to_string())?;
        let entry = Entry::new(&message, None, None);
        entry.content.store(Some(Arc::new(Content::Paged {
            original,
            cleared: EncodedPayload::from_json(json.into()),
        })));
        entry
            .results
            .set(results)
            .expect("a new entry has no results yet");
        entry.flags.store(STORED | WIRE_CLEARED, Ordering::Release);
        Ok(Self(Arc::new(entry), wire_id()))
    }

    pub(super) fn cleared(&self) -> Self {
        Self(Arc::clone(&self.0), self.1 | CLEARED_WIRE)
    }

    pub(super) fn mark_wire_cleared(&self) {
        if !self.has_cleared_bit() {
            return;
        }
        let flags = self.0.flags.fetch_or(WIRE_CLEARED, Ordering::AcqRel);
        if flags & (STORED | WIRE_CLEARED) == STORED {
            self.0.page();
        }
    }

    pub(super) fn image_count(&self, count: impl FnOnce(&Self) -> usize) -> usize {
        if self.is_cleared() {
            return count(self);
        }
        let images = self.0.images.load(Ordering::Acquire);
        if images != IMAGES_UNKNOWN {
            return images as usize;
        }
        let images = count(self);
        if let Ok(known) = u32::try_from(images)
            && known != IMAGES_UNKNOWN
        {
            self.0.images.store(known, Ordering::Release);
        }
        images
    }

    pub(super) fn result_tokens(
        &self,
        measure: impl Fn(&crate::ContentBlock) -> Option<u64>,
    ) -> &[(usize, u64)] {
        self.0.results.get_or_init(|| {
            self.0
                .typed()
                .content
                .iter()
                .enumerate()
                .filter_map(|(block, content)| measure(content).map(|tokens| (block, tokens)))
                .collect()
        })
    }

    pub(super) fn has_companions(&self, check: impl FnOnce(&Message) -> bool) -> bool {
        if self.is_cleared() {
            return check(&self.typed());
        }
        self.0
            .flag(COMPANIONS_KNOWN, COMPANIONS, || check(&self.0.typed()))
    }

    pub(super) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) && self.has_cleared_bit() == other.has_cleared_bit()
    }

    pub(super) fn into_message(self) -> Message {
        if self.is_cleared() {
            return Arc::unwrap_or_clone(self.typed());
        }
        match Arc::try_unwrap(self.0) {
            Ok(entry) => Arc::unwrap_or_clone(entry.typed()),
            Err(entry) => Message::clone(&entry.typed()),
        }
    }

    pub(super) fn record(&self) -> serde_json::Result<EncodedPayload> {
        if self.is_cleared() {
            return Ok(EncodedPayload::from_json(
                serde_json::to_string(&*self.typed())?.into(),
            ));
        }
        let held = self.0.content();
        match held.as_deref() {
            Some(Content::Full(record)) => return Ok(record.clone()),
            Some(Content::Paged { original, .. }) => return Ok(EncodedPayload::from_key(original)),
            None => {}
        }
        let record = EncodedPayload::from_json(serde_json::to_string(&*self.0.typed())?.into());
        let previous = self
            .0
            .content
            .compare_and_swap(&held, Some(Arc::new(Content::Full(record.clone()))));
        match previous.as_deref() {
            None => Ok(record),
            Some(_) => self.record(),
        }
    }

    fn json(&self) -> serde_json::Result<Arc<Content>> {
        if let Some(content) = self.0.content() {
            return Ok(content);
        }
        self.record()?;
        Ok(self
            .0
            .content()
            .expect("a recorded message keeps its bytes"))
    }

    pub(super) fn with_json<R>(&self, read: impl FnOnce(&str) -> R) -> serde_json::Result<R> {
        if self.is_cleared() {
            return Ok(read(&serde_json::to_string(&*self.typed())?));
        }
        let content = self.json()?;
        Ok(read(
            content.json().expect("a message's bytes are resolved"),
        ))
    }

    pub(super) fn committed(&self) {
        if self.has_cleared_bit() || self.0.content().is_none() {
            return;
        }
        self.0.typed.store(None);
        let flags = self.0.flags.fetch_or(STORED, Ordering::AcqRel);
        if flags & WIRE_CLEARED != 0 {
            self.0.page();
        }
    }

    fn durable_key(&self) -> Option<Arc<str>> {
        if let Some(original) = self.0.original() {
            return Some(original);
        }
        self.record().ok().map(|record| Arc::from(record.key()))
    }
}

impl From<Message> for HistoryMessage {
    fn from(message: Message) -> Self {
        let typed = Arc::new(message);
        Self(
            Arc::new(Entry::new(&typed, Some(Arc::clone(&typed)), None)),
            wire_id(),
        )
    }
}

impl PartialEq for HistoryMessage {
    fn eq(&self, other: &Self) -> bool {
        if self.ptr_eq(other) {
            return true;
        }
        if self.is_cleared() || other.is_cleared() {
            return *self.typed() == *other.typed();
        }
        if self.0.original().is_some() || other.0.original().is_some() {
            return self.durable_key().is_some() && self.durable_key() == other.durable_key();
        }
        if let (Some(left), Some(right)) = (self.0.content(), other.0.content())
            && let (Some(left), Some(right)) = (left.json(), right.json())
        {
            return left == right;
        }
        *self.typed() == *other.typed()
    }
}

impl Serialize for HistoryMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.is_cleared() {
            return self.typed().serialize(serializer);
        }
        let Some(content) = self.0.content() else {
            return self.typed().serialize(serializer);
        };
        match &*content {
            Content::Full(record) => match record.json() {
                Ok(json) => serde_json::from_str::<&serde_json::value::RawValue>(json)
                    .map_err(serde::ser::Error::custom)?
                    .serialize(serializer),
                Err(_) => self.typed().serialize(serializer),
            },
            Content::Paged { original, .. } => {
                let typed = self.0.typed();
                PagedOut {
                    role: typed.role,
                    content: &typed.content,
                    output_config: typed.output_config.as_ref(),
                    paged: original,
                    results: self.0.results.get().map_or(&[][..], |results| &results[..]),
                }
                .serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for HistoryMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::stored(StoredMessage::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl WireMessage for HistoryMessage {
    fn message(&self) -> MessageView<'_> {
        self.view()
    }

    fn wire_id(&self) -> Option<u64> {
        Some(self.1)
    }

    fn layout(&self) -> MessageLayout {
        <Message as WireMessage>::layout(&self.typed())
    }

    fn encoded<'a>(
        &'a self,
        subscription: bool,
        encode: &dyn Fn(&Message) -> Result<String, ClaudeError>,
    ) -> Result<Encoded<'a>, ClaudeError> {
        assert!(
            !self.reaches_wire_uncleared(),
            "a paged message reached the wire without its clearing"
        );
        if self.is_cleared() {
            let typed = self.typed();
            return match subscription {
                true => encode(&typed).map(Encoded::Owned),
                false => Ok(Encoded::Owned(serde_json::to_string(&*typed)?)),
            };
        }
        let content = self.json()?;
        let payload = content.payload();
        let shared = || {
            payload
                .shared_json()
                .map_err(|error| ClaudeError::Protocol(error.to_string()))
        };
        if !subscription {
            return shared().map(Encoded::Shared);
        }
        if content.original().is_some() {
            return encode(&self.0.typed()).map(Encoded::Owned);
        }
        let json = content.json().expect("a message's bytes are resolved");
        let mut wire = None;
        let same = self.0.flag(SAME_WIRE_KNOWN, SAME_WIRE, || {
            let encoded = encode(&self.0.typed());
            let same = encoded.as_deref().is_ok_and(|encoded| encoded == json);
            wire = Some(encoded);
            same
        });
        if same {
            return shared().map(Encoded::Shared);
        }
        match wire {
            Some(wire) => wire.map(Encoded::Owned),
            None => crate::subscription_wire::encode_record(json).map(Encoded::Owned),
        }
    }
}

#[derive(Clone)]
pub(super) struct CodeRound(Arc<Round>);

struct Round {
    record: EncodedPayload,
    bytes: usize,
}

impl CodeRound {
    pub(super) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub(super) fn record(&self) -> serde_json::Result<EncodedPayload> {
        Ok(self.0.record.clone())
    }

    pub(super) fn bytes(&self) -> usize {
        self.0.bytes
    }

    fn json(&self) -> &str {
        self.0.record.json().expect("a code round keeps its bytes")
    }
}

impl From<serde_json::Value> for CodeRound {
    fn from(value: serde_json::Value) -> Self {
        let json = serde_json::to_string(&value).expect("a JSON value serializes");
        Self(Arc::new(Round {
            bytes: super::bounded_json_len(&value, usize::MAX),
            record: EncodedPayload::from_json(json.into()),
        }))
    }
}

impl PartialEq for CodeRound {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.json() == other.json()
    }
}

impl Serialize for CodeRound {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde_json::from_str::<&serde_json::value::RawValue>(self.json())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CodeRound {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde_json::Value::deserialize(deserializer).map(Self::from)
    }
}

static ADMITTED_EPOCH: AtomicU64 = AtomicU64::new(1);

pub(super) struct AdmittedIds {
    order: nanocode_sync::Vector<Arc<str>>,
    positions: Option<nanocode_sync::Map<Arc<str>, usize>>,
    epoch: u64,
}

impl Default for AdmittedIds {
    fn default() -> Self {
        Self {
            order: nanocode_sync::Vector::new(),
            positions: None,
            epoch: ADMITTED_EPOCH.fetch_add(1, Ordering::Relaxed),
        }
    }
}

impl Clone for AdmittedIds {
    fn clone(&self) -> Self {
        Self {
            order: self.order.clone(),
            positions: self.positions.clone(),
            epoch: ADMITTED_EPOCH.fetch_add(1, Ordering::Relaxed),
        }
    }
}

impl PartialEq for AdmittedIds {
    fn eq(&self, other: &Self) -> bool {
        self.order.len() == other.order.len() && self.order.iter().all(|id| other.contains(id))
    }
}

impl AdmittedIds {
    pub(super) fn contains(&self, id: &str) -> bool {
        self.position(id).is_some()
    }

    pub(super) fn position(&self, id: &str) -> Option<usize> {
        self.positions.as_ref()?.get(id).copied()
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub(super) const fn len(&self) -> usize {
        self.order.len()
    }

    pub(super) const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(super) fn since(&self, seen: usize) -> impl Iterator<Item = &Arc<str>> {
        (seen..self.order.len()).map(|at| &self.order[at])
    }

    pub(super) fn insert(&mut self, id: Arc<str>) {
        if !self.contains(&id) {
            self.positions
                .get_or_insert_default()
                .insert(Arc::clone(&id), self.order.len());
            self.order.push_back(id);
        }
    }
}

impl Extend<String> for AdmittedIds {
    fn extend<I: IntoIterator<Item = String>>(&mut self, ids: I) {
        for id in ids {
            self.insert(id.into());
        }
    }
}

impl Extend<Arc<str>> for AdmittedIds {
    fn extend<I: IntoIterator<Item = Arc<str>>>(&mut self, ids: I) {
        for id in ids {
            self.insert(id);
        }
    }
}

impl IntoIterator for AdmittedIds {
    type Item = Arc<str>;
    type IntoIter = <nanocode_sync::Vector<Arc<str>> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.order.into_iter()
    }
}

impl Serialize for AdmittedIds {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(&self.order)
    }
}

impl<'de> Deserialize<'de> for AdmittedIds {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut ids = Self::default();
        ids.extend(Vec::<String>::deserialize(deserializer)?);
        Ok(ids)
    }
}

pub(super) fn media_placeholders(messages: &mut [HistoryMessage]) {
    let placeholder = |kind: &str| match kind {
        "image" => Some("[image]"),
        "document" => Some("[document]"),
        _ => None,
    };
    let media = |block: &ContentBlock| match block {
        ContentBlock::Image { .. } | ContentBlock::Document { .. } => true,
        ContentBlock::ToolResult {
            content: crate::ToolResultContent::Blocks(blocks),
            ..
        } => blocks
            .iter()
            .any(|block| block["type"].as_str().and_then(placeholder).is_some()),
        _ => false,
    };
    for message in messages {
        if !(message.mentions("image") || message.mentions("document"))
            || !message.any_block(&media)
        {
            continue;
        }
        for block in &mut message.make_mut().content {
            match block {
                ContentBlock::Image { .. } => *block = ContentBlock::text("[image]"),
                ContentBlock::Document { .. } => *block = ContentBlock::text("[document]"),
                ContentBlock::ToolResult {
                    content: crate::ToolResultContent::Blocks(blocks),
                    ..
                } => {
                    for block in blocks {
                        if let Some(text) = block["type"].as_str().and_then(placeholder) {
                            *block = serde_json::json!({"type": "text", "text": text});
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

pub(super) fn strip_thinking(messages: &mut Vec<HistoryMessage>) {
    messages.retain_mut(|message| {
        if message.mentions("thinking")
            && message.any_block(|block| {
                matches!(
                    block,
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
                )
            })
        {
            message.make_mut().content.retain(|block| {
                !matches!(
                    block,
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
                )
            });
        }
        !message.view().content.is_empty() || message.is_effort_update()
    });
}
