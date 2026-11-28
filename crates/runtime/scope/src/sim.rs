use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    CheckpointIndexesOnly,
    ReadThroughWriter,
    PublishBeforeCommit,
    CheckpointSkipsEpoch,
    EvictWithoutCheckpoint,
    CheckpointRewritesUnchangedParts,
    ForwardWaitsUnbounded,
    ListingSkipsDropped,
    ListingChainUnbounded,
    AdoptionSkipsCheckpoint,
    DeferredRedecided,
    StagedPublished,
    FlushSkipsStaged,
    RiderSplit,
    MeterSkipsWriterTask,
    AdmissionUnbounded,
    ColdLoadDuplicated,
    RoutesPastExpiry,
    LegacyAdmittedIdle,
    ChainInStoreOrder,
    AdoptInRequest,
    BackfillTakesServingLanes,
    ReleaseStopsAtDrain,
    BackfillRereadsHistory,
    BackfillIgnoresServingLoad,
    BackfillStopsAtBudget,
}

impl Mutation {
    pub const ALL: [Self; 19] = [
        Self::CheckpointIndexesOnly,
        Self::ReadThroughWriter,
        Self::PublishBeforeCommit,
        Self::CheckpointSkipsEpoch,
        Self::EvictWithoutCheckpoint,
        Self::CheckpointRewritesUnchangedParts,
        Self::ForwardWaitsUnbounded,
        Self::ListingSkipsDropped,
        Self::ListingChainUnbounded,
        Self::DeferredRedecided,
        Self::StagedPublished,
        Self::FlushSkipsStaged,
        Self::RiderSplit,
        Self::MeterSkipsWriterTask,
        Self::RoutesPastExpiry,
        Self::ChainInStoreOrder,
        Self::ReleaseStopsAtDrain,
        Self::BackfillRereadsHistory,
        Self::BackfillStopsAtBudget,
    ];

    pub const ADMISSION: [Self; 6] = [
        Self::AdmissionUnbounded,
        Self::ColdLoadDuplicated,
        Self::MeterSkipsWriterTask,
        Self::LegacyAdmittedIdle,
        Self::BackfillTakesServingLanes,
        Self::BackfillIgnoresServingLoad,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::CheckpointIndexesOnly => "checkpoint_indexes_only",
            Self::ReadThroughWriter => "read_through_writer",
            Self::PublishBeforeCommit => "publish_before_commit",
            Self::CheckpointSkipsEpoch => "checkpoint_skips_epoch",
            Self::EvictWithoutCheckpoint => "evict_without_checkpoint",
            Self::CheckpointRewritesUnchangedParts => "checkpoint_rewrites_unchanged_parts",
            Self::ForwardWaitsUnbounded => "forward_waits_unbounded",
            Self::ListingSkipsDropped => "listing_skips_dropped",
            Self::ListingChainUnbounded => "listing_chain_unbounded",
            Self::AdoptionSkipsCheckpoint => "adoption_skips_checkpoint",
            Self::DeferredRedecided => "deferred_redecided",
            Self::StagedPublished => "staged_published",
            Self::FlushSkipsStaged => "flush_skips_staged",
            Self::RiderSplit => "rider_split",
            Self::MeterSkipsWriterTask => "meter_skips_writer_task",
            Self::AdmissionUnbounded => "admission_unbounded",
            Self::ColdLoadDuplicated => "cold_load_duplicated",
            Self::RoutesPastExpiry => "routes_past_expiry",
            Self::LegacyAdmittedIdle => "legacy_admitted_idle",
            Self::ChainInStoreOrder => "chain_in_store_order",
            Self::AdoptInRequest => "adopt_in_request",
            Self::BackfillTakesServingLanes => "backfill_takes_serving_lanes",
            Self::ReleaseStopsAtDrain => "release_stops_at_drain",
            Self::BackfillRereadsHistory => "backfill_rereads_history",
            Self::BackfillIgnoresServingLoad => "backfill_ignores_serving_load",
            Self::BackfillStopsAtBudget => "backfill_stops_at_budget",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        std::iter::once(Self::None)
            .chain(Self::ALL)
            .chain(Self::ADMISSION)
            .find(|mutation| mutation.name() == name)
    }
}

static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);

pub fn mutate(mutation: Mutation) {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
}

#[must_use]
pub fn mutated(mutation: Mutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}
