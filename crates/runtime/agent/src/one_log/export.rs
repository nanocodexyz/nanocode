use nanocode_turnlog::{Fold, Point};

use super::{OneLog, STORED_MARK, import::sim, shared};
use crate::durability::{StoreError, StoreRecord};

#[derive(Clone, Debug)]
pub struct Exported {
    pub revision: u64,
    pub payload: String,
    pub records: Vec<StoreRecord>,
}

impl OneLog {
    pub async fn export_state(&self, state_id: &str) -> Result<Option<Exported>, StoreError> {
        let state_id = state_id.to_owned();
        self.writes(move |writing| {
                Box::pin(async move {
                    let state_id = state_id.as_str();
                    if sim::mutated(sim::Mutation::ExportSkipsKeys) {
                        writing.load_owners(&[state_id.to_owned()]).await?;
                    } else {
                        writing.load_owner(state_id).await?;
                    }
                    if writing.buffered() > 0 && !sim::mutated(sim::Mutation::ExportSkipsCommit) {
                        writing.commit(Point::Progress).await?;
                    }
                    let owners =
                        shared::spliced_keys(state_id, records_of(writing.fold(), state_id))?;
                    writing.load_owners(&owners).await?;
                    let fold = writing.fold();
                    let Some(checkpoint) = fold.checkpoint(state_id) else {
                        return Ok(None);
                    };
                    let records = records_of(fold, state_id)
                        .map(|(key, value)| {
                            Ok(StoreRecord {
                                key: key.to_owned(),
                                value: shared::whole(fold, state_id, key, value.to_owned())?.into(),
                            })
                        })
                        .collect::<Result<_, StoreError>>()?;
                    Ok(Some(Exported {
                        revision: checkpoint.revision,
                        payload: checkpoint.payload.clone(),
                        records,
                    }))
                })
            })
            .await
    }
}

pub(super) fn records_of<'a>(
    fold: &'a Fold,
    state_id: &str,
) -> impl Iterator<Item = (&'a str, &'a str)> {
    fold.stored_of(state_id)
        .filter(|(key, _)| *key != STORED_MARK)
}
