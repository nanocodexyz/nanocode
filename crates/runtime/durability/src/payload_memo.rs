use std::sync::Arc;

use nanocode_sync::{Atom, Map};

use crate::{EncodedPayload, Error, Result, shared_store::SharedStore};

#[derive(Clone)]
enum Kept {
    Keeping(Map<Arc<str>, EncodedPayload>),
    Forgotten,
}

pub(crate) struct PayloadMemo(Atom<Kept>);

impl PayloadMemo {
    pub(crate) fn new() -> Self {
        Self(Atom::new(Kept::Keeping(Map::new())))
    }

    pub(crate) fn forget(&self) {
        self.0.set(Kept::Forgotten);
    }

    pub(crate) fn len(&self) -> usize {
        self.0.read(|kept| match kept {
            Kept::Keeping(map) => map.len(),
            Kept::Forgotten => 0,
        })
    }

    fn kept(&self, payloads: &[EncodedPayload]) -> Vec<Option<EncodedPayload>> {
        self.0.read(|kept| match kept {
            Kept::Keeping(map) => payloads
                .iter()
                .map(|payload| map.get(payload.key()).cloned())
                .collect(),
            Kept::Forgotten => vec![None; payloads.len()],
        })
    }

    fn keep(&self, loaded: &[EncodedPayload]) {
        self.0.update(|kept| {
            if let Kept::Keeping(map) = kept {
                for payload in loaded {
                    map.insert(Arc::from(payload.key()), payload.clone());
                }
            }
        });
    }

    pub(crate) async fn load_many(
        &self,
        payloads: &[EncodedPayload],
        store: &SharedStore,
        state_id: &str,
    ) -> Result<Vec<EncodedPayload>> {
        let mut slots = self.kept(payloads);
        let missing: Vec<EncodedPayload> = payloads
            .iter()
            .zip(&slots)
            .filter(|(_, kept)| kept.is_none())
            .map(|(payload, _)| payload.clone())
            .collect();
        if !missing.is_empty() {
            let loaded = EncodedPayload::load_many(&missing, &mut store.clone(), state_id).await?;
            self.keep(&loaded);
            let mut loaded = loaded.into_iter();
            for slot in slots.iter_mut().filter(|slot| slot.is_none()) {
                *slot = loaded.next();
            }
        }
        slots
            .into_iter()
            .map(|slot| {
                slot.ok_or_else(|| Error::InvalidState("payload load lost a payload".into()))
            })
            .collect()
    }
}
