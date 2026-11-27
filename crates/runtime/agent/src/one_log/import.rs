use nanocode_turnlog::{Point, Record};

use super::{OneLog, staging, stored_key, upgraded};
use crate::durability::{StoreError, StoreRecord};

pub mod sim {
    use std::sync::atomic::{AtomicU8, Ordering};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u8)]
    pub enum Mutation {
        None,
        HeldSkipsCommit,
        ReimportAdvances,
        ExportSkipsCommit,
        ExportSkipsKeys,
    }

    impl Mutation {
        pub const ALL: [Self; 4] = [
            Self::HeldSkipsCommit,
            Self::ReimportAdvances,
            Self::ExportSkipsCommit,
            Self::ExportSkipsKeys,
        ];

        #[must_use]
        pub const fn name(self) -> &'static str {
            match self {
                Self::None => "none",
                Self::HeldSkipsCommit => "held_skips_commit",
                Self::ReimportAdvances => "reimport_advances",
                Self::ExportSkipsCommit => "export_skips_commit",
                Self::ExportSkipsKeys => "export_skips_keys",
            }
        }

        #[must_use]
        pub fn parse(name: &str) -> Option<Self> {
            std::iter::once(Self::None)
                .chain(Self::ALL)
                .find(|mutation| mutation.name() == name)
        }
    }

    static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);

    pub fn mutate(mutation: Mutation) {
        MUTATION.store(mutation as u8, Ordering::SeqCst);
    }

    pub fn mutated(mutation: Mutation) -> bool {
        MUTATION.load(Ordering::Relaxed) == mutation as u8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Imported {
    Copied,
    Held,
}

impl OneLog {
    pub async fn import_state(
        &self,
        state_id: &str,
        revision: u64,
        payload: &str,
        records: &[StoreRecord],
    ) -> Result<Imported, StoreError> {
        if revision == 0 {
            return Err(StoreError::NotCommitted(format!(
                "state {state_id} is imported at revision 0, which holds no checkpoint"
            )));
        }
        let this = self.weak();
        let state_id = state_id.to_owned();
        let payload = payload.to_owned();
        let records = records.to_vec();
        self.writes(move |writing| Box::pin(async move {
let this = upgraded(&this);
let state_id = state_id.as_str();
let payload = payload.as_str();
let records = records.as_slice();
        if this.owners.read(|owners| owners.get(state_id).is_some()) {
            return Err(StoreError::Fenced);
        }
        staging(writing, state_id, records).await?;
        let fold = writing.fold();
        let unsaved = records
            .iter()
            .map(|record| (record, super::shared::staged(fold, state_id, record)))
            .filter(|(record, value)| {
                fold.stored(&stored_key(state_id, &record.key)) != Some(&**value)
            })
            .map(|(record, value)| (record, value.into_owned()))
            .collect::<Vec<_>>();
        let (imported, written) = match fold.checkpoint(state_id) {
            None => (Imported::Copied, revision),
            Some(held) if held.revision != revision => {
                return Err(StoreError::Conflict {
                    expected: revision,
                    actual: held.revision,
                });
            }
            Some(held) if held.payload != payload => {
                return Err(StoreError::NotCommitted(format!(
                    "turn log {} holds state {state_id} at revision {revision} with a different payload",
                    this.scope()
                )));
            }
            Some(_) if !unsaved.is_empty() => {
                return Err(StoreError::NotCommitted(format!(
                    "turn log {} holds state {state_id} at revision {revision} with a different record {}",
                    this.scope(),
                    unsaved[0].0.key
                )));
            }
            Some(_) if sim::mutated(sim::Mutation::ReimportAdvances) => {
                (Imported::Copied, revision + 1)
            }
            Some(_) => (Imported::Held, revision),
        };
        let pushed = imported == Imported::Copied;
        if pushed {
            let unsaved = unsaved
                .into_iter()
                .map(|(record, value)| Record::Stored {
                    key: stored_key(state_id, &record.key),
                    value,
                })
                .collect::<Vec<_>>();
            for record in unsaved {
                writing.push(record)?;
            }
            writing.push(Record::Checkpoint {
                payload: payload.to_owned(),
                revision: written,
                state: state_id.to_owned(),
            })?;
        }
        let owed = writing.buffered() > 0;
        assert!(
            !pushed || owed,
            "an import that pushed records has them buffered"
        );
        if owed && (pushed || !sim::mutated(sim::Mutation::HeldSkipsCommit)) {
            writing.commit(Point::Progress).await?;
        }
        Ok(imported)
}))
.await
    }
}
