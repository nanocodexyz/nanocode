pub mod adopt;
mod bound;
mod codec;
mod effect;
#[cfg(not(target_family = "wasm"))]
mod file;
mod fold;
mod formats;
mod hold;
mod host;
mod image;
mod layouts;
pub mod legacy;
pub mod parted;
mod patch;
mod record;
pub mod sim;
mod stamps;
mod wake;
mod wake_index;
mod writer;

pub use codec::{extensible, standalone, standalone_plain, standalone_row};
pub use effect::{EffectClass, Effects};
#[cfg(not(target_family = "wasm"))]
pub use file::{FileLog, FileOptions};
pub use fold::{
    CachedAt, CachedFold, Checkpoint, Event, FOLD_SNAPSHOT_KEY, Fold, OpenCall, Phase, RETIRED_KEY,
    Recovered, Retired, Retry, Segment, Turn, cache_key, input_digest, internal_host, tag,
};
pub use formats::{Encoder, Format, FormatError, Formats, Release, Stamp};
pub use hold::{Answer, Answered, HeldCall, Hold, HoldError, Resumption};
pub use host::{
    CHECKPOINT_PART_BYTES, Compaction, Extension, LogHost, MemoryLog, MemoryWrites, Rider,
    SCAN_END, ScopeRows, SqlLog, StoredCheckpoint, Write, host_id,
};
pub use image::{FileIo, Header, Image, ImageOptions, Medium};
pub use layouts::{
    CHILD_SCOPE_CHARS, CHILD_SCOPE_DIGEST_BYTES, Layout, LayoutError, child_scope_key,
    child_scope_keyed,
};
pub use patch::apply as apply_patch;
pub use record::{Outcome, Record, Status, decode, encode};
pub use stamps::{Landed, Stamps, tagged_row};
pub use wake::{ArmedWake, Trigger, Unfit, Wake, Wakes, sim as wake_sim};
pub use wake_index::{Statements, WakeIndex, WakeRider, WakeSummary};
pub const CHAIN_WINDOW_BYTES: usize = codec::WINDOW;
pub use writer::{
    EVENT_GAP, FIRST_ROW, Follower, Point, Row, SNAPSHOT_RATIO, Stats, TurnLog, Writing, rows,
};
