use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    StartsBeforeIntent,
    RetireKeepsClaims,
    SleepsThroughIdle,
    SleepsAfterActing,
    NeverReaps,
    SleepsAfterBriefBlock,
}

impl Mutation {
    pub const ALL: [Self; 7] = [
        Self::None,
        Self::StartsBeforeIntent,
        Self::RetireKeepsClaims,
        Self::SleepsThroughIdle,
        Self::SleepsAfterActing,
        Self::NeverReaps,
        Self::SleepsAfterBriefBlock,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::StartsBeforeIntent => "dispatcher_starts_before_intent",
            Self::RetireKeepsClaims => "dispatcher_retire_keeps_claims",
            Self::SleepsThroughIdle => "dispatcher_sleeps_through_idle",
            Self::SleepsAfterActing => "dispatcher_sleeps_after_acting",
            Self::NeverReaps => "dispatcher_never_reaps",
            Self::SleepsAfterBriefBlock => "dispatcher_sleeps_after_brief_block",
        }
    }
}

static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);

pub fn mutate(mutation: Mutation) {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
}

pub fn mutated(mutation: Mutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}
