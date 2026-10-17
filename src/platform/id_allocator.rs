//! Process-local ownership for unpublished novel IDs.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{NarouError, Result};
use super::{NovelId, NovelIdReservation};

#[derive(Debug, Default)]
pub(crate) struct IdAllocator {
    next_id: i64,
    generation: i64,
    reserved: BTreeMap<i64, i64>,
    released: BTreeSet<i64>,
}

impl IdAllocator {
    pub(crate) fn with_next_id(next_id: i64) -> Self {
        Self { next_id, ..Self::default() }
    }

    pub(crate) fn allocate(&mut self) -> Result<NovelIdReservation> {
        let generation = self.generation.checked_add(1)
            .ok_or_else(|| NarouError::Platform("Novel ID reservation generation exhausted".into()))?;
        let id = match self.released.pop_first() {
            Some(id) => id,
            None => {
                let id = self.next_id;
                self.next_id = id.checked_add(1)
                    .ok_or_else(|| NarouError::Platform("Novel ID space exhausted".into()))?;
                id
            }
        };
        self.generation = generation;
        self.reserved.insert(id, generation);
        Ok(NovelIdReservation::new(NovelId(id), generation))
    }

    pub(crate) fn commit(&mut self, id: i64) {
        self.next_id = self.next_id.max(id.saturating_add(1));
        self.reserved.remove(&id);
        self.released.remove(&id);
    }

    #[cfg(any(feature = "native-runtime", test))]
    pub(crate) fn reconcile(&mut self, max_id: Option<i64>, mut committed: impl FnMut(i64) -> bool) {
        if let Some(id) = max_id {
            self.next_id = self.next_id.max(id.saturating_add(1));
        }
        self.reserved.retain(|id, _| !committed(*id));
        self.released.retain(|id| !committed(*id));
    }

    pub(crate) fn release(&mut self, reservation: NovelIdReservation, committed: bool) {
        let id = reservation.id().0;
        if self.reserved.get(&id) != Some(&reservation.generation()) {
            return;
        }
        self.reserved.remove(&id);
        if committed {
            self.commit(id);
        } else {
            self.released.insert(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn released_ids_preserve_live_owners_and_reject_stale_releases() {
        let mut ids = IdAllocator::default();
        let failed = ids.allocate().unwrap();
        let stale = NovelIdReservation::new(failed.id(), failed.generation());
        let live = ids.allocate().unwrap();
        ids.release(failed, false);
        let retry = ids.allocate().unwrap();
        assert_eq!(retry.id(), NovelId(0));
        assert_eq!(live.id(), NovelId(1));
        assert_ne!(retry.generation(), stale.generation());
        ids.release(stale, false);
        assert_eq!(ids.allocate().unwrap().id(), NovelId(2));
    }

    #[test]
    fn committed_and_manually_inserted_ids_never_reenter_the_pool() {
        let mut ids = IdAllocator::default();
        let committed = ids.allocate().unwrap();
        ids.commit(committed.id().0);
        ids.release(committed, true);
        let failed = ids.allocate().unwrap();
        ids.release(failed, false);
        ids.commit(1);
        assert_eq!(ids.allocate().unwrap().id(), NovelId(2));
        ids.reconcile(None, |_| false);
        assert_eq!(ids.allocate().unwrap().id(), NovelId(3));
    }
}
