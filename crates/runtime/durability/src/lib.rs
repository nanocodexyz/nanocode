#![deny(rustdoc::broken_intra_doc_links)]

mod backend;
mod blob;
mod branch;
mod catalog;
mod child_journal;
mod format;
mod kept_checkpoints;
mod memory;
mod paged;
mod payload_memo;
mod pool;
mod record_cache;
mod session;
mod shared_store;
pub mod sim;
mod state;

pub mod facts {
    pub use nanocode_durability_core::{
        Append, Base, BlobRef, BlockRef, DIGEST_BYTES, Decision, Digest, Divergence, DurableFold,
        EffectResult, Fact, FactLog, FactLogs, FactSink, FoldError, Folded, Input, InputKind,
        Intent, IntentTarget, Marker, ModelOut, Outcome, OwnedFact, Replay, Seen, Snapshot, Start,
        StoredFact, TurnStatus, VALUE_INLINE_BYTES, Value, Values, commit_fact, fold_facts,
        referenced,
    };
}

pub use backend::{
    Branches, DurableAgentExt, DurableBackendRegistration, agent_error, agent_store_error,
};
pub use blob::MemoryBlobStore;
pub use branch::{BranchTurn, CheckpointBranch};
pub use catalog::{
    Advance, BranchCursor, BranchExport, BranchExportMode, BranchImport, BranchPage, BranchPoint,
    CancelTo, CancelledPending, IntoSessionStore, LIST_LIMIT, Resumable, ResumableSession,
    SessionBrief, SessionPage, SessionQuery, SessionReader, SessionStore, SessionSummary,
    StoredSession, StoredTurn, TranscriptItem, TurnStatus,
};
pub use catalog::{BranchBoundary, InitialSettings, SessionRecord};
pub use child_journal::{JournalClaim, JournalSeed};
pub use format::{
    AdmittedTurn, Advanced, Checkpoint, CheckpointFormat, FormatFuture, Hydrated, Previous,
    RenderCheckpoint,
};
pub use kept_checkpoints::{ChildPreload, ChildReuse};
pub use memory::MemoryStore;
pub use nanocode_agent_core::backend::{ContinuationAhead, Prefetched};
pub use nanocode_durability_core::{
    BlobKey, BlobStore, BlobUrl, BriefFilter, BriefState, ChildPlace, ChildScopes, Colocation,
    Committed, ConcurrentIndex, EncodedPayload, Error, EventCursor, EventHit, EventHits,
    EventOrder, EventPage, EventQuery, EventRange, Examined, FactLog, FactLogs, IndexedState,
    KeyValue, KeyValueStore, KnownSummary, ListedState, ListedSummary, Migrated, Migration,
    MissingColumn, OwnedCommit, OwnedState, OwnerId, OwnerToken, PageCodec, Result, RowStore,
    Settle, SharedBlobStore, Sql, SqlAccess, SqlDialect, SqlRow, SqlStatement, SqlStore,
    StagedStore, StagedWrites, StateCommit, StateCursor, StatePage, StateQuery, StateReader,
    StateStore, StateSummary, StoreError, StoreEvent, StoreFuture, StoreHandles, StoreRecord,
    StoreReplacement, StoreUpdate, StoredEvent, StoredState, SummaryFilter, SummaryUpdate,
    Versioned, base64_data_url, build_index_concurrently, drop_index_concurrently, get_blob,
    migrate_format, migrate_format_then,
};
pub use paged::{
    PagedWriter, PayloadReader, Prepared, Sequence, SequenceLog, load_sequence, load_sequence_from,
    load_sequence_payloads, sequence_references, sequence_references_from,
};
pub use pool::pool_state;
pub use record_cache::{CachedStore, RecordCache};
pub use session::{
    Admission, AutomaticAdmission, BeginStep, DurableOwner, DurableSession, InitialAcquisition,
    MetadataExpectation, MetadataWrite, OwnerWrite, ResultTiming, SessionReceipt, SessionWrite,
    StepRecovery, StoredSteer, TitleChange, WeakDurableSession, WriteOutcome,
};
pub use shared_store::SharedStore;
#[cfg(not(target_family = "wasm"))]
pub use shared_store::drive_on;
pub use state::{
    DurableState, IdentifiedSteerReceipt, OperationState, OperationStatus, SteerState, StepState,
    StepStatus, Transition,
};
