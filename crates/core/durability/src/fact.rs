use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};
use serde_json::value::RawValue;

use nanocode_sys::base64::STANDARD_NO_PAD;

use crate::{BlobKey, StoreError};

pub const DIGEST_BYTES: usize = nanocode_digest::BLAKE3_OUTPUT_BYTES;

pub const VALUE_INLINE_BYTES: usize = crate::payload::INLINE_BYTES;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Digest([u8; DIGEST_BYTES]);

impl Digest {
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(*nanocode_digest::blake3(bytes).as_bytes())
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; DIGEST_BYTES]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; DIGEST_BYTES] {
        &self.0
    }

    #[must_use]
    pub fn key(&self) -> BlobKey {
        BlobKey::from_hash(&nanocode_digest::Hash::from_bytes(self.0))
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD_NO_PAD.encode(self.0))
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        let bytes = STANDARD_NO_PAD
            .decode(text.as_bytes())
            .map_err(|error| serde::de::Error::custom(format!("digest is not base64: {error}")))?;
        let bytes: [u8; DIGEST_BYTES] = bytes.try_into().map_err(|bytes: Vec<u8>| {
            serde::de::Error::custom(format!(
                "digest has {} bytes, not {DIGEST_BYTES}",
                bytes.len()
            ))
        })?;
        Ok(Self(bytes))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct BlobRef(u32);

impl BlobRef {
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }

    #[must_use]
    pub const fn from_index(index: u32) -> Self {
        Self(index)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Value {
    Inline(Box<RawValue>),
    Blob(BlobRef),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlockRef {
    pub model_out: u64,
    pub block: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Prompt,
    Steer { message_id: Option<String> },
    Edit { edit_id: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Cancelled,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Cancelling,
    Interrupted {
        note: String,
    },
    Retrying {
        attempts: u64,
        error: String,
        kind: String,
        retry_at: u64,
    },
    Finished {
        last: u64,
        outcome: Outcome,
        result: Box<RawValue>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Base<V> {
    pub state: String,
    pub revision: u64,
    pub checkpoint: V,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Input<V> {
    pub turn: String,
    pub kind: InputKind,
    pub value: V,
    pub metadata: V,
    pub hand: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelOut<V> {
    pub turn: String,
    pub response: V,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentTarget<V> {
    ToolUse(BlockRef),
    Nested { parent: String, input: V },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Intent<V> {
    pub turn: Option<String>,
    pub call_id: String,
    pub tool: String,
    pub target: IntentTarget<V>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EffectResult<V> {
    pub call_id: String,
    pub output: V,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision<D> {
    Time { unix_millis: u64 },
    Id { id: String },
    Route { model: String },
    Host { key: String, value: Box<RawValue> },
    Backend(D),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Marker {
    Status {
        turn: String,
        status: TurnStatus,
    },
    Recovery {
        turn: String,
        attempt: u64,
        at: Option<u64>,
    },
    Fence {
        owner: String,
    },
    Floor {
        seq: u64,
    },
    Wake {
        key: String,
        generation: u64,
        armed: bool,
    },
    Hold {
        key: String,
        held: bool,
    },
    Blob {
        digest: Digest,
        bytes: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fact<V, D> {
    Base(Base<V>),
    Input(Input<V>),
    ModelOut(ModelOut<V>),
    Intent(Intent<V>),
    EffectResult(EffectResult<V>),
    Decision(Decision<D>),
    Marker(Marker),
}

pub type StoredFact = Fact<Value, Box<RawValue>>;

pub type OwnedFact<D> = Fact<Box<RawValue>, D>;

pub type Seen<'a, D> = Fact<&'a RawValue, &'a D>;

impl<V, D> Fact<V, D> {
    pub fn try_map<'a, W, E, X>(
        &'a self,
        mut value: impl FnMut(&'a V) -> Result<W, X>,
        decision: impl FnOnce(&'a D) -> Result<E, X>,
    ) -> Result<Fact<W, E>, X> {
        Ok(match self {
            Self::Base(base) => Fact::Base(Base {
                state: base.state.clone(),
                revision: base.revision,
                checkpoint: value(&base.checkpoint)?,
            }),
            Self::Input(input) => Fact::Input(Input {
                turn: input.turn.clone(),
                kind: input.kind.clone(),
                value: value(&input.value)?,
                metadata: value(&input.metadata)?,
                hand: input.hand.clone(),
                first: input.first,
            }),
            Self::ModelOut(out) => Fact::ModelOut(ModelOut {
                turn: out.turn.clone(),
                response: value(&out.response)?,
            }),
            Self::Intent(intent) => Fact::Intent(Intent {
                turn: intent.turn.clone(),
                call_id: intent.call_id.clone(),
                tool: intent.tool.clone(),
                target: match &intent.target {
                    IntentTarget::ToolUse(block) => IntentTarget::ToolUse(*block),
                    IntentTarget::Nested { parent, input } => IntentTarget::Nested {
                        parent: parent.clone(),
                        input: value(input)?,
                    },
                },
            }),
            Self::EffectResult(result) => Fact::EffectResult(EffectResult {
                call_id: result.call_id.clone(),
                output: value(&result.output)?,
            }),
            Self::Decision(chosen) => Fact::Decision(match chosen {
                Decision::Time { unix_millis } => Decision::Time {
                    unix_millis: *unix_millis,
                },
                Decision::Id { id } => Decision::Id { id: id.clone() },
                Decision::Route { model } => Decision::Route {
                    model: model.clone(),
                },
                Decision::Host { key, value } => Decision::Host {
                    key: key.clone(),
                    value: value.clone(),
                },
                Decision::Backend(backend) => Decision::Backend(decision(backend)?),
            }),
            Self::Marker(marker) => Fact::Marker(marker.clone()),
        })
    }
}

impl<D> OwnedFact<D> {
    #[must_use]
    pub fn seen(&self) -> Seen<'_, D> {
        let seen: Result<Seen<'_, D>, std::convert::Infallible> =
            self.try_map(|value| Ok(&**value), Ok);
        match seen {
            Ok(seen) => seen,
            Err(never) => match never {},
        }
    }
}

#[derive(Debug)]
pub enum FoldError {
    Rejected { seq: u64, reason: String },
    Decode { seq: u64, reason: String },
    UnknownBlob { seq: u64, blob: BlobRef },
    MissingBlob { digest: Digest },
    CorruptBlob { digest: Digest },
    OutOfOrder { seq: u64, after: u64 },
    Encode(String),
    Sink(StoreError),
}

impl std::fmt::Display for FoldError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected { seq, reason } => {
                write!(formatter, "fact {seq} does not fold: {reason}")
            }
            Self::Decode { seq, reason } => write!(formatter, "fact {seq} is invalid: {reason}"),
            Self::UnknownBlob { seq, blob } => write!(
                formatter,
                "fact {seq} names blob {} that the log never interned",
                blob.index()
            ),
            Self::MissingBlob { digest } => {
                write!(formatter, "blob {} is missing", digest.key())
            }
            Self::CorruptBlob { digest } => {
                write!(
                    formatter,
                    "blob {} does not hold the value its digest names",
                    digest.key()
                )
            }
            Self::OutOfOrder { seq, after } => {
                write!(formatter, "fact {seq} arrived after fact {after}")
            }
            Self::Encode(reason) => {
                write!(formatter, "fact or folded state does not encode: {reason}")
            }
            Self::Sink(error) => std::fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for FoldError {}

impl From<FoldError> for StoreError {
    fn from(error: FoldError) -> Self {
        match error {
            FoldError::Sink(error) => error,
            FoldError::Encode(reason) => Self::NotCommitted(reason),
            other => Self::Backend(other.to_string()),
        }
    }
}

pub trait DurableFold: Default + Serialize + DeserializeOwned {
    const FAMILY: &'static str;
    const FORMAT: u32;
    type Decision: Serialize + DeserializeOwned;

    fn apply(&mut self, seq: u64, fact: Seen<'_, Self::Decision>) -> Result<(), FoldError>;

    fn request(&self) -> Result<Vec<u8>, FoldError>;
}

pub trait FactSink {
    fn next(&self) -> u64;

    fn put_blob(&mut self, digest: &Digest, json: &RawValue) -> Result<(), FoldError>;

    fn put_interned(&mut self, _digest: &Digest, _json: &RawValue) -> Result<(), FoldError> {
        Ok(())
    }

    fn append(&mut self, fact: StoredFact) -> Result<u64, FoldError>;

    fn cache_due(&self) -> bool {
        false
    }

    fn cache(&mut self, _snapshot: &Snapshot) -> Result<Vec<(u64, StoredFact)>, FoldError> {
        Ok(Vec::new())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Values {
    digests: Vec<Digest>,
    #[serde(skip)]
    known: BTreeMap<Digest, BlobRef>,
}

impl Values {
    #[must_use]
    pub fn digest(&self, blob: BlobRef) -> Option<&Digest> {
        self.digests.get(blob.0 as usize)
    }

    #[must_use]
    pub fn blob(&self, digest: &Digest) -> Option<BlobRef> {
        self.known.get(digest).copied()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.digests.len()
    }

    #[must_use]
    pub fn digests(&self) -> &[Digest] {
        &self.digests
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.digests.is_empty()
    }

    pub fn reindex(&mut self) {
        self.known = self
            .digests
            .iter()
            .enumerate()
            .map(|(index, digest)| (*digest, BlobRef(blob_index(index))))
            .collect();
    }

    fn intern(&mut self, digest: Digest) -> (BlobRef, bool) {
        if let Some(blob) = self.known.get(&digest) {
            return (*blob, false);
        }
        let blob = BlobRef(blob_index(self.digests.len()));
        self.digests.push(digest);
        self.known.insert(digest, blob);
        (blob, true)
    }

    pub fn observe(&mut self, seq: u64, fact: &StoredFact) -> Result<(), FoldError> {
        if let Fact::Marker(Marker::Blob { digest, .. }) = fact {
            let (_, fresh) = self.intern(*digest);
            if !fresh {
                return Err(FoldError::Decode {
                    seq,
                    reason: format!("blob {} is interned twice", digest.key()),
                });
            }
        }
        Ok(())
    }
}

fn blob_index(index: usize) -> u32 {
    u32::try_from(index).expect("a log interns fewer blobs than u32 counts")
}

pub fn referenced<'a>(facts: impl IntoIterator<Item = &'a StoredFact>) -> Vec<Digest> {
    facts
        .into_iter()
        .filter_map(|fact| match fact {
            Fact::Marker(Marker::Blob { digest, .. }) => Some(*digest),
            _ => None,
        })
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub family: String,
    pub format: u32,
    pub seq: u64,
    pub applied: u64,
    pub values: Values,
    pub state: Box<RawValue>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Start {
    Empty,
    Snapshot { seq: u64 },
    Replayed { stale: Option<u32> },
}

#[derive(Debug)]
pub enum Divergence {
    State {
        seq: u64,
        live: String,
        folded: String,
    },
    Request {
        seq: u64,
        live: Vec<u8>,
        folded: Vec<u8>,
    },
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::State { seq, .. } => write!(
                formatter,
                "live state at fact {seq} differs from the fold of its log"
            ),
            Self::Request { seq, .. } => write!(
                formatter,
                "live next request at fact {seq} differs from the one the fold of its log builds"
            ),
        }
    }
}

pub struct Folded<F: DurableFold> {
    state: F,
    seq: u64,
    applied: u64,
    values: Values,
}

impl<F: DurableFold> Default for Folded<F> {
    fn default() -> Self {
        Self {
            state: F::default(),
            seq: 0,
            applied: 0,
            values: Values::default(),
        }
    }
}

impl<F: DurableFold> Folded<F> {
    #[must_use]
    pub const fn state(&self) -> &F {
        &self.state
    }

    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    #[must_use]
    pub const fn values(&self) -> &Values {
        &self.values
    }

    pub fn request(&self) -> Result<Vec<u8>, FoldError> {
        self.state.request()
    }

    pub fn open<'b>(
        snapshot: Option<Snapshot>,
        facts: impl IntoIterator<Item = (u64, StoredFact)>,
        blob: impl Fn(&Digest) -> Result<&'b RawValue, FoldError>,
    ) -> Result<(Self, Start), FoldError> {
        let (mut folded, mut start) = Self::start(snapshot)?;
        for (seq, fact) in facts {
            if seq <= folded.seq && matches!(start, Start::Snapshot { .. }) {
                continue;
            }
            folded.replay(seq, &fact, &blob)?;
            if start == Start::Empty {
                start = Start::Replayed { stale: None };
            }
        }
        Ok((folded, start))
    }

    fn start(snapshot: Option<Snapshot>) -> Result<(Self, Start), FoldError> {
        let Some(snapshot) = snapshot else {
            return Ok((Self::default(), Start::Empty));
        };
        if snapshot.family != F::FAMILY || snapshot.format != F::FORMAT {
            let stale = Some(snapshot.format);
            return Ok((Self::default(), Start::Replayed { stale }));
        }
        let state =
            serde_json::from_str(snapshot.state.get()).map_err(|error| FoldError::Decode {
                seq: snapshot.seq,
                reason: format!("snapshot does not decode: {error}"),
            })?;
        let mut values = snapshot.values;
        values.reindex();
        let seq = snapshot.seq;
        let applied = snapshot.applied;
        let folded = Self {
            state,
            seq,
            applied,
            values,
        };
        Ok((folded, Start::Snapshot { seq }))
    }

    fn replay<'b>(
        &mut self,
        seq: u64,
        fact: &StoredFact,
        blob: &impl Fn(&Digest) -> Result<&'b RawValue, FoldError>,
    ) -> Result<(), FoldError> {
        if seq <= self.seq {
            return Err(FoldError::OutOfOrder {
                seq,
                after: self.seq,
            });
        }
        self.values.observe(seq, fact)?;
        self.seq = seq;
        if matches!(fact, Fact::Marker(Marker::Blob { .. })) {
            return Ok(());
        }
        let values = &self.values;
        let owned: OwnedFact<F::Decision> = fact.try_map(
            |value| match value {
                Value::Inline(json) => Ok(json.clone()),
                Value::Blob(reference) => {
                    let digest = values.digest(*reference).ok_or(FoldError::UnknownBlob {
                        seq,
                        blob: *reference,
                    })?;
                    let json = blob(digest)?;
                    Ok(json.to_owned())
                }
            },
            |decision| {
                serde_json::from_str(decision.get()).map_err(|error| FoldError::Decode {
                    seq,
                    reason: format!("{} decision does not decode: {error}", F::FAMILY),
                })
            },
        )?;
        self.fold(seq, &owned)
    }

    fn fold(&mut self, seq: u64, fact: &OwnedFact<F::Decision>) -> Result<(), FoldError> {
        if matches!(fact, Fact::Base(_)) && self.applied > 0 {
            return Err(FoldError::Decode {
                seq,
                reason: format!("a base follows {} folded facts", self.applied),
            });
        }
        self.state.apply(seq, fact.seen())?;
        self.applied += 1;
        Ok(())
    }

    #[must_use]
    pub const fn applied(&self) -> u64 {
        self.applied
    }

    pub fn commit(
        mut self,
        sink: &mut impl FactSink,
        fact: OwnedFact<F::Decision>,
    ) -> Result<(Self, u64), FoldError> {
        let next = sink.next();
        assert!(
            next > self.seq,
            "a sink hands out sequence numbers past the fold"
        );
        let mut fresh: Vec<(Digest, &RawValue)> = Vec::new();
        let mut interned: Vec<(Digest, &RawValue)> = Vec::new();
        let values = &mut self.values;
        let stored: StoredFact = fact.try_map(
            |json| {
                if json.get().len() < VALUE_INLINE_BYTES {
                    return Ok(Value::Inline(json.clone()));
                }
                let digest = Digest::of(json.get().as_bytes());
                let (blob, new) = values.intern(digest);
                if new {
                    fresh.push((digest, &**json));
                } else {
                    interned.push((digest, &**json));
                }
                Ok(Value::Blob(blob))
            },
            |decision| {
                serde_json::value::to_raw_value(decision)
                    .map_err(|error| FoldError::Encode(error.to_string()))
            },
        )?;
        let seq = next + fresh.len() as u64;
        assert!(
            !matches!(fact, Fact::Base(_)) || self.applied == 0,
            "a base is committed before any other fact"
        );
        self.fold(seq, &fact)?;
        for (digest, json) in &fresh {
            sink.put_blob(digest, json)?;
            let marker = Fact::Marker(Marker::Blob {
                digest: *digest,
                bytes: json.get().len() as u64,
            });
            sink.append(marker)?;
        }
        for (digest, json) in &interned {
            sink.put_interned(digest, json)?;
        }
        let written = sink.append(stored)?;
        assert_eq!(
            written, seq,
            "the sink appended the fact where it said it would"
        );
        self.seq = written;
        Ok((self, written))
    }

    pub fn cache(mut self, sink: &mut impl FactSink) -> Result<Self, FoldError> {
        for (seq, fact) in sink.cache(&self.snapshot()?)? {
            assert!(
                matches!(fact, Fact::Marker(Marker::Blob { .. })),
                "a cached fold appends only the marker of its own blob"
            );
            assert!(seq > self.seq, "a cached fold's marker follows the fold");
            self.values.observe(seq, &fact)?;
            self.seq = seq;
        }
        Ok(self)
    }

    pub fn snapshot(&self) -> Result<Snapshot, FoldError> {
        let state = serde_json::value::to_raw_value(&self.state)
            .map_err(|error| FoldError::Encode(error.to_string()))?;
        Ok(Snapshot {
            family: F::FAMILY.to_owned(),
            format: F::FORMAT,
            seq: self.seq,
            applied: self.applied,
            values: self.values.clone(),
            state,
        })
    }

    pub fn diverges<'b>(
        &self,
        facts: impl IntoIterator<Item = (u64, StoredFact)>,
        blob: impl Fn(&Digest) -> Result<&'b RawValue, FoldError>,
    ) -> Result<Option<Divergence>, FoldError> {
        let (folded, _) = Self::open(None, facts, blob)?;
        let live = serde_json::to_string(&self.state)
            .map_err(|error| FoldError::Encode(error.to_string()))?;
        let again = serde_json::to_string(&folded.state)
            .map_err(|error| FoldError::Encode(error.to_string()))?;
        if live != again {
            return Ok(Some(Divergence::State {
                seq: self.seq,
                live,
                folded: again,
            }));
        }
        let (live, again) = (self.request()?, folded.request()?);
        Ok((live != again).then_some(Divergence::Request {
            seq: self.seq,
            live,
            folded: again,
        }))
    }
}
