use std::collections::HashSet;

use nanocode_agent_core::{
    HarnessFamily, SessionCheckpoint,
    session::{ToolOutcome, TranscriptItem},
};
use nanocode_durability::{
    AdmittedTurn, Advanced, Checkpoint, CheckpointFormat, EncodedPayload, Error, FormatFuture,
    Hydrated, PayloadReader, Previous, RenderCheckpoint, Result, Sequence, SessionRecord,
    load_sequence, load_sequence_from, sequence_references_from,
};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, PartialEq, Deserialize)]
pub(crate) struct Prefix {
    #[serde(default)]
    pub(crate) system: Option<std::sync::Arc<Value>>,
    #[serde(default)]
    pub(crate) tools: Vec<crate::ClaudeToolSpec>,
}

static PREFIXES: std::sync::LazyLock<nanocode_sync::AtomMap<String, std::sync::Arc<Prefix>>> =
    std::sync::LazyLock::new(nanocode_sync::AtomMap::default);

pub(crate) fn prefix(key: &str) -> Option<std::sync::Arc<Prefix>> {
    PREFIXES.get(key)
}

pub fn forget_templates() {
    PREFIXES.update(|prefixes| *prefixes = nanocode_sync::Map::default());
}

pub(crate) fn keep_prefix(key: &str, prefix: &std::sync::Arc<Prefix>) {
    if PREFIXES.get(key).is_none() {
        PREFIXES.insert(key.to_owned(), std::sync::Arc::clone(prefix));
    }
}

pub struct ClaudeCheckpointFormat;

#[derive(Deserialize)]
struct PagedConversation {
    version: u32,
    messages: Sequence,
    #[serde(default)]
    code_calls: Option<Sequence>,
    #[serde(default)]
    code_calls_skip: usize,
    #[serde(default)]
    compacted: Option<Sequence>,
    #[serde(default)]
    paged: Option<Sequence>,
    #[serde(default)]
    admitted_base: Option<EncodedPayload>,
    #[serde(default)]
    admitted_tool_ids: Vec<String>,
    #[serde(default)]
    unadmitted_tool_ids: HashSet<String>,
}

#[derive(Deserialize)]
struct AdmittedBase {
    ids: Vec<String>,
}

#[derive(Clone, Default)]
pub(crate) struct HydratedConversation {
    messages: LoadedSequence,
    code_calls: Option<LoadedSequence>,
    compacted: Option<LoadedSequence>,
    compacted_pages: Option<Sequence>,
}

#[derive(Clone, Default)]
struct LoadedSequence {
    references: Vec<EncodedPayload>,
    loaded: Vec<EncodedPayload>,
}

impl HydratedConversation {
    pub(crate) fn messages(&self) -> &[EncodedPayload] {
        &self.messages.loaded
    }

    pub(crate) fn code_calls(&self) -> Option<&[EncodedPayload]> {
        self.code_calls
            .as_ref()
            .map(|sequence| sequence.loaded.as_slice())
    }

    pub(crate) fn compacted(&self) -> Option<&[EncodedPayload]> {
        self.compacted
            .as_ref()
            .map(|sequence| sequence.loaded.as_slice())
    }

    pub(crate) fn whole(&self, head: &Value) -> Result<Value> {
        self.assemble(head)
    }

    fn assemble(&self, head: &Value) -> Result<Value> {
        let decode = |payloads: &[EncodedPayload]| -> Result<Value> {
            payloads
                .iter()
                .map(EncodedPayload::decode::<Value>)
                .collect::<Result<Vec<_>>>()
                .map(Value::Array)
        };
        let mut checkpoint = head.clone();
        let conversation = object(&mut checkpoint, "/conversation")?;
        conversation.insert("messages".into(), decode(self.messages())?);
        if let Some(code_calls) = self.code_calls() {
            conversation.insert("code_calls".into(), decode(code_calls)?);
        }
        if let Some(compacted) = self.compacted() {
            conversation.insert("compacted".into(), decode(compacted)?);
        } else if let Some(pages) = &self.compacted_pages {
            conversation.insert(
                "compacted_pages".into(),
                serde_json::to_value(pages).map_err(Error::InvalidPayload)?,
            );
        }
        Ok(checkpoint)
    }
}

/// A paged checkpoint as loaded: its JSON head (everything but the
/// conversation) and the conversation's still-encoded payloads, which render
/// the whole JSON form only on demand.
#[derive(Clone)]
pub(crate) struct TypedSnapshot {
    pub(crate) head: Value,
    pub(crate) conversation: HydratedConversation,
}

impl RenderCheckpoint for TypedSnapshot {
    fn render(&self) -> Result<Value> {
        self.conversation.assemble(&self.head)
    }
}

fn typed_checkpoint(
    head: Value,
    conversation: Option<HydratedConversation>,
) -> (Checkpoint, Option<Hydrated>) {
    match conversation {
        Some(conversation) => {
            let hydrated = Hydrated::rendering(TypedSnapshot {
                head: head.clone(),
                conversation,
            });
            (Checkpoint::typed(head, hydrated.clone()), Some(hydrated))
        }
        None => (Checkpoint::from(head), None),
    }
}

#[derive(Deserialize)]
struct PagedRecord {
    entries: Vec<(usize, Value, serde_json::Map<String, Value>)>,
}

type PagedMessages = std::collections::BTreeMap<usize, (Value, serde_json::Map<String, Value>)>;

async fn paged_messages(
    reader: PayloadReader<'_>,
    sequence: Option<Sequence>,
    read: &mut HashSet<String>,
) -> Result<PagedMessages> {
    let Some(sequence) = sequence else {
        return Ok(PagedMessages::new());
    };
    let records: Vec<PagedRecord> = load_sequence(reader, sequence, read).await?;
    Ok(records
        .into_iter()
        .flat_map(|record| record.entries)
        .map(|(at, results, message)| (at, (results, message)))
        .collect())
}

fn paged_payload(
    (results, message): &(Value, serde_json::Map<String, Value>),
    original: &EncodedPayload,
) -> Result<EncodedPayload> {
    let mut message = message.clone();
    message.insert("paged".into(), Value::String(original.key().to_owned()));
    message.insert("results".into(), results.clone());
    serde_json::to_string(&message)
        .map(|json| EncodedPayload::from_json(json.into()))
        .map_err(Error::InvalidPayload)
}

#[derive(Default)]
struct Reuse {
    reused: usize,
    decoded: usize,
}

async fn advance_sequence(
    reader: PayloadReader<'_>,
    sequence: Sequence,
    skip: usize,
    read: &mut HashSet<String>,
    kept: Option<LoadedSequence>,
    reuse: &mut Reuse,
    paged: &PagedMessages,
) -> Result<LoadedSequence> {
    let references = sequence_references_from(reader, sequence, skip, read).await?;
    let mut loaded = match kept {
        Some(kept) if kept.references.len() == kept.loaded.len() => {
            let reused = kept
                .references
                .iter()
                .zip(&references)
                .take_while(|(kept, reference)| kept.key() == reference.key())
                .count();
            let mut loaded = kept.loaded;
            loaded.truncate(reused);
            loaded
        }
        _ => Vec::new(),
    };
    let reused = loaded.len();
    reuse.reused += reused;
    reuse.decoded += references.len() - reused;
    if reused < references.len() {
        let wanted = (reused..references.len())
            .filter(|at| !paged.contains_key(at))
            .map(|at| references[at].clone())
            .collect::<Vec<_>>();
        let mut fetched = reader.take_payloads(wanted).await?.into_iter();
        for (at, reference) in references.iter().enumerate().skip(reused) {
            match paged.get(&at) {
                Some(entry) => {
                    read.insert(reference.key().to_owned());
                    loaded.push(paged_payload(entry, reference)?);
                }
                None => loaded.push(fetched.next().ok_or_else(|| {
                    Error::InvalidState("loading a sequence lost a payload".into())
                })?),
            }
        }
    }
    Ok(LoadedSequence { references, loaded })
}

fn tool_use_ids(message: &EncodedPayload) -> Result<Vec<String>> {
    let message: Value = message.decode()?;
    Ok(message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "tool_use")
        .filter_map(|block| block["id"].as_str())
        .map(str::to_owned)
        .collect())
}

impl PagedConversation {
    async fn advance(
        self,
        reader: PayloadReader<'_>,
        conversation: &mut serde_json::Map<String, Value>,
        read: &mut HashSet<String>,
        previous: Option<HydratedConversation>,
        reuse: &mut Reuse,
        lazy: bool,
    ) -> Result<HydratedConversation> {
        let previous = previous.unwrap_or_default();
        let paged = paged_messages(reader, self.paged, read).await?;
        let messages = advance_sequence(
            reader,
            self.messages,
            0,
            read,
            Some(previous.messages),
            reuse,
            &paged,
        )
        .await?;
        let mut hydrated = HydratedConversation {
            messages,
            code_calls: None,
            compacted: None,
            compacted_pages: None,
        };
        if let Some(code_calls) = self.code_calls {
            hydrated.code_calls = Some(
                advance_sequence(
                    reader,
                    code_calls,
                    self.code_calls_skip,
                    read,
                    previous.code_calls,
                    reuse,
                    &PagedMessages::new(),
                )
                .await?,
            );
        }
        if let Some(compacted) = self.compacted {
            let resumable = compacted.count().is_some();
            if resumable {
                hydrated.compacted_pages = Some(compacted.clone());
            }
            if !(lazy && resumable) {
                hydrated.compacted = Some(
                    advance_sequence(
                        reader,
                        compacted,
                        0,
                        read,
                        previous.compacted,
                        reuse,
                        &PagedMessages::new(),
                    )
                    .await?,
                );
            }
        }
        if self.version >= 3 {
            let mut admitted = match self.admitted_base {
                Some(base) => {
                    read.insert(base.key().to_owned());
                    reader
                        .load_payload(base)
                        .await?
                        .decode::<AdmittedBase>()?
                        .ids
                }
                None => Vec::new(),
            };
            admitted.extend(self.admitted_tool_ids);
            for message in hydrated.messages() {
                admitted.extend(
                    tool_use_ids(message)?
                        .into_iter()
                        .filter(|id| !self.unadmitted_tool_ids.contains(id)),
                );
            }
            conversation.insert(
                "admitted_tool_ids".into(),
                Value::Array(admitted.into_iter().map(Value::String).collect()),
            );
        }
        conversation.insert("messages".into(), Value::Array(Vec::new()));
        Ok(hydrated)
    }

    fn restore<'a>(
        self,
        reader: PayloadReader<'a>,
        conversation: &'a mut serde_json::Map<String, Value>,
        read: &'a mut HashSet<String>,
    ) -> crate::agent::AgentFuture<'a, Result<()>> {
        Box::pin(async move {
            let paged = paged_messages(reader, self.paged, read).await?;
            let messages: Vec<Value> = advance_sequence(
                reader,
                self.messages,
                0,
                read,
                None,
                &mut Reuse::default(),
                &paged,
            )
            .await?
            .loaded
            .iter()
            .map(EncodedPayload::decode)
            .collect::<Result<_>>()?;
            if let Some(code_calls) = self.code_calls {
                let rounds =
                    load_sequence_from(reader, code_calls, self.code_calls_skip, read).await?;
                conversation.insert("code_calls".into(), Value::Array(rounds));
            }
            if let Some(compacted) = self.compacted {
                let compacted = load_sequence(reader, compacted, read).await?;
                conversation.insert("compacted".into(), Value::Array(compacted));
            }
            if self.version >= 3 {
                let mut admitted = match self.admitted_base {
                    Some(base) => {
                        read.insert(base.key().to_owned());
                        reader
                            .load_payload(base)
                            .await?
                            .decode::<AdmittedBase>()?
                            .ids
                    }
                    None => Vec::new(),
                };
                admitted.extend(self.admitted_tool_ids);
                admitted.extend(
                    messages
                        .iter()
                        .filter_map(|message| message["content"].as_array())
                        .flatten()
                        .filter(|block| block["type"] == "tool_use")
                        .filter_map(|block| block["id"].as_str())
                        .filter(|id| !self.unadmitted_tool_ids.contains(*id))
                        .map(str::to_owned),
                );
                conversation.insert(
                    "admitted_tool_ids".into(),
                    Value::Array(admitted.into_iter().map(Value::String).collect()),
                );
            }
            conversation.insert("messages".into(), Value::Array(messages));
            Ok(())
        })
    }
}

#[derive(Deserialize)]
struct PagedSnapshot {
    #[serde(default)]
    instructions: Option<EncodedPayload>,
    #[serde(default)]
    template: Option<EncodedPayload>,
    snapshot: Value,
}

#[derive(Deserialize)]
struct PagedCursor {
    pending: Sequence,
    template: EncodedPayload,
    cursor: Value,
}

fn paged(value: &Value) -> bool {
    value
        .get("version")
        .and_then(Value::as_u64)
        .is_some_and(|version| (2..=4).contains(&version))
}

fn object<'a>(
    value: &'a mut Value,
    pointer: &str,
) -> Result<&'a mut serde_json::Map<String, Value>> {
    value
        .pointer_mut(pointer)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| Error::InvalidState(format!("Claude checkpoint has no {pointer}")))
}

fn insert(value: &mut Value, pointer: &str, field: &str, items: Vec<Value>) -> Result<()> {
    object(value, pointer)?.insert(field.into(), Value::Array(items));
    Ok(())
}

pub(super) async fn hydrate_paged(
    reader: PayloadReader<'_>,
    stored: Value,
    read: &mut HashSet<String>,
) -> Result<(Value, Option<HydratedConversation>)> {
    advance_snapshot(reader, stored, read, None, true)
        .await
        .map(|(head, conversation, _)| (head, conversation))
}

fn take_compacted_pages(stored: &mut Value) -> Result<Option<Sequence>> {
    stored
        .pointer_mut("/conversation")
        .and_then(Value::as_object_mut)
        .and_then(|conversation| conversation.remove("compacted_pages"))
        .map(|pages| serde_json::from_value(pages).map_err(Error::InvalidPayload))
        .transpose()
}

fn splice_compacted(stored: &mut Value, mut compacted: Vec<Value>) -> Result<()> {
    let conversation = object(stored, "/conversation")?;
    if let Some(Value::Array(tail)) = conversation.remove("compacted") {
        compacted.extend(tail);
    }
    conversation.insert("compacted".into(), Value::Array(compacted));
    Ok(())
}

async fn materialize_compacted(
    reader: PayloadReader<'_>,
    stored: &mut Value,
    read: &mut HashSet<String>,
) -> Result<()> {
    if let Some(sequence) = take_compacted_pages(stored)? {
        let compacted = load_sequence(reader, sequence, read).await?;
        splice_compacted(stored, compacted)?;
    }
    Ok(())
}

pub(super) fn hydrate_snapshot<'a>(
    reader: PayloadReader<'a>,
    stored: Value,
    read: &'a mut HashSet<String>,
) -> crate::agent::AgentFuture<'a, Result<Value>> {
    Box::pin(async move {
        if !paged(&stored) {
            let mut stored = stored;
            materialize_compacted(reader, &mut stored, read).await?;
            return Ok(stored);
        }
        let conversation: PagedConversation =
            serde_json::from_value(stored.clone()).map_err(Error::InvalidPayload)?;
        let paged: PagedSnapshot = serde_json::from_value(stored).map_err(Error::InvalidPayload)?;
        let mut snapshot = paged.snapshot;
        conversation
            .restore(reader, object(&mut snapshot, "/conversation")?, read)
            .await?;
        if let Some(instructions) = paged.instructions {
            read.insert(instructions.key().to_owned());
            let text: String = reader.load_payload(instructions).await?.decode()?;
            object(&mut snapshot, "/context")?.insert("text".into(), Value::String(text));
        }
        if let Some(template) = paged.template {
            let key = template.key().to_owned();
            read.insert(key.clone());
            if prefix(&key).is_none() {
                let loaded: Prefix = reader.load_payload(template).await?.decode()?;
                keep_prefix(&key, &std::sync::Arc::new(loaded));
            }
            object(&mut snapshot, "/context")?.insert("template".into(), Value::String(key));
        }
        Ok(snapshot)
    })
}

async fn advance_snapshot(
    reader: PayloadReader<'_>,
    stored: Value,
    read: &mut HashSet<String>,
    previous: Option<HydratedConversation>,
    lazy: bool,
) -> Result<(Value, Option<HydratedConversation>, Option<Reuse>)> {
    if !paged(&stored) {
        let mut stored = stored;
        if !lazy {
            materialize_compacted(reader, &mut stored, read).await?;
        }
        return Ok((stored, None, None));
    }
    let conversation: PagedConversation =
        serde_json::from_value(stored.clone()).map_err(Error::InvalidPayload)?;
    let paged: PagedSnapshot = serde_json::from_value(stored).map_err(Error::InvalidPayload)?;
    let mut snapshot = paged.snapshot;
    let mut reuse = Reuse::default();
    let hydrated = conversation
        .advance(
            reader,
            object(&mut snapshot, "/conversation")?,
            read,
            previous,
            &mut reuse,
            lazy,
        )
        .await?;
    load_context(
        reader,
        paged.instructions,
        paged.template,
        &mut snapshot,
        read,
    )
    .await?;
    Ok((snapshot, Some(hydrated), Some(reuse)))
}

async fn load_context(
    reader: PayloadReader<'_>,
    instructions: Option<EncodedPayload>,
    template: Option<EncodedPayload>,
    snapshot: &mut Value,
    read: &mut HashSet<String>,
) -> Result<()> {
    if let Some(instructions) = instructions {
        read.insert(instructions.key().to_owned());
        let text: String = reader.load_payload(instructions).await?.decode()?;
        object(snapshot, "/context")?.insert("text".into(), Value::String(text));
    }
    if let Some(template) = template {
        let key = template.key().to_owned();
        read.insert(key.clone());
        if prefix(&key).is_none() {
            let loaded: Prefix = reader.load_payload(template).await?.decode()?;
            keep_prefix(&key, &std::sync::Arc::new(loaded));
        }
        object(snapshot, "/context")?.insert("template".into(), Value::String(key));
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct DeferredSnapshot {
    head: Value,
    stored: Value,
    messages: bool,
}

impl DeferredSnapshot {
    pub(super) fn into_parts(self) -> (Value, Value, bool) {
        (self.head, self.stored, self.messages)
    }
}

pub(super) fn deferrable(stored: &Value) -> Result<Option<bool>> {
    if !paged(stored) {
        return Ok(None);
    }
    let conversation = PagedConversation::deserialize(stored).map_err(Error::InvalidPayload)?;
    Ok(Some(conversation.messages.count() != Some(0)))
}

pub(super) async fn hydrate_head(
    reader: PayloadReader<'_>,
    stored: &Value,
    read: &mut HashSet<String>,
) -> Result<Value> {
    assert!(
        paged(stored),
        "only a paged checkpoint defers its conversation"
    );
    let conversation = PagedConversation::deserialize(stored).map_err(Error::InvalidPayload)?;
    let paged = PagedSnapshot::deserialize(stored).map_err(Error::InvalidPayload)?;
    let mut snapshot = paged.snapshot;
    let head = object(&mut snapshot, "/conversation")?;
    head.insert("messages".into(), Value::Array(Vec::new()));
    if conversation.version >= 3 {
        let admitted = conversation
            .admitted_tool_ids
            .into_iter()
            .map(Value::String);
        head.insert("admitted_tool_ids".into(), Value::Array(admitted.collect()));
    }
    load_context(
        reader,
        paged.instructions,
        paged.template,
        &mut snapshot,
        read,
    )
    .await?;
    Ok(snapshot)
}

pub(super) async fn hydrate_cursor_paged(
    reader: PayloadReader<'_>,
    stored: Value,
    read: &mut HashSet<String>,
) -> Result<(Value, Option<HydratedConversation>)> {
    if !paged(&stored) {
        return Ok((stored, None));
    }
    let conversation: PagedConversation =
        serde_json::from_value(stored.clone()).map_err(Error::InvalidPayload)?;
    let paged: PagedCursor = serde_json::from_value(stored).map_err(Error::InvalidPayload)?;
    let pending = load_sequence(reader, paged.pending, read).await?;
    let prompt: serde_json::Map<String, Value> =
        reader.load_payload(paged.template).await?.decode()?;
    let mut cursor = paged.cursor;
    let hydrated = conversation
        .advance(
            reader,
            object(&mut cursor, "/snapshot/conversation")?,
            read,
            None,
            &mut Reuse::default(),
            true,
        )
        .await?;
    insert(&mut cursor, "", "pending", pending)?;
    object(&mut cursor, "/template")?.extend(prompt);
    Ok((cursor, Some(hydrated)))
}

impl HydratedConversation {
    pub(super) fn into_pages(self) -> crate::agent::ContinuationPages {
        crate::agent::ContinuationPages {
            messages: self.messages.loaded,
            code_calls: self.code_calls.map(|sequence| sequence.loaded),
            compacted: match self.compacted_pages {
                Some(_) => None,
                None => self.compacted.map(|sequence| sequence.loaded),
            },
            compacted_pages: self.compacted_pages,
        }
    }
}

pub(super) fn assemble_cursor(
    mut head: Value,
    pages: &crate::agent::ContinuationPages,
) -> Result<Value> {
    let decode = |payloads: &[EncodedPayload]| -> Result<Value> {
        payloads
            .iter()
            .map(EncodedPayload::decode::<Value>)
            .collect::<Result<Vec<_>>>()
            .map(Value::Array)
    };
    let conversation = object(&mut head, "/snapshot/conversation")?;
    conversation.insert("messages".into(), decode(&pages.messages)?);
    if let Some(code_calls) = &pages.code_calls {
        conversation.insert("code_calls".into(), decode(code_calls)?);
    }
    if let Some(compacted) = &pages.compacted {
        conversation.insert("compacted".into(), decode(compacted)?);
    }
    if let Some(sequence) = &pages.compacted_pages {
        conversation.insert(
            "compacted_pages".into(),
            serde_json::to_value(sequence).map_err(Error::InvalidPayload)?,
        );
    }
    Ok(head)
}

pub(super) async fn hydrate_cursor(
    reader: PayloadReader<'_>,
    stored: Value,
    read: &mut HashSet<String>,
) -> Result<Value> {
    if !paged(&stored) {
        return Ok(stored);
    }
    let conversation: PagedConversation =
        serde_json::from_value(stored.clone()).map_err(Error::InvalidPayload)?;
    let paged: PagedCursor = serde_json::from_value(stored).map_err(Error::InvalidPayload)?;
    let pending = load_sequence(reader, paged.pending, read).await?;
    let prompt: serde_json::Map<String, Value> =
        reader.load_payload(paged.template).await?.decode()?;
    let mut cursor = paged.cursor;
    conversation
        .restore(reader, object(&mut cursor, "/snapshot/conversation")?, read)
        .await?;
    insert(&mut cursor, "", "pending", pending)?;
    object(&mut cursor, "/template")?.extend(prompt);
    Ok(cursor)
}

async fn record(reader: PayloadReader<'_>, key: &str) -> Result<Value> {
    reader
        .load_payload(EncodedPayload::by_key(key))
        .await?
        .decode()
}

pub(super) async fn hydrate_receipt(
    reader: PayloadReader<'_>,
    mut receipt: Value,
) -> Result<Value> {
    let Some(Value::String(key)) = receipt
        .as_object_mut()
        .and_then(|fields| fields.remove("content_record"))
    else {
        return Ok(receipt);
    };
    let message = record(reader, &key).await?;
    let content = message.get("content").cloned().ok_or_else(|| {
        Error::InvalidState(format!("Claude message record {key} has no content"))
    })?;
    object(&mut receipt, "")?.insert("content".into(), content);
    Ok(receipt)
}

pub(super) async fn hydrate_output(reader: PayloadReader<'_>, mut output: Value) -> Result<Value> {
    let Some(Value::String(key)) = output
        .as_object_mut()
        .and_then(|fields| fields.remove("final_record"))
    else {
        return Ok(output);
    };
    let message = record(reader, &key).await?;
    let text = message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<String>();
    object(&mut output, "")?.insert("final_message".into(), Value::String(text));
    Ok(output)
}

impl CheckpointFormat for ClaudeCheckpointFormat {
    fn family(&self) -> HarnessFamily {
        HarnessFamily::Claude
    }

    fn owns(&self, checkpoint: &Value) -> bool {
        checkpoint.get("provider").and_then(Value::as_str) == Some("claude")
    }

    fn hydrate<'a>(
        &'a self,
        reader: PayloadReader<'a>,
        stored: Value,
    ) -> FormatFuture<'a, Result<Value>> {
        Box::pin(async move { hydrate_snapshot(reader, stored, &mut HashSet::new()).await })
    }

    fn hydrate_noting<'a>(
        &'a self,
        reader: PayloadReader<'a>,
        stored: Value,
        read: &'a mut HashSet<String>,
    ) -> FormatFuture<'a, Result<Value>> {
        Box::pin(hydrate_snapshot(reader, stored, read))
    }

    fn hydrate_deferred<'a>(
        &'a self,
        reader: PayloadReader<'a>,
        stored: &'a Value,
        read: &'a mut HashSet<String>,
    ) -> FormatFuture<'a, Result<Option<Checkpoint>>> {
        Box::pin(async move {
            let Some(messages) = deferrable(stored)? else {
                return Ok(None);
            };
            let head = hydrate_head(reader, stored, read).await?;
            let deferred = DeferredSnapshot {
                head: head.clone(),
                stored: stored.clone(),
                messages,
            };
            Ok(Some(Checkpoint::typed(head, Hydrated::new(deferred))))
        })
    }

    fn hydrate_typed<'a>(
        &'a self,
        reader: PayloadReader<'a>,
        stored: Value,
        read: &'a mut HashSet<String>,
    ) -> FormatFuture<'a, Result<(Checkpoint, Option<Hydrated>)>> {
        Box::pin(async move {
            let (head, conversation, _) =
                advance_snapshot(reader, stored, read, None, false).await?;
            Ok(typed_checkpoint(head, conversation))
        })
    }

    fn advance_typed<'a>(
        &'a self,
        reader: PayloadReader<'a>,
        stored: Value,
        read: &'a mut HashSet<String>,
        previous: Previous,
    ) -> FormatFuture<'a, Result<Advanced>> {
        Box::pin(async move {
            let earlier = previous
                .hydrated
                .and_then(Hydrated::take::<TypedSnapshot>)
                .map(|typed| typed.conversation);
            let (head, conversation, reuse) =
                advance_snapshot(reader, stored, read, earlier, false).await?;
            let (checkpoint, hydrated) = typed_checkpoint(head, conversation);
            Ok(match reuse {
                Some(reuse) if reuse.reused > 0 => Advanced {
                    checkpoint,
                    hydrated,
                    whole: false,
                    reused: reuse.reused,
                    decoded: reuse.decoded,
                },
                Some(reuse) => Advanced {
                    decoded: reuse.decoded,
                    ..Advanced::whole(checkpoint, hydrated)
                },
                None => Advanced::whole(checkpoint, hydrated),
            })
        })
    }

    fn hydrate_cursor<'a>(
        &'a self,
        reader: PayloadReader<'a>,
        continuation: Value,
    ) -> FormatFuture<'a, Result<Option<Value>>> {
        Box::pin(async move {
            hydrate_cursor(reader, continuation, &mut HashSet::new())
                .await
                .map(Some)
        })
    }

    fn hydrate_continuation<'a>(
        &'a self,
        reader: PayloadReader<'a>,
        continuation: Value,
    ) -> FormatFuture<'a, Result<Option<Value>>> {
        Box::pin(async move {
            let mut continuation =
                hydrate_cursor(reader, continuation, &mut HashSet::new()).await?;
            let Some(mut snapshot) = continuation
                .get_mut("snapshot")
                .map(Value::take)
                .filter(|snapshot| self.owns(snapshot))
            else {
                return Ok(None);
            };
            let base = continuation.get("pending_base").and_then(Value::as_u64);
            if let (Some(pending), Some(conversation)) = (
                continuation
                    .get_mut("pending")
                    .and_then(Value::as_array_mut),
                snapshot
                    .get_mut("conversation")
                    .and_then(Value::as_object_mut),
            ) {
                let mut pending = std::mem::take(pending);
                if let Some(base) = base {
                    let prefix = usize::try_from(base)
                        .ok()
                        .and_then(|base| {
                            conversation
                                .get("messages")
                                .and_then(Value::as_array)
                                .and_then(|messages| messages.get(..base))
                        })
                        .ok_or_else(|| {
                            Error::InvalidState(
                                "Claude continuation pending prefix exceeds its conversation"
                                    .to_owned(),
                            )
                        })?;
                    pending.splice(0..0, prefix.iter().cloned());
                }
                if !pending.is_empty() {
                    conversation.insert("messages".into(), Value::Array(pending));
                    conversation.insert("summary".into(), Value::String(String::new()));
                }
            }
            Ok(Some(snapshot))
        })
    }

    fn session_checkpoint(
        &self,
        record: &SessionRecord,
        checkpoint: Value,
    ) -> Result<SessionCheckpoint> {
        crate::session_checkpoint(&record.session_id, record.lineage.clone(), checkpoint)
            .map_err(|error| Error::InvalidState(error.to_string()))
    }

    fn thinking(
        &self,
        _record: &SessionRecord,
        checkpoint: &Value,
    ) -> Result<nanocode_agent_core::Thinking> {
        crate::agent::checkpoint_thinking(checkpoint)
            .map_err(|error| Error::InvalidState(error.to_string()))
    }

    fn transcript(&self, checkpoint: &Value, turns: &[AdmittedTurn<'_>]) -> Vec<TranscriptItem> {
        claude_transcript(checkpoint, &admitted_prompts(turns))
    }

    fn thinking_typed(
        &self,
        record: &SessionRecord,
        head: &Value,
        _hydrated: &Hydrated,
    ) -> Option<Result<nanocode_agent_core::Thinking>> {
        Some(self.thinking(record, head))
    }

    fn transcript_typed(
        &self,
        hydrated: &Hydrated,
        turns: &[AdmittedTurn<'_>],
    ) -> Option<Vec<TranscriptItem>> {
        let checkpoint = hydrated.get::<TypedSnapshot>()?.render().ok()?;
        Some(claude_transcript(&checkpoint, &admitted_prompts(turns)))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PromptPart {
    Text(String),
    Media,
}

struct AdmittedPrompt {
    parts: Vec<PromptPart>,
    steers: usize,
}

fn admitted_prompts(turns: &[AdmittedTurn<'_>]) -> Vec<AdmittedPrompt> {
    turns
        .iter()
        .filter_map(|turn| {
            let input = turn.input;
            if input["provider"] != "claude" || input["kind"] != "prompt" {
                return None;
            }
            let parts = match &input["prompt"]["instruction"] {
                Value::String(text) => vec![PromptPart::Text(text.clone())],
                Value::Array(items) => items
                    .iter()
                    .map(|item| match item["type"].as_str() {
                        Some("text") => {
                            PromptPart::Text(item["text"].as_str().unwrap_or_default().to_owned())
                        }
                        _ => PromptPart::Media,
                    })
                    .collect(),
                _ => return None,
            };
            Some(AdmittedPrompt {
                parts,
                steers: turn.steers,
            })
        })
        .collect()
}
fn blocks(message: &Value) -> impl Iterator<Item = &Value> {
    message["content"].as_array().into_iter().flatten()
}

fn prompt_parts(message: &Value) -> Vec<PromptPart> {
    blocks(message)
        .filter_map(|block| match block["type"].as_str()? {
            "text" => Some(PromptPart::Text(block["text"].as_str()?.to_owned())),
            "image" | "document" => Some(PromptPart::Media),
            _ => None,
        })
        .collect()
}

fn prompt_display(message: &Value) -> String {
    let (mut text, mut images, mut documents) = (String::new(), 0, 0);
    for block in blocks(message) {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
            Some("image") => {
                images += 1;
                text.push_str(&format!("[Image #{images}]"));
            }
            Some("document") => {
                documents += 1;
                text.push_str(&format!("[Document #{documents}]"));
            }
            _ => {}
        }
    }
    text
}

fn recovery_notice(message: &Value, notices: &[&str]) -> bool {
    let mut text = String::new();
    for block in blocks(message) {
        match block["text"].as_str() {
            Some(part) if block["type"] == "text" => text.push_str(part),
            _ => return false,
        }
    }
    notices.contains(&text.as_str())
}

fn tool_output(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item["type"].as_str()? {
                "text" => item["text"].as_str().map(str::to_owned),
                "image" => Some("[image]".to_owned()),
                "document" => Some("[document]".to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn user_rows(messages: &[&Value], notices: &[&str], items: &mut Vec<TranscriptItem>) {
    for message in messages {
        if recovery_notice(message, notices) {
            continue;
        }
        let text = prompt_display(message);
        if !text.is_empty() {
            items.push(TranscriptItem::User(text));
        }
    }
}

fn steer_rows(
    segment: &[&Value],
    steers: usize,
    notices: &[&str],
    items: &mut Vec<TranscriptItem>,
) {
    user_rows(
        &segment[segment.len().saturating_sub(steers)..],
        notices,
        items,
    );
}

fn claude_assistant_rows(message: &Value, items: &mut Vec<TranscriptItem>) {
    for block in blocks(message) {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str() {
                    items.push(TranscriptItem::Assistant(text.into()));
                }
            }

            Some("thinking") => {
                if let Some(text) = block["thinking"]
                    .as_str()
                    .filter(|text| !text.trim().is_empty())
                {
                    items.push(TranscriptItem::Reasoning(text.into()));
                }
            }
            Some("tool_use" | "server_tool_use") => items.push(TranscriptItem::Tool {
                call_id: block["id"].as_str().unwrap_or_default().into(),
                name: block["name"].as_str().unwrap_or_default().into(),
                arguments: block["input"].to_string(),
                parent_call_id: None,
            }),
            Some("mcp_tool_use") => items.push(TranscriptItem::Tool {
                call_id: block["id"].as_str().unwrap_or_default().into(),
                name: format!(
                    "mcp__{}__{}",
                    block["server_name"].as_str().unwrap_or_default(),
                    block["name"].as_str().unwrap_or_default()
                ),
                arguments: block["input"].to_string(),
                parent_call_id: None,
            }),
            _ => {}
        }
    }
}

fn summary_message(message: &Value) -> Option<String> {
    if message["role"] != "user" {
        return None;
    }
    blocks(message).find_map(|block| match block["type"].as_str() {
        Some("compaction") => Some(block["content"].as_str().unwrap_or_default().to_owned()),
        Some("text") => block["text"]
            .as_str()?
            .strip_prefix(crate::agent::COMPACT_CONTINUED)
            .map(|summary| summary.trim_start().to_owned()),
        _ => None,
    })
}

fn claude_transcript(checkpoint: &Value, prompts: &[AdmittedPrompt]) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    let conversation = &checkpoint["conversation"];
    let compacted = conversation["compacted"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let summaries = conversation["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(summary_message)
        .chain(
            conversation["summary"]
                .as_str()
                .filter(|summary| !summary.is_empty())
                .map(str::to_owned),
        );
    if compacted.is_empty() {
        for summary in summaries {
            items.push(TranscriptItem::Assistant(format!(
                "Retained conversation summary:\n{summary}"
            )));
        }
    }
    let notices = conversation["recovery_notices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let messages = compacted
        .iter()
        .chain(conversation["messages"].as_array().into_iter().flatten())
        .filter(|message| summary_message(message).is_none())
        .collect::<Vec<&Value>>();
    let messages = messages.as_slice();
    let receipt = |message: &Value| blocks(message).any(|block| block["type"] == "tool_result");

    let mut children = std::collections::HashMap::<&str, Vec<(&str, &Value)>>::new();
    let mut outcomes = std::collections::HashMap::<&str, &str>::new();
    for round in conversation["code_calls"].as_array().into_iter().flatten() {
        let Some(receipt_id) = round["tool_use_id"].as_str() else {
            continue;
        };
        let origin = round["origin_call_id"].as_str();
        for call in round["calls"].as_array().into_iter().flatten() {
            if let Some(call_id) = call["call_id"].as_str() {
                outcomes.insert(call_id, call["status"].as_str().unwrap_or("unknown"));
                let cell = call["parent_call_id"]
                    .as_str()
                    .or(origin)
                    .unwrap_or(receipt_id);
                children.entry(receipt_id).or_default().push((cell, call));
            }
        }
    }
    let mut replayed_children = std::collections::HashSet::new();
    let mut next_prompt = 0;
    let mut index = 0;
    while let Some(message) = messages.get(index) {
        if message["role"].as_str() == Some("system") {
            index += 1;
            continue;
        }
        if message["role"].as_str() == Some("assistant") {
            claude_assistant_rows(message, &mut items);
            index += 1;
            continue;
        }
        if receipt(message) {
            for block in blocks(message).filter(|block| block["type"] == "tool_result") {
                let parent = block["tool_use_id"].as_str().unwrap_or_default();
                for (cell, call) in children.get(parent).into_iter().flatten() {
                    let call_id = call["call_id"].as_str().unwrap_or_default();
                    if !replayed_children.insert(call_id) {
                        continue;
                    }
                    items.push(TranscriptItem::Tool {
                        call_id: call_id.into(),
                        name: call["name"].as_str().unwrap_or_default().into(),
                        arguments: call["input"].to_string(),
                        parent_call_id: Some((*cell).into()),
                    });
                    let outcome = match outcomes.get(call_id).copied() {
                        Some("completed") => ToolOutcome::Completed,
                        Some("failed") => ToolOutcome::Failed,
                        _ => ToolOutcome::Unknown,
                    };
                    items.push(TranscriptItem::tool_result(call_id, "", outcome));
                }
                items.push(TranscriptItem::tool_result(
                    parent,
                    &tool_output(&block["content"]),
                    if block["is_error"].as_bool() == Some(true) {
                        ToolOutcome::Failed
                    } else {
                        ToolOutcome::Completed
                    },
                ));
            }

            let content = blocks(message)
                .filter(|block| block["type"] != "tool_result")
                .cloned()
                .collect::<Vec<_>>();
            if !content.is_empty() {
                let shared = serde_json::json!({ "content": content });
                user_rows(&[&shared], &notices, &mut items);
            }
            index += 1;
            continue;
        }

        let end = messages[index..]
            .iter()
            .position(|message| message["role"].as_str() != Some("user") || receipt(message))
            .map_or(messages.len(), |offset| index + offset);
        let run = &messages[index..end];
        index = end;
        let mut segment = Vec::new();
        let mut steers = None;
        for message in run {
            let parts = prompt_parts(message);
            let Some(offset) = prompts
                .get(next_prompt..)
                .and_then(|rest| rest.iter().position(|prompt| prompt.parts == parts))
            else {
                segment.push(*message);
                continue;
            };
            match steers {
                Some(steers) => steer_rows(&segment, steers, &notices, &mut items),

                None if offset == 0 => {}
                None => user_rows(&segment, &notices, &mut items),
            }
            segment.clear();
            steers = Some(prompts[next_prompt + offset].steers);
            next_prompt += offset + 1;
            items.push(TranscriptItem::User(prompt_display(message)));
        }

        match steers {
            Some(steers) => steer_rows(&segment, steers, &notices, &mut items),
            None => user_rows(&segment, &notices, &mut items),
        }
    }
    items
}

#[cfg(test)]
mod compacted_tests {
    use super::*;
    use nanocode_durability::SequenceLog;
    use serde_json::json;

    #[test]
    fn a_whole_checkpoint_reads_its_paged_compacted_history_in_full() {
        let persisted = (0..40)
            .map(|index| json!({"index": index}))
            .collect::<Vec<_>>();
        let mut log = SequenceLog::default();
        for item in &persisted {
            log.push(EncodedPayload::encode(item).unwrap()).unwrap();
        }
        let tail = vec![json!({"index": 40}), json!({"index": 41})];
        let mut stored = json!({
            "conversation": {"compacted": tail, "compacted_pages": log.sequence()},
        });
        let sequence = take_compacted_pages(&mut stored).unwrap().unwrap();
        assert!(sequence == log.sequence());
        assert_eq!(sequence.count(), Some(persisted.len()));
        splice_compacted(&mut stored, persisted).unwrap();
        let expected = (0..42)
            .map(|index| json!({"index": index}))
            .collect::<Vec<_>>();
        assert_eq!(stored, json!({"conversation": {"compacted": expected}}));
        let mut plain = json!({"conversation": {"compacted": [json!({"index": 0})]}});
        assert!(take_compacted_pages(&mut plain).unwrap().is_none());
        assert_eq!(
            plain,
            json!({"conversation": {"compacted": [json!({"index": 0})]}})
        );
    }
}
