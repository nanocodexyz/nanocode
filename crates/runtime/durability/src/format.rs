use std::{any::Any, collections::HashSet, fmt, future::Future, pin::Pin, sync::Arc};

use nanocode_sync::Swap;

use nanocode_agent_core::{HarnessFamily, SessionCheckpoint, Thinking, session::TranscriptItem};
use serde_json::Value;

use crate::{EncodedPayload, Error, PayloadReader, Result, SessionRecord};

#[cfg(not(target_family = "wasm"))]
pub type FormatFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
#[cfg(target_family = "wasm")]
pub type FormatFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

type Render = fn(&(dyn Any + Send + Sync)) -> Result<Value>;

/// A format's own typed form of a hydrated checkpoint, which can render
/// the checkpoint's JSON form on demand.
pub trait RenderCheckpoint: Any + Send + Sync {
    /// The checkpoint's whole JSON form, as `CheckpointFormat::hydrate`
    /// returns it.
    fn render(&self) -> Result<Value>;
}

#[derive(Clone)]
pub struct Hydrated {
    value: Arc<dyn Any + Send + Sync>,
    render: Option<Render>,
}

fn render_as<T: RenderCheckpoint>(value: &(dyn Any + Send + Sync)) -> Result<Value> {
    value.downcast_ref::<T>().map_or_else(
        || {
            Err(Error::InvalidState(
                "a hydrated checkpoint changed type".to_owned(),
            ))
        },
        T::render,
    )
}

impl Hydrated {
    pub fn new<T: Any + Send + Sync>(value: T) -> Self {
        Self {
            value: Arc::new(value),
            render: None,
        }
    }

    /// A typed checkpoint that renders its JSON form only when asked.
    pub fn rendering<T: RenderCheckpoint>(value: T) -> Self {
        Self {
            value: Arc::new(value),
            render: Some(render_as::<T>),
        }
    }

    pub fn take<T: Any + Clone + Send + Sync>(self) -> Option<T> {
        Arc::downcast::<T>(self.value)
            .ok()
            .map(Arc::unwrap_or_clone)
    }

    #[must_use]
    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.value.downcast_ref::<T>()
    }

    fn render(&self) -> Option<Result<Value>> {
        self.render.map(|render| render(&*self.value))
    }

    fn is(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.value, &other.value)
    }
}

/// A loaded checkpoint: its JSON form, or a format's typed form with just
/// the JSON head (every field but the conversation), whose whole JSON form
/// is rendered only on demand.
#[derive(Clone, Debug)]
pub enum Checkpoint {
    Value(Value),
    Typed { head: Value, hydrated: Hydrated },
}

impl From<Value> for Checkpoint {
    fn from(value: Value) -> Self {
        Self::Value(value)
    }
}

impl Checkpoint {
    /// A typed checkpoint; `hydrated` must come from `Hydrated::rendering`.
    #[must_use]
    pub const fn typed(head: Value, hydrated: Hydrated) -> Self {
        Self::Typed { head, hydrated }
    }

    /// The JSON fields beside the conversation (model, thinking, workspace
    /// and the rest); for a JSON checkpoint the whole checkpoint.
    #[must_use]
    pub const fn head(&self) -> &Value {
        match self {
            Self::Value(value) | Self::Typed { head: value, .. } => value,
        }
    }

    #[must_use]
    pub const fn as_value(&self) -> Option<&Value> {
        match self {
            Self::Value(value) => Some(value),
            Self::Typed { .. } => None,
        }
    }

    #[must_use]
    pub const fn hydrated(&self) -> Option<&Hydrated> {
        match self {
            Self::Value(_) => None,
            Self::Typed { hydrated, .. } => Some(hydrated),
        }
    }

    #[must_use]
    pub fn into_hydrated(self) -> Option<Hydrated> {
        match self {
            Self::Value(_) => None,
            Self::Typed { hydrated, .. } => Some(hydrated),
        }
    }

    /// Whether this checkpoint's typed form is `hydrated`.
    #[must_use]
    pub fn holds(&self, hydrated: &Hydrated) -> bool {
        self.hydrated().is_some_and(|held| held.is(hydrated))
    }

    /// The whole JSON form, rendered when the checkpoint is typed.
    pub fn value(&self) -> Result<std::borrow::Cow<'_, Value>> {
        match self {
            Self::Value(value) => Ok(std::borrow::Cow::Borrowed(value)),
            Self::Typed { .. } => self.render().map(std::borrow::Cow::Owned),
        }
    }

    pub fn into_value(self) -> Result<Value> {
        match self {
            Self::Value(value) => Ok(value),
            Self::Typed { .. } => self.render(),
        }
    }

    fn render(&self) -> Result<Value> {
        match self {
            Self::Value(value) => Ok(value.clone()),
            Self::Typed { hydrated, .. } => hydrated.render().unwrap_or_else(|| {
                Err(Error::InvalidState(
                    "a typed checkpoint has no JSON form".to_owned(),
                ))
            }),
        }
    }
}

impl fmt::Debug for Hydrated {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Hydrated")
    }
}

pub struct Previous {
    pub checkpoint: Checkpoint,
    pub hydrated: Option<Hydrated>,
}

pub struct Advanced {
    pub checkpoint: Checkpoint,
    pub hydrated: Option<Hydrated>,
    pub whole: bool,
    pub reused: usize,
    pub decoded: usize,
}

impl Advanced {
    #[must_use]
    pub const fn whole(checkpoint: Checkpoint, hydrated: Option<Hydrated>) -> Self {
        Self {
            checkpoint,
            hydrated,
            whole: true,
            reused: 0,
            decoded: 0,
        }
    }
}

pub struct AdmittedTurn<'a> {
    pub input: &'a Value,
    pub steers: usize,
}

pub trait CheckpointFormat: Send + Sync {
    fn family(&self) -> HarnessFamily;

    fn owns(&self, checkpoint: &Value) -> bool;

    fn hydrate<'a>(
        &'a self,
        _reader: PayloadReader<'a>,
        stored: Value,
    ) -> FormatFuture<'a, Result<Value>> {
        Box::pin(async move { Ok(stored) })
    }

    fn hydrate_noting<'a>(
        &'a self,
        reader: PayloadReader<'a>,
        stored: Value,
        _read: &'a mut HashSet<String>,
    ) -> FormatFuture<'a, Result<Value>> {
        self.hydrate(reader, stored)
    }

    fn hydrate_deferred<'a>(
        &'a self,
        _reader: PayloadReader<'a>,
        _stored: &'a Value,
        _read: &'a mut HashSet<String>,
    ) -> FormatFuture<'a, Result<Option<Checkpoint>>> {
        Box::pin(async { Ok(None) })
    }

    fn hydrate_typed<'a>(
        &'a self,
        reader: PayloadReader<'a>,
        stored: Value,
        read: &'a mut HashSet<String>,
    ) -> FormatFuture<'a, Result<(Checkpoint, Option<Hydrated>)>> {
        Box::pin(async move {
            Ok((
                Checkpoint::Value(self.hydrate_noting(reader, stored, read).await?),
                None,
            ))
        })
    }

    fn advance_typed<'a>(
        &'a self,
        reader: PayloadReader<'a>,
        stored: Value,
        read: &'a mut HashSet<String>,
        previous: Previous,
    ) -> FormatFuture<'a, Result<Advanced>> {
        drop(previous);
        Box::pin(async move {
            let (checkpoint, hydrated) = self.hydrate_typed(reader, stored, read).await?;
            Ok(Advanced::whole(checkpoint, hydrated))
        })
    }

    fn hydrate_continuation<'a>(
        &'a self,
        _reader: PayloadReader<'a>,
        _continuation: Value,
    ) -> FormatFuture<'a, Result<Option<Value>>> {
        Box::pin(async { Ok(None) })
    }

    /// A stored continuation hydrated as the family's own execution reads
    /// it before running the operation; `None` when the family reads none
    /// ahead (`SessionStore::continuation_ahead`).
    fn hydrate_cursor<'a>(
        &'a self,
        _reader: PayloadReader<'a>,
        _continuation: Value,
    ) -> FormatFuture<'a, Result<Option<Value>>> {
        Box::pin(async { Ok(None) })
    }

    fn encode(&self, checkpoint: Value) -> Result<EncodedPayload> {
        EncodedPayload::encode(&checkpoint)
    }

    fn session_checkpoint(
        &self,
        record: &SessionRecord,
        checkpoint: Value,
    ) -> Result<SessionCheckpoint>;

    fn thinking(&self, record: &SessionRecord, checkpoint: &Value) -> Result<Thinking> {
        Ok(self
            .session_checkpoint(record, checkpoint.clone())?
            .thinking())
    }

    fn transcript(&self, checkpoint: &Value, turns: &[AdmittedTurn<'_>]) -> Vec<TranscriptItem>;

    /// `thinking` read from a typed checkpoint without rendering its JSON
    /// form; `None` when the format reads only the JSON form.
    fn thinking_typed(
        &self,
        _record: &SessionRecord,
        _head: &Value,
        _hydrated: &Hydrated,
    ) -> Option<Result<Thinking>> {
        None
    }

    /// `transcript` read from a typed checkpoint without rendering its
    /// JSON form; `None` when the format reads only the JSON form.
    fn transcript_typed(
        &self,
        _hydrated: &Hydrated,
        _turns: &[AdmittedTurn<'_>],
    ) -> Option<Vec<TranscriptItem>> {
        None
    }
}

#[derive(Clone, Default)]
pub(crate) struct Formats(Arc<Swap<Vec<Arc<dyn CheckpointFormat>>>>);

impl Formats {
    pub(crate) fn register(&self, format: Arc<dyn CheckpointFormat>) -> Result<()> {
        let family = format.family();
        self.0.rcu(|formats| {
            formats
                .iter()
                .filter(|known| known.family() != family)
                .cloned()
                .chain(std::iter::once(Arc::clone(&format)))
                .collect::<Vec<_>>()
        });
        Ok(())
    }

    pub(crate) fn family(&self, family: HarnessFamily) -> Result<Arc<dyn CheckpointFormat>> {
        self.0
            .load()
            .iter()
            .find(|format| format.family() == family)
            .cloned()
            .ok_or_else(|| Error::UnsupportedFamily(family.to_string()))
    }

    pub(crate) fn owner(&self, checkpoint: &Value) -> Result<Arc<dyn CheckpointFormat>> {
        self.0
            .load()
            .iter()
            .find(|format| format.owns(checkpoint))
            .cloned()
            .ok_or_else(|| {
                Error::InvalidState("no registered checkpoint format owns this checkpoint".into())
            })
    }
}

impl fmt::Debug for Formats {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let families: Vec<_> = self.0.load().iter().map(|format| format.family()).collect();
        formatter.debug_list().entries(families).finish()
    }
}
