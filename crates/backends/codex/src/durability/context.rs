use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use nanocode_agent_core::TurnUsage;
use nanocode_durability::{
    EncodedPayload, Error, PagedWriter, PayloadReader, Prepared, Result, Sequence, SequenceLog,
    load_sequence, sequence_references,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

use crate::{
    client::responses::{ContentItem, MessageRole, ResponseHistory, ResponseItem},
    execution::{ExecutionContinuation, ExecutionOutput},
    session::{SessionSnapshot, SessionSnapshotHead},
};

type Fields = BTreeMap<String, Box<RawValue>>;

fn fields(json: &str) -> Result<Fields> {
    serde_json::from_str(json).map_err(Error::InvalidPayload)
}

fn raw<T: Serialize + ?Sized>(value: &T) -> Result<Box<RawValue>> {
    serde_json::value::to_raw_value(value).map_err(Error::InvalidPayload)
}

pub(crate) fn prepare_receipt(json: &str) -> Result<EncodedPayload> {
    let mut receipt = fields(json)?;
    let Some(mut response) = receipt
        .get("response")
        .map(|response| fields(response.get()))
        .transpose()?
    else {
        return Ok(EncodedPayload::from_json(json.into()));
    };
    let Some(items) = response.remove("output_items") else {
        return Ok(EncodedPayload::from_json(json.into()));
    };
    let items: Vec<Box<RawValue>> =
        serde_json::from_str(items.get()).map_err(Error::InvalidPayload)?;
    let records = items
        .iter()
        .map(|item| EncodedPayload::from_json(item.get().into()))
        .collect::<Vec<_>>();
    let keys = records
        .iter()
        .map(EncodedPayload::reference)
        .collect::<Vec<_>>();
    response.insert("output_records".into(), raw(&keys)?);
    receipt.insert("response".into(), raw(&response)?);
    Ok(EncodedPayload::encode(&receipt)?.with_payloads(records))
}

pub(crate) async fn restore_receipt(reader: PayloadReader<'_>, json: String) -> Result<String> {
    let mut receipt = fields(&json)?;
    let Some(mut response) = receipt
        .get("response")
        .map(|response| fields(response.get()))
        .transpose()?
    else {
        return Ok(json);
    };
    let Some(keys) = response.remove("output_records") else {
        return Ok(json);
    };
    let keys: Vec<EncodedPayload> =
        serde_json::from_str(keys.get()).map_err(Error::InvalidPayload)?;
    let items = reader
        .load_payloads(keys)
        .await?
        .iter()
        .map(|item| item.json().map(str::to_owned))
        .collect::<Result<Vec<_>>>()?;
    let items =
        RawValue::from_string(format!("[{}]", items.join(","))).map_err(Error::InvalidPayload)?;
    response.insert("output_items".into(), items);
    receipt.insert("response".into(), raw(&response)?);
    serde_json::to_string(&receipt).map_err(Error::InvalidPayload)
}

fn assistant_text(item: &ResponseItem) -> Option<String> {
    let ResponseItem::Message {
        role: MessageRole::Assistant,
        content,
        ..
    } = item
    else {
        return None;
    };
    Some(
        content
            .iter()
            .filter_map(|part| match part {
                ContentItem::OutputText { text, .. } => Some(&**text),
                _ => None,
            })
            .collect(),
    )
}

#[derive(Serialize)]
struct ReferencedOutput<'a> {
    final_record: EncodedPayload,
    usage: &'a TurnUsage,
}

fn completion_output(history: &ResponseHistory, output: &ExecutionOutput) -> Result<Box<RawValue>> {
    let referenced = history
        .iter_rev()
        .next()
        .filter(|item| {
            !output.final_message.is_empty()
                && assistant_text(item).as_deref() == Some(output.final_message.as_str())
        })
        .map(EncodedPayload::encode)
        .transpose()?;
    match referenced {
        Some(record) => raw(&ReferencedOutput {
            final_record: record.reference(),
            usage: &output.usage,
        }),
        None => raw(output),
    }
}

pub(crate) async fn restore_output(
    reader: PayloadReader<'_>,
    mut output: Value,
) -> Result<ExecutionOutput> {
    if let Some(Value::String(key)) = output
        .as_object_mut()
        .and_then(|fields| fields.remove("final_record"))
    {
        let item: ResponseItem = reader
            .load_payload(EncodedPayload::by_key(&key))
            .await?
            .decode()?;
        let text = assistant_text(&item).ok_or_else(|| {
            Error::InvalidState(format!(
                "Codex output record {key} is not an assistant message"
            ))
        })?;
        output["final_message"] = Value::String(text);
    }
    serde_json::from_value(output).map_err(Error::InvalidPayload)
}

#[derive(Deserialize, Serialize)]
struct Continuation {
    state: Box<RawValue>,
    history: Sequence,
    prefix: Sequence,
}

pub(crate) fn prepare_continuation(
    value: ExecutionContinuation,
    known: &HashSet<String>,
) -> Result<Prepared> {
    let mut writer = PagedWriter::new(known);
    let history = writer.sequence(&value.history)?;
    let prefix = writer.sequence(&value.prefix)?;
    let state = RawValue::from_string(value.state_json)
        .map_err(nanocode_durability::Error::InvalidPayload)?;
    writer.finish(&Continuation {
        state,
        history,
        prefix,
    })
}

fn loaded_history(
    base: &ResponseHistory,
    log: &mut SequenceLog,
    payloads: Vec<EncodedPayload>,
) -> Result<ResponseHistory> {
    let mut entries = Vec::with_capacity(payloads.len());
    let keep = log.len();
    for payload in payloads {
        log.push(payload.reference())?;
        entries.push(payload.decode::<ResponseItem>()?);
    }
    log.release();
    let history = if keep == 0 {
        ResponseHistory::loaded(entries)
    } else {
        base.extended_loaded(keep, entries)
    };
    Ok(history)
}

#[derive(Clone)]
pub(crate) struct HydratedSnapshot {
    pub(crate) snapshot: SessionSnapshot,
    history: SequenceLog,
    prefix: Option<Vec<EncodedPayload>>,
}

impl HydratedSnapshot {
    fn into_resumed(self) -> (SessionSnapshot, ContinuationCache) {
        let cache = ContinuationCache {
            history: self.snapshot.shared_history().clone(),
            history_log: self.history,
            ..ContinuationCache::default()
        };
        (self.snapshot, cache)
    }
}

pub(crate) struct AdvancedSnapshot {
    pub(crate) hydrated: HydratedSnapshot,
    pub(crate) keys: HashSet<String>,
    pub(crate) reused: usize,
    pub(crate) decoded: usize,
}

async fn load_items(
    reader: PayloadReader<'_>,
    references: &[EncodedPayload],
) -> Result<Vec<ResponseItem>> {
    if references.is_empty() {
        return Ok(Vec::new());
    }
    reader
        .load_payloads(references.to_vec())
        .await?
        .iter()
        .map(EncodedPayload::decode)
        .collect()
}

fn same_keys(left: &[EncodedPayload], right: &[EncodedPayload]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.key() == right.key())
}

pub(crate) async fn advance_snapshot(
    reader: PayloadReader<'_>,
    saved: Snapshot,
    previous: HydratedSnapshot,
) -> Result<AdvancedSnapshot> {
    let keys = HashSet::new();
    let HydratedSnapshot {
        snapshot,
        history: log,
        prefix: previous_prefix_references,
    } = previous;
    let (_, previous_history, previous_prefix) = snapshot.into_context_parts();
    advance_parts(
        reader,
        saved,
        Earlier {
            history: previous_history,
            log,
            prefix: previous_prefix,
            prefix_references: previous_prefix_references,
        },
        keys,
    )
    .await
}

struct Earlier {
    history: ResponseHistory,
    log: SequenceLog,
    prefix: Option<Arc<[ResponseItem]>>,
    prefix_references: Option<Vec<EncodedPayload>>,
}

async fn advance_parts(
    reader: PayloadReader<'_>,
    saved: Snapshot,
    earlier: Earlier,
    mut keys: HashSet<String>,
) -> Result<AdvancedSnapshot> {
    let references = sequence_references(reader, saved.history, &mut keys).await?;
    let Earlier {
        history: previous_history,
        mut log,
        prefix: previous_prefix,
        prefix_references: previous_prefix_references,
    } = earlier;
    let reused = log
        .items()
        .iter()
        .zip(&references)
        .take_while(|(kept, reference)| kept.key() == reference.key())
        .count()
        .min(previous_history.len());
    log.truncate(reused);
    let added: Vec<_> = references[reused..].to_vec();
    let decoded = added.len();
    let payloads = if added.is_empty() {
        Vec::new()
    } else {
        reader.take_payloads(added).await?
    };
    let history = loaded_history(&previous_history, &mut log, payloads)?;
    let (prefix, prefix_references) = match saved.prefix {
        None => (None, None),
        Some(sequence) => {
            let references = sequence_references(reader, sequence, &mut keys).await?;
            match (previous_prefix, previous_prefix_references) {
                (Some(items), Some(kept)) if same_keys(&kept, &references) => {
                    (Some(items), Some(references))
                }
                _ => {
                    let items = load_items(reader, &references).await?;
                    (Some(Arc::from(items)), Some(references))
                }
            }
        }
    };
    Ok(AdvancedSnapshot {
        hydrated: HydratedSnapshot {
            snapshot: saved.head.with_context(history, prefix),
            history: log,
            prefix: prefix_references,
        },
        keys,
        reused,
        decoded,
    })
}

#[derive(Default)]
pub(crate) struct ContinuationCache {
    history: ResponseHistory,
    history_log: SequenceLog,
    prefix: Arc<[ResponseItem]>,
    prefix_log: SequenceLog,
    prefix_pooled: bool,
}

impl ContinuationCache {
    pub(crate) fn committed(&mut self) {
        self.history_log.release();
        self.prefix_log.release();
    }

    pub(crate) fn unpooled_prefix(
        &mut self,
        prefix: &Arc<[ResponseItem]>,
    ) -> Result<Vec<EncodedPayload>> {
        self.sync_prefix(prefix)?;
        if self.prefix_pooled {
            return Ok(Vec::new());
        }
        Ok(self.prefix_log.frontier().cloned().collect())
    }

    pub(crate) fn prefix_pooled(&mut self) {
        self.prefix_pooled = true;
        self.prefix_log.release();
    }

    fn sync_history(&mut self, history: &ResponseHistory) -> Result<()> {
        let unchanged = self
            .history
            .shared_prefix_len(history)
            .min(self.history_log.len());
        self.history_log.sync(
            unchanged,
            history.iter_from(unchanged).map(EncodedPayload::encode),
        )?;
        self.history = history.clone();
        Ok(())
    }

    fn sync_prefix(&mut self, prefix: &Arc<[ResponseItem]>) -> Result<()> {
        static LAST: nanocode_sync::SwapOption<(Arc<[ResponseItem]>, Vec<EncodedPayload>)> =
            nanocode_sync::SwapOption::const_empty();
        if !Arc::ptr_eq(&self.prefix, prefix) || self.prefix_log.len() != prefix.len() {
            let shared = crate::client::responses::shared_prefix_records(prefix);
            let encoded = match (
                shared,
                LAST.load_full().filter(|last| Arc::ptr_eq(&last.0, prefix)),
            ) {
                (Some(records), _) => records
                    .into_iter()
                    .map(EncodedPayload::from_record)
                    .collect::<Result<Vec<_>>>()?,
                (None, Some(last)) => last.1.clone(),
                (None, None) => {
                    let encoded = prefix
                        .iter()
                        .map(EncodedPayload::encode)
                        .collect::<Result<Vec<_>>>()?;
                    LAST.store(Some(Arc::new((Arc::clone(prefix), encoded.clone()))));
                    encoded
                }
            };
            self.prefix_log.sync(0, encoded.into_iter().map(Ok))?;
            self.prefix = Arc::clone(prefix);
            self.prefix_pooled = false;
        }
        Ok(())
    }
}

pub(crate) fn prepare_cached_snapshot(
    snapshot: SessionSnapshot,
    cache: &mut ContinuationCache,
) -> Result<EncodedPayload> {
    let (head, history, prefix) = snapshot.into_context_parts();
    cached_parts(head, &history, prefix.as_ref(), cache)
}

pub(crate) fn prepare_cached_completion(
    snapshot: SessionSnapshot,
    output: &ExecutionOutput,
    cache: &mut ContinuationCache,
) -> Result<(EncodedPayload, Box<RawValue>)> {
    let (head, history, prefix) = snapshot.into_context_parts();
    let stored = completion_output(&history, output)?;
    Ok((
        cached_parts(head, &history, prefix.as_ref(), cache)?,
        stored,
    ))
}

fn cached_parts(
    head: SessionSnapshotHead,
    history: &ResponseHistory,
    prefix: Option<&Arc<[ResponseItem]>>,
    cache: &mut ContinuationCache,
) -> Result<EncodedPayload> {
    cache.sync_history(history)?;
    if let Some(prefix) = prefix {
        cache.sync_prefix(prefix)?;
    }
    let payload = EncodedPayload::encode(&Snapshot {
        head,
        history: cache.history_log.sequence(),
        prefix: prefix.map(|_| cache.prefix_log.sequence()),
    })?;
    let mut children = cache.history_log.frontier().cloned().collect::<Vec<_>>();
    if prefix.is_some() {
        children.extend(cache.prefix_log.frontier().cloned());
    }
    Ok(payload.with_payloads(children))
}

pub(crate) fn prepare_cached_continuation(
    state_json: String,
    history: &ResponseHistory,
    prefix: &Arc<[ResponseItem]>,
    cache: &mut ContinuationCache,
) -> Result<EncodedPayload> {
    cache.sync_history(history)?;
    cache.sync_prefix(prefix)?;
    let state =
        RawValue::from_string(state_json).map_err(nanocode_durability::Error::InvalidPayload)?;
    let payload = EncodedPayload::encode(&Continuation {
        state,
        history: cache.history_log.sequence(),
        prefix: cache.prefix_log.sequence(),
    })?;
    Ok(payload.with_payloads(
        cache
            .history_log
            .frontier()
            .chain(cache.prefix_log.frontier())
            .cloned()
            .collect(),
    ))
}

pub(crate) async fn load_continuation(
    reader: PayloadReader<'_>,
    payload: EncodedPayload,
) -> Result<(ExecutionContinuation, HashSet<String>)> {
    let value: Continuation = payload.decode()?;
    let mut keys = HashSet::new();
    let continuation = ExecutionContinuation {
        state_json: value.state.get().to_owned(),
        history: load_sequence(reader, value.history, &mut keys).await?,
        prefix: load_sequence(reader, value.prefix, &mut keys).await?,
    };
    Ok((continuation, keys))
}

#[derive(Deserialize)]
struct ContinuationHistory {
    history: Sequence,
}

pub(crate) async fn continuation_history(
    reader: PayloadReader<'_>,
    continuation: serde_json::Value,
) -> Result<Vec<serde_json::Value>> {
    let value: ContinuationHistory =
        serde_json::from_value(continuation).map_err(nanocode_durability::Error::InvalidPayload)?;
    load_sequence(reader, value.history, &mut HashSet::new()).await
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Snapshot {
    head: SessionSnapshotHead,
    history: Sequence,
    prefix: Option<Sequence>,
}

pub(crate) fn prepare_snapshot(
    value: SessionSnapshot,
    known: &HashSet<String>,
) -> Result<Prepared> {
    let (head, history, prefix) = value.into_context_parts();
    prepare_parts(head, &history, prefix.as_deref(), known)
}

fn prepare_parts(
    head: SessionSnapshotHead,
    history: &ResponseHistory,
    prefix: Option<&[ResponseItem]>,
    known: &HashSet<String>,
) -> Result<Prepared> {
    let mut writer = PagedWriter::new(known);
    let history = writer.sequence_iter(history)?;
    let prefix = prefix.map(|items| writer.sequence(items)).transpose()?;
    writer.finish(&Snapshot {
        head,
        history,
        prefix,
    })
}

pub(crate) async fn load_snapshot(
    reader: PayloadReader<'_>,
    payload: EncodedPayload,
) -> Result<SessionSnapshot> {
    restore_snapshot(reader, payload.decode()?).await
}

pub(crate) async fn restore_snapshot(
    reader: PayloadReader<'_>,
    saved: Snapshot,
) -> Result<SessionSnapshot> {
    load_snapshot_with_keys(reader, saved)
        .await
        .map(|(snapshot, _)| snapshot)
}

pub(crate) async fn restore_cached_snapshot(
    reader: PayloadReader<'_>,
    saved: Snapshot,
    cache: &mut ContinuationCache,
) -> Result<SessionSnapshot> {
    let prefix_references = (!cache.prefix.is_empty()
        && cache.prefix_log.len() == cache.prefix.len())
    .then(|| cache.prefix_log.items().to_vec());
    let earlier = Earlier {
        history: std::mem::take(&mut cache.history),
        log: std::mem::take(&mut cache.history_log),
        prefix: prefix_references
            .as_ref()
            .map(|_| Arc::clone(&cache.prefix)),
        prefix_references,
    };
    let advanced = advance_parts(reader, saved, earlier, HashSet::new()).await?;
    let snapshot = advanced.hydrated.snapshot;
    cache.history = snapshot.shared_history().clone();
    cache.history_log = advanced.hydrated.history;
    Ok(snapshot)
}

pub(crate) fn preloaded_snapshot(
    checkpoint: nanocode_durability::Checkpoint,
    records: Option<Arc<HashSet<String>>>,
    hydrated: Option<nanocode_durability::Hydrated>,
    key: &str,
) -> Option<(SessionSnapshot, ContinuationCache)> {
    let hydrated = hydrated.or_else(|| checkpoint.hydrated().cloned());
    let checkpoint = match checkpoint {
        nanocode_durability::Checkpoint::Value(value) => Some(value),
        nanocode_durability::Checkpoint::Typed { .. } => None,
    };
    let (snapshot, cache) =
        match hydrated.and_then(nanocode_durability::Hydrated::take::<HydratedSnapshot>) {
            Some(hydrated) => hydrated.into_resumed(),
            None => (
                SessionSnapshot::deserialize(checkpoint?).ok()?,
                ContinuationCache::default(),
            ),
        };
    if records.is_some() {
        return Some((snapshot, cache));
    }
    let (head, history, prefix) = snapshot.into_context_parts();
    let known = HashSet::new();
    let mut writer = PagedWriter::new(&known);
    let saved = Snapshot {
        history: writer.sequence_iter(&history).ok()?,
        prefix: prefix
            .as_ref()
            .map(|items| writer.sequence(items))
            .transpose()
            .ok()?,
        head,
    };
    let prepared = writer.finish(&saved).ok()?;
    if !prepared.payload.matches_key(key) {
        return None;
    }
    Some((saved.head.with_context(history, prefix), cache))
}

pub(crate) async fn load_snapshot_with_keys(
    reader: PayloadReader<'_>,
    saved: Snapshot,
) -> Result<(SessionSnapshot, HashSet<String>)> {
    load_hydrated_with_keys(reader, saved)
        .await
        .map(|(hydrated, keys)| (hydrated.snapshot, keys))
}

pub(crate) async fn load_resumed(
    reader: PayloadReader<'_>,
    saved: Snapshot,
) -> Result<(SessionSnapshot, ContinuationCache)> {
    let (hydrated, _) = load_hydrated_with_keys(reader, saved).await?;
    Ok(hydrated.into_resumed())
}

pub(crate) async fn load_hydrated_with_keys(
    reader: PayloadReader<'_>,
    saved: Snapshot,
) -> Result<(HydratedSnapshot, HashSet<String>)> {
    let mut keys = HashSet::new();
    let references = sequence_references(reader, saved.history, &mut keys).await?;
    let payloads = if references.is_empty() {
        Vec::new()
    } else {
        reader.take_payloads(references).await?
    };
    let mut log = SequenceLog::default();
    let history = loaded_history(&ResponseHistory::default(), &mut log, payloads)?;
    let (prefix, prefix_references) = match saved.prefix {
        Some(sequence) => {
            let references = sequence_references(reader, sequence, &mut keys).await?;
            let items = load_items(reader, &references).await?;
            (Some(Arc::from(items)), Some(references))
        }
        None => (None, None),
    };
    Ok((
        HydratedSnapshot {
            snapshot: saved.head.with_context(history, prefix),
            history: log,
            prefix: prefix_references,
        },
        keys,
    ))
}
