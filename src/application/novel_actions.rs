//! Novel action service: tag editing, freeze/unfreeze, and removal.
//!
//! Owns the business logic behind the web UI's per-novel and batch actions
//! (tag add/remove/replace, freeze/unfreeze, remove). All persistence goes
//! through injected platform ports:
//!
//! - [`NovelRepository`] for record reads and [`NovelRepository::apply_batch`]
//!   record mutations in atomic batches,
//! - [`FreezeStore`] (read-only) to learn the current frozen set for toggles,
//! - [`FreezeMutationStore`] (write) to persist the frozen set,
//! - an optional [`ObjectStore`] for deleting a novel's files on removal.
//!
//! The service never touches the filesystem, the database globals, the
//! inventory, or the web framework. When no object store is injected, removal
//! still deletes the record but reports the file deletion as explicitly
//! unavailable — it never silently claims files were deleted.

use std::collections::HashSet;
use std::sync::Arc;
use super::novel_access::NovelWriteGuard;

use crate::application::error::ApplicationError;
use crate::application::events::FreezeStore;
use crate::db::NovelRecord;
use crate::platform::repository::{FreezeReadMode, record_is_frozen};
use crate::platform::{
    NovelId, NovelMutation, NovelObjectKeys, NovelRepository, WriteFence,
    GateState, ObjectListRequest, ObjectPrefix, ObjectStore, PlatformFuture, is_reserved_key,
};

/// Maximum number of tags accepted in one request (mirrors the web layer).
pub const MAX_TAGS_PER_REQUEST: usize = 128;
/// Maximum length of a single tag (mirrors the web layer).
pub const MAX_TAG_LENGTH: usize = 255;

/// The `modified` tag native `--gl` checks set and a successful update clears
/// (`commands::update::MODIFIED_TAG` — kept identical by name and value so
/// the Worker writes the same tag rows).
const MODIFIED_TAG: &str = "modified";
/// What to do with the requested tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagAction {
    /// Append the tags that are not already present.
    Add,
    /// Remove the given tags.
    Remove,
    /// Replace the whole tag list with the given tags.
    Replace,
}

/// A tag edit request for one or more novels.
#[derive(Debug, Clone)]
pub struct TagChangeRequest {
    pub ids: Vec<NovelId>,
    pub action: TagAction,
    pub tags: Vec<String>,
}

/// Outcome of a tag edit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagChangeResult {
    /// Records whose tags actually changed.
    pub updated: Vec<NovelId>,
    /// Records that were loaded but already satisfied the request.
    pub unchanged: Vec<NovelId>,
    /// Requested ids with no record in the repository.
    pub missing: Vec<NovelId>,
}

/// Persistence port for the frozen-novel id set.
///
/// Desktop: the freeze inventory (`freeze.yaml`). Worker: a freeze table.
/// This is the write side of [`FreezeStore`]. The service persists it before
/// synchronizing record tags, so a failed store write leaves tags unchanged.
pub trait FreezeMutationStore: Send + Sync {
    /// Same original writer token as the record mutation; unsupported stores
    /// fail closed rather than perform a late, unfenced freeze.
    fn set_frozen_guarded<'a>(
        &'a self, fence: &'a WriteFence, ids: &'a [NovelId], frozen: bool,
    ) -> PlatformFuture<'a, crate::error::Result<()>> {
        self.set_frozen_guarded_many(std::slice::from_ref(fence), ids, frozen)
    }

    fn set_frozen_guarded_many<'a>(
        &'a self, fences: &'a [WriteFence], ids: &'a [NovelId], frozen: bool,
    ) -> PlatformFuture<'a, crate::error::Result<()>> {
        let _ = (fences, ids, frozen);
        Box::pin(async { Err(crate::error::NarouError::Platform("epoch-guarded grouped freeze updates unsupported".into())) })
    }

    /// Mark `ids` as frozen (`frozen = true`) or unfrozen (`frozen = false`).
    fn set_frozen<'a>(
        &'a self,
        ids: &'a [NovelId],
        frozen: bool,
    ) -> PlatformFuture<'a, crate::error::Result<()>>;
}

/// In-memory freeze store for tests. Guarded writes use the captured
/// memory-store proof, so expired operation epochs cannot edit the set.
#[derive(Debug, Default)]
pub struct MemoryFreezeMutationStore {
    frozen: parking_lot::Mutex<HashSet<i64>>,
}

impl MemoryFreezeMutationStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The currently stored frozen ids (for assertions in tests).
    pub fn frozen_ids(&self) -> HashSet<i64> {
        self.frozen.lock().clone()
    }
}

impl FreezeMutationStore for MemoryFreezeMutationStore {
    fn set_frozen_guarded_many<'a>(
        &'a self, fences: &'a [WriteFence], ids: &'a [NovelId], frozen: bool,
    ) -> PlatformFuture<'a, crate::error::Result<()>> {
        Box::pin(async move {
            WriteFence::run_many_proven(fences, false, &mut || {
                let mut set = self.frozen.lock();
                for id in ids {
                    if frozen { set.insert(id.0); } else { set.remove(&id.0); }
                }
                Ok(())
            })
        })
    }
    fn set_frozen<'a>(
        &'a self,
        ids: &'a [NovelId],
        frozen: bool,
    ) -> PlatformFuture<'a, crate::error::Result<()>> {
        let ids: Vec<i64> = ids.iter().map(|id| id.0).collect();
        Box::pin(async move {
            let mut set = self.frozen.lock();
            for id in ids {
                if frozen {
                    set.insert(id);
                } else {
                    set.remove(&id);
                }
            }
            Ok(())
        })
    }
}


/// Freeze/unfreeze request for one or more novels.
#[derive(Debug, Clone)]
pub struct FreezeRequest {
    pub ids: Vec<NovelId>,
    /// `true` freezes, `false` unfreezes.
    pub freeze: bool,
}

/// Outcome of a freeze/unfreeze action.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FreezeResult {
    /// Ids that ended up frozen.
    pub frozen: Vec<NovelId>,
    /// Ids that ended up unfrozen.
    pub unfrozen: Vec<NovelId>,
    /// Requested ids with no record in the repository.
    pub missing: Vec<NovelId>,
    /// Ids whose freeze-store persistence failed; their tags are unchanged.
    pub store_failed: Vec<NovelId>,
}

/// Removal request for one or more novels.
#[derive(Debug, Clone)]
pub struct RemoveRequest {
    pub ids: Vec<NovelId>,
    /// When `true`, also delete the novel's object-store files (toc,
    /// sections, illustrations, ...) via the injected [`ObjectStore`].
    pub delete_files: bool,
}

/// Per-id file deletion outcome for a removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileDeletionStatus {
    /// All objects under the novel's key prefix were deleted.
    Deleted,
}

/// Outcome of a removal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoveResult {
    /// Ids whose records were removed.
    pub removed: Vec<NovelId>,
    /// Requested ids with no record in the repository.
    pub missing: Vec<NovelId>,
    /// File deletion status per removed id. Empty when `delete_files` was
    /// not requested. Unavailable fencing or failed deletion returns an error
    /// instead of acknowledging an unprotected record removal.
    pub files: Vec<(NovelId, FileDeletionStatus)>,
}

/// Concrete novel action service.
///
/// Dependencies are injected platform ports shared by native and Worker.
/// Memory adapters exercise the same admission and guarded-write contracts.
pub struct NovelActionService {
    novels: Arc<dyn NovelRepository>,
    freeze_store: Arc<dyn FreezeStore>,
    freeze_mutations: Arc<dyn FreezeMutationStore>,
    objects: Option<Arc<dyn ObjectStore>>,
}

impl NovelActionService {
    /// Create the service with injected dependencies.
    ///
    /// Mutations require `Some(objects)` with a real access fence. `None`
    /// supports reads only; writes fail closed, including record-only removal.
    pub fn new(
        novels: Arc<dyn NovelRepository>,
        freeze_store: Arc<dyn FreezeStore>,
        freeze_mutations: Arc<dyn FreezeMutationStore>,
        objects: Option<Arc<dyn ObjectStore>>,
    ) -> Self {
        Self {
            novels,
            freeze_store,
            freeze_mutations,
            objects,
        }
    }

    /// Resolve the namespace, capture once, then load the mutation input under
    /// that origin. The first metadata lookup is only namespace discovery; it
    /// is never the stale record later committed after a transfer.
    async fn mutation_records(&self, ids: &[NovelId]) -> Result<Vec<Option<(NovelRecord, NovelWriteGuard)>>, ApplicationError> {
        if ids.is_empty() { return Ok(Vec::new()); }
        let initial = self.novels.get_many_current(ids).await.map_err(ApplicationError::platform)?;
        if initial.len() != ids.len() { return Err(ApplicationError::Platform("current mutation metadata is not aligned".into())); }
        let mut captured = std::collections::BTreeMap::new();
        let mut origins = Vec::with_capacity(ids.len());
        for (id, record) in ids.iter().zip(initial) {
            let Some(record) = record else { origins.push(None); continue; };
            let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory).map_err(ApplicationError::platform)?;
            let guard = match captured.get(id) {
                Some(guard) => NovelWriteGuard::clone(guard),
                None => {
                    let objects = self.objects.as_ref().ok_or_else(|| ApplicationError::Platform("novel access fence unavailable".into()))?;
                    let guard = NovelWriteGuard::capture_for(objects, None, &keys).await.map_err(ApplicationError::platform)?;
                    captured.insert(*id, guard.clone());
                    guard
                }
            };
            if keys.prefix().as_ref() != guard.prefix().as_ref().trim_end_matches('/') {
                return Err(ApplicationError::Conflict("novel namespace changed during mutation admission".into()));
            }
            origins.push(Some(guard));
        }
        let current = self.novels.get_many_current(ids).await.map_err(ApplicationError::platform)?;
        if current.len() != ids.len() { return Err(ApplicationError::Platform("current mutation metadata is not aligned".into())); }
        let mut records = Vec::with_capacity(ids.len());
        for (origin, record) in origins.into_iter().zip(current) {
            match (origin, record) {
                (Some(guard), Some(record)) => {
                    let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory).map_err(ApplicationError::platform)?;
                    if keys.prefix().as_ref() != guard.prefix().as_ref().trim_end_matches('/') {
                        return Err(ApplicationError::Conflict("novel namespace changed during mutation admission".into()));
                    }
                    records.push(Some((record, guard)));
                }
                (None, Some(_)) => return Err(ApplicationError::Conflict("novel appeared during mutation admission".into())),
                (_, None) => records.push(None),
            }
        }
        Ok(records)
    }

    /// Apply a tag edit to several novels in one atomic batch.
    pub async fn change_tags(
        &self,
        request: &TagChangeRequest,
    ) -> Result<TagChangeResult, ApplicationError> {
        let tags = validate_tags(&request.tags)?;
        if request.action != TagAction::Replace
            && tags.iter().any(|tag| crate::application::attribute_tags::is_attribute_tag(tag))
        {
            return Err(ApplicationError::InvalidRequest("属性タグは編集できません".into()));
        }
        let mut updated = Vec::new();
        let mut unchanged = Vec::new();
        let mut missing = Vec::new();
        let mut mutations = Vec::new();
        let mut fences = Vec::new();

        for (id, admitted) in request.ids.iter().zip(self.mutation_records(&request.ids).await?) {
            let Some((mut record, guard)) = admitted
            else {
                missing.push(*id);
                continue;
            };
            let before = record.tags.clone();
            match request.action {
                TagAction::Add => {
                    for tag in &tags {
                        if !record.tags.iter().any(|existing| existing == tag) {
                            record.tags.push(tag.clone());
                        }
                    }
                }
                TagAction::Remove => {
                    record.tags.retain(|existing| !tags.contains(existing));
                }
                TagAction::Replace => {
                    if tags.iter().any(|tag| crate::application::attribute_tags::is_attribute_tag(tag) && record.attribute_tag() != Some(tag)) {
                        return Err(ApplicationError::InvalidRequest("属性タグは編集できません".into()));
                    }
                    record.tags = tags.iter()
                        .filter(|tag| !crate::application::attribute_tags::is_attribute_tag(tag))
                        .cloned().collect();
                }
            }
            if record.tags == before {
                unchanged.push(*id);
            } else {
                updated.push(*id);
                fences.push(guard.fence().clone());
                mutations.push(NovelMutation::Upsert(record));
            }
        }

        if !mutations.is_empty() {
            self.novels
                .apply_batch_guarded_many(&fences, mutations)
                .await
                .map_err(ApplicationError::platform)?;
        }

        Ok(TagChangeResult {
            updated,
            unchanged,
            missing,
        })
    }

    /// Freeze the given novels and synchronize their `frozen` tags.
    pub async fn freeze(&self, ids: &[NovelId]) -> Result<FreezeResult, ApplicationError> {
        self.set_frozen(ids, Some(true)).await
    }

    /// Unfreeze the given novels, including legacy tag-only freezes.
    pub async fn unfreeze(&self, ids: &[NovelId]) -> Result<FreezeResult, ApplicationError> {
        self.set_frozen(ids, Some(false)).await
    }

    /// Apply a freeze/unfreeze request.
    pub async fn apply_freeze(
        &self,
        request: &FreezeRequest,
    ) -> Result<FreezeResult, ApplicationError> {
        self.set_frozen(&request.ids, Some(request.freeze)).await
    }

    /// Flip each novel's effective frozen state (inventory or legacy tag).
    pub async fn toggle_freeze(&self, ids: &[NovelId]) -> Result<FreezeResult, ApplicationError> {
        self.set_frozen(ids, None).await
    }

    /// Close each original epoch before enumerating or deleting anything.
    /// The durable owner lets recovery finish an interrupted removal without
    /// ever capturing a new writer or exposing an admission window.
    pub async fn remove(
        &self,
        request: &RemoveRequest,
    ) -> Result<RemoveResult, ApplicationError> {
        let admitted = self.mutation_records(&request.ids).await?;
        let mut removed = Vec::new();
        let mut missing = Vec::new();
        let mut files = Vec::new();
        let mut completed = HashSet::new();
        for (id, admitted) in request.ids.iter().zip(admitted) {
            let Some((_record, guard)) = admitted else {
                missing.push(*id);
                continue;
            };
            if !completed.contains(id) {
                let objects = self.objects.as_ref().ok_or_else(|| ApplicationError::Platform("novel access fence unavailable".into()))?;
                let access = objects.access_fence().ok_or_else(|| ApplicationError::Platform("novel access fence unavailable".into()))?;
                let owner = crate::platform::access_fence::new_removal_owner(*id, request.delete_files).map_err(ApplicationError::platform)?;
                let epoch = access.retire_writer(guard.fence(), &owner).await.map_err(ApplicationError::platform)?;
                if !self.advance_removal(guard.prefix(), &owner, epoch, *id, request.delete_files, usize::MAX).await? {
                    return Err(ApplicationError::platform(crate::platform::access_fence::novel_busy_error(guard.prefix())));
                }
                completed.insert(*id);
            }
            removed.push(*id);
            if request.delete_files { files.push((*id, FileDeletionStatus::Deleted)); }
        }
        Ok(RemoveResult { removed, missing, files })
    }

    /// Advance one page of durable removal owners. A deferred reader or file
    /// budget leaves its owner closed for a later scan, without blocking other
    /// owners. Native startup may use `usize::MAX` to finish all drained files;
    /// Worker cron supplies an explicit small per-owner deletion budget.
    pub async fn recover_removals(
        &self,
        cursor: Option<&str>,
        limit: usize,
        max_files_per_owner: usize,
    ) -> Result<Option<String>, ApplicationError> {
        let objects = self.objects.as_ref().ok_or_else(|| ApplicationError::Platform("novel access fence unavailable".into()))?;
        let access = objects.access_fence().ok_or_else(|| ApplicationError::Platform("novel access fence unavailable".into()))?;
        let (owners, next) = access.pending_removals(cursor, limit).await.map_err(ApplicationError::platform)?;
        for (prefix, owner, epoch) in owners {
            let (id, delete_files) = crate::platform::access_fence::parse_removal_owner(&owner).map_err(ApplicationError::platform)?;
            self.advance_removal(&prefix, &owner, epoch, id, delete_files, max_files_per_owner).await?;
        }
        Ok(next)
    }

    async fn advance_removal(
        &self,
        prefix: &ObjectPrefix,
        owner: &str,
        epoch: u64,
        id: NovelId,
        delete_files: bool,
        max_files: usize,
    ) -> Result<bool, ApplicationError> {
        let objects = self.objects.as_ref().ok_or_else(|| ApplicationError::Platform("novel access fence unavailable".into()))?;
        let access = objects.access_fence().ok_or_else(|| ApplicationError::Platform("novel access fence unavailable".into()))?;
        let info = access.gate_info(prefix).await.map_err(ApplicationError::platform)?;
        if info.state == GateState::Open && info.epoch >= epoch {
            // The original owner already finished. In particular, do not list
            // a namespace that has since been reused in a later open epoch.
            return Ok(true);
        }
        if info.epoch != epoch {
            return Err(ApplicationError::platform(crate::platform::access_fence::gate_epoch_error(prefix, epoch, info.epoch)));
        }
        if info.state != GateState::Closed || info.owner.as_deref() != Some(owner) {
            return Err(ApplicationError::platform(crate::platform::access_fence::gate_owner_error(prefix, info.owner.as_deref().unwrap_or("missing"))));
        }
        if info.pending_readers != 0 { return Ok(false); }
        if delete_files {
            let illustration_prefix = ObjectPrefix::new(format!("{}/挿絵", prefix.as_ref().trim_end_matches('/'))).map_err(ApplicationError::platform)?;
            let mut remaining = max_files;
            for listed_prefix in [prefix.clone(), illustration_prefix] {
                let mut request = ObjectListRequest::new(listed_prefix, std::num::NonZeroUsize::new(remaining.clamp(1, 100)).expect("bounded non-zero page"));
                loop {
                    access.renew_gate(prefix, owner, epoch).await.map_err(ApplicationError::platform)?;
                    let page = objects.list_page(&request).await.map_err(ApplicationError::platform)?;
                    for object in page.objects {
                        if is_reserved_key(&object.key) { continue; }
                        if remaining == 0 { return Ok(false); }
                        objects.delete_closed(prefix, owner, epoch, &object.key).await.map_err(ApplicationError::platform)?;
                        remaining -= 1;
                    }
                    match page.next_cursor {
                        Some(next) => {
                            request.cursor = Some(next);
                            request.limit = std::num::NonZeroUsize::new(remaining.clamp(1, 100)).expect("bounded non-zero page");
                        }
                        None => break,
                    }
                }
            }
        }
        access.renew_gate(prefix, owner, epoch).await.map_err(ApplicationError::platform)?;
        self.novels.remove_closed(prefix, owner, epoch, id).await.map_err(ApplicationError::platform)?;
        access.open_gate(prefix, owner, epoch).await.map_err(ApplicationError::platform)?;
        Ok(true)
    }

    /// Store-only freeze write, mirroring upstream `Command::Freeze.execute!`
    /// and native `compat::set_frozen_state`: the freeze store (native
    /// `freeze.yaml`, Worker `frozen_novels`) is the only state written;
    /// record tags are left alone. `frozen`/`404` tags on older rows keep
    /// meaning only on the *read* side (`record_is_frozen*` fallbacks), so
    /// legacy tag-only freezes still render as frozen.
    async fn set_frozen(
        &self,
        ids: &[NovelId],
        frozen: Option<bool>,
    ) -> Result<FreezeResult, ApplicationError> {
        let mut result = FreezeResult::default();
        let mut records = Vec::new();
        let mut seen = HashSet::new();
        let ids: Vec<_> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
        for (id, admitted) in ids.iter().zip(self.mutation_records(&ids).await?) {
            match admitted {
                Some((record, guard)) => records.push((*id, record, guard)),
                None => result.missing.push(*id),
            }
        }
        // The toggle snapshot is fetched only after every operation origin
        // has been captured; no post-publication epoch can bless an old set.
        let current = if frozen.is_none() {
            Some(self.freeze_store.frozen_ids().await.map_err(ApplicationError::platform)?)
        } else { None };
        let mut groups = [(Vec::new(), Vec::new()), (Vec::new(), Vec::new())];
        let mut guards = std::collections::HashMap::new();
        for (id, mut record, guard) in records {
            let freeze = frozen.unwrap_or_else(|| {
                !record_is_frozen(&record, current.as_ref(), FreezeReadMode::InventoryOrTag)
            });
            let (persisted_ids, mutations) = &mut groups[usize::from(!freeze)];
            persisted_ids.push(id);
            guards.insert(id, guard);
            if sync_frozen_tag(&mut record, freeze) {
                mutations.push(NovelMutation::Upsert(record));
            }
        }
        for (freeze, (persisted_ids, mutations)) in [true, false].into_iter().zip(groups) {
            if persisted_ids.is_empty() {
                continue;
            }
            let fences = persisted_ids.iter().map(|id| guards[id].fence().clone()).collect::<Vec<_>>();
            if self.freeze_mutations.set_frozen_guarded_many(&fences, &persisted_ids, freeze).await.is_err() {
                result.store_failed.extend(persisted_ids);
                continue;
            }
            if !mutations.is_empty() {
                self.novels.apply_batch_guarded_many(&fences, mutations).await.map_err(ApplicationError::platform)?;
            }
            if freeze {
                result.frozen.extend(persisted_ids);
            } else {
                result.unfrozen.extend(persisted_ids);
            }
        }
        Ok(result)
    }

    /// `true` when the inventory or the record's `frozen` tag marks this id.
    /// Honoring tag-only rows keeps older frozen records recognizable.
    ///
    /// Read failures fall back to "not frozen": a store/listing hiccup must
    /// never turn into a phantom freeze that skips work.
    pub async fn is_frozen(&self, id: NovelId) -> bool {
        if self
            .freeze_store
            .frozen_ids()
            .await
            .map(|ids| ids.contains(&id.0))
            .unwrap_or(false)
        {
            return true;
        }
        self.novels
            .get_current(id)
            .await
            .ok()
            .flatten()
            .is_some_and(|record| record_is_frozen(&record, None, FreezeReadMode::InventoryOrTag))
    }

    /// Register `id` as gone upstream (404): freeze it and synchronize
    /// the `404` and `frozen` tags with native automatic freezes.
    ///
    /// Returns `Ok(false)` when the id has no record (nothing to freeze).
    /// A guarded freeze-store failure surfaces as `Err`; callers must not
    /// acknowledge an unfenced automatic freeze as successful bookkeeping.
    pub async fn mark_not_found_and_freeze(
        &self,
        guard: &NovelWriteGuard,
        id: NovelId,
    ) -> Result<bool, ApplicationError> {
        let Some(mut record) = self.novels.get_current(id).await.map_err(ApplicationError::platform)?
        else {
            return Ok(false);
        };
        self.freeze_mutations
            .set_frozen_guarded(guard.fence(), &[id], true)
            .await
            .map_err(ApplicationError::platform)?;
        let changed = mark_not_found(&mut record);
        if sync_frozen_tag(&mut record, true) || changed {
            self.novels
                .apply_batch_guarded(guard.fence(), vec![NovelMutation::Upsert(record)])
                .await
                .map_err(ApplicationError::platform)?;
        }
        Ok(true)
    }

    /// Post-update bookkeeping that native `commands::update` runs after a
    /// successful *or* unchanged `update` (its `remove_modified_tag` /
    /// `update_last_check_date` / `sync_end_tag`, in that order):
    ///
    /// 1. drop the `modified` tag (the novel was just checked),
    /// 2. stamp `last_check_date = now`,
    /// 3. synchronize the `end` tag with the record's `end` flag.
    ///
    /// One repository read + one conditional write: the three native
    /// helpers collapse into a single mutation because they only touch the
    /// same record. `Ok(false)` when the id has no record. The caller picks
    /// `now` so tests can pin the timestamp.
    pub async fn record_update_check(
        &self,
        guard: &NovelWriteGuard,
        id: NovelId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, ApplicationError> {
        let Some(mut record) = self.novels.get_current(id).await.map_err(ApplicationError::platform)?
        else {
            return Ok(false);
        };
        let changed = mark_update_checked(&mut record, now);
        if changed {
            self.novels
                .apply_batch_guarded(guard.fence(), vec![NovelMutation::Upsert(record)])
                .await
                .map_err(ApplicationError::platform)?;
        }
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Pure record-state transitions (shared by the async service and the native
// CLI helpers, which persist one batch per update).
// ---------------------------------------------------------------------------

/// Post-update record transition (native `remove_modified_tag` /
/// `update_last_check_date` / `sync_end_tag`, in that order): drop the
/// `modified` tag, stamp `last_check_date`, and sync the `end` tag with the
/// record's `end` flag.
///
/// Returns whether anything changed; the caller persists once when it did.
pub fn mark_update_checked(record: &mut NovelRecord, now: chrono::DateTime<chrono::Utc>) -> bool {
    let mut changed = false;
    if record.tags.iter().any(|tag| tag == MODIFIED_TAG) {
        record.tags.retain(|tag| tag != MODIFIED_TAG);
        changed = true;
    }
    if record.last_check_date != Some(now) {
        record.last_check_date = Some(now);
        changed = true;
    }
    changed |= sync_end_tag(record);
    changed
}

/// Synchronize the completion tag without clearing `modified` or changing
/// the check date (the general-last-update check has already set those).
pub fn sync_end_tag(record: &mut NovelRecord) -> bool {
    let has_end_tag = record.tags.iter().any(|tag| tag == "end");
    match (record.end, has_end_tag) {
        (true, false) => {
            record.tags.push("end".to_string());
            true
        }
        (false, true) => {
            record.tags.retain(|tag| tag != "end");
            true
        }
        _ => false,
    }
}

/// Synchronize the frozen-state tag without disturbing other tags.
pub fn sync_frozen_tag(record: &mut NovelRecord, frozen: bool) -> bool {
    let has_tag = record.tags.iter().any(|tag| tag == "frozen");
    if frozen && !has_tag {
        record.tags.push("frozen".into());
        true
    } else if !frozen && has_tag {
        record.tags.retain(|tag| tag != "frozen");
        true
    } else {
        false
    }
}

/// Tag `record` as gone upstream (`404`; native `narou tag --add 404`).
/// Returns whether the tag was added; the freeze-state write stays with the
/// caller ([`NovelActionService`] or the native compatibility helper).
pub fn mark_not_found(record: &mut NovelRecord) -> bool {
    if record.tags.iter().any(|existing| existing == "404") {
        return false;
    }
    record.tags.push("404".to_string());
    true
}

/// Set or clear the `convert_failure` flag (native `set_convert_failure` /
/// `clear_convert_failure`, Worker `set_convert_failure_flag`). Returns
/// whether the flag changed.
pub fn mark_convert_failure(record: &mut NovelRecord, failed: bool) -> bool {
    if record.convert_failure == failed {
        return false;
    }
    record.convert_failure = failed;
    true
}

/// Validate and normalize a tag list: trim, drop empties, dedupe (first-seen
/// order), enforce the count and length limits.
fn validate_tags(tags: &[String]) -> Result<Vec<String>, ApplicationError> {
    if tags.len() > MAX_TAGS_PER_REQUEST {
        return Err(ApplicationError::InvalidRequest(format!(
            "too many tags: {} (max {})",
            tags.len(),
            MAX_TAGS_PER_REQUEST
        )));
    }
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for raw in tags {
        let tag = raw.trim();
        if tag.is_empty() {
            continue;
        }
        if tag.chars().any(|ch| ch.is_control()) {
            return Err(ApplicationError::InvalidRequest(format!(
                "tag contains control characters: {tag:?}"
            )));
        }
        if tag.len() > MAX_TAG_LENGTH {
            return Err(ApplicationError::InvalidRequest(format!(
                "tag is too long: {tag:?}"
            )));
        }
        if seen.insert(tag.to_string()) {
            normalized.push(tag.to_string());
        }
    }
    Ok(normalized)
}

/// Delete every object under the novel's key prefix, paging through the
/// store. Returns the first error message on failure.
///
/// 挿絵の実体は小説プレフィックス直下ではなく `<prefix>/挿絵/<file>` に
/// あり、`SplitStore` は一覧をプレフィックスの形だけで挿絵側のストアへ
/// 振り分ける (小説プレフィックスの一覧はプライマリ側へ行く)。したがって
/// `asset_backend=s3` の構成では小説プレフィックスの一覧に挿絵が含まれず、
/// 挿絵側の一覧を明示的に消さないと「削除しました」と報告しながら実体が
/// 残る。両方のプレフィックスを同じストア (SplitStore) 経由で消す。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NovelRecord;
    use crate::platform::mocks::{MemoryNovelRepository, MemoryObjectStore};
    use crate::platform::ObjectKey;

    fn sample_record(id: i64) -> NovelRecord {
        NovelRecord {
            id,
            author: "author".into(),
            title: format!("title {id}"),
            file_title: format!("title {id}"),
            toc_url: format!("https://example.com/novel/{id}"),
            sitename: "example".into(),
            novel_type: 1,
            end: false,
            last_update: chrono::Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    fn service(
        repo: Arc<dyn NovelRepository>,
        objects: Option<Arc<dyn ObjectStore>>,
    ) -> NovelActionService {
        NovelActionService::new(
            repo,
            Arc::new(crate::application::events::SystemFreezeStore),
            Arc::new(MemoryFreezeMutationStore::new()),
            Some(objects.unwrap_or_else(|| Arc::new(MemoryObjectStore::new()))),
        )
    }

    fn test_write(service: &NovelActionService, id: i64) -> NovelWriteGuard {
        let record = sample_record(id);
        let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory).unwrap();
        futures::executor::block_on(NovelWriteGuard::capture_for(service.objects.as_ref().unwrap(), None, &keys)).unwrap()
    }

    #[test]
    fn add_remove_replace_tags() {
        let mut record = sample_record(1);
        record.tags = vec!["a".into(), "b".into()];
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![record]));
        let service = service(repo.clone(), None);

        let result = futures::executor::block_on(service.change_tags(&TagChangeRequest {
            ids: vec![NovelId(1)],
            action: TagAction::Add,
            tags: vec!["b".into(), "c".into()],
        }))
        .unwrap();
        assert_eq!(result.updated, vec![NovelId(1)]);
        let record = futures::executor::block_on(repo.get(NovelId(1))).unwrap().unwrap();
        assert_eq!(record.tags, vec!["a", "b", "c"]);

        let result = futures::executor::block_on(service.change_tags(&TagChangeRequest {
            ids: vec![NovelId(1)],
            action: TagAction::Remove,
            tags: vec!["a".into()],
        }))
        .unwrap();
        assert_eq!(result.updated, vec![NovelId(1)]);
        let record = futures::executor::block_on(repo.get(NovelId(1))).unwrap().unwrap();
        assert_eq!(record.tags, vec!["b", "c"]);

        let result = futures::executor::block_on(service.change_tags(&TagChangeRequest {
            ids: vec![NovelId(1)],
            action: TagAction::Replace,
            tags: vec!["x".into(), "y".into()],
        }))
        .unwrap();
        assert_eq!(result.updated, vec![NovelId(1)]);
        let record = futures::executor::block_on(repo.get(NovelId(1))).unwrap().unwrap();
        assert_eq!(record.tags, vec!["x", "y"]);
    }

    #[test]
    fn attribute_tags_cannot_be_added_or_removed_and_survive_replacement() {
        let mut record = sample_record(1);
        record.tags = vec!["手動".into()];
        record.set_attribute_tag("漫画");
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![record]));
        let service = service(repo.clone(), None);
        for (action, tag) in [(TagAction::Add, "小説"), (TagAction::Remove, "漫画")] {
            let result = futures::executor::block_on(service.change_tags(&TagChangeRequest {
                ids: vec![NovelId(1)], action, tags: vec![tag.into()],
            }));
            assert!(matches!(result, Err(ApplicationError::InvalidRequest(_))));
        }
        let replacement = TagChangeRequest { ids: vec![NovelId(1)], action: TagAction::Replace, tags: vec!["別".into()] };
        futures::executor::block_on(service.change_tags(&replacement)).unwrap();
        let record = futures::executor::block_on(repo.get(NovelId(1))).unwrap().unwrap();
        assert_eq!(record.tags, vec!["別"]);
        assert_eq!(record.attribute_tag(), Some("漫画"));
        let clear = TagChangeRequest { ids: vec![NovelId(1)], action: TagAction::Replace, tags: vec![] };
        futures::executor::block_on(service.change_tags(&clear)).unwrap();
        let record = futures::executor::block_on(repo.get(NovelId(1))).unwrap().unwrap();
        assert!(record.tags.is_empty());
        assert_eq!(record.attribute_tag(), Some("漫画"));
    }

    #[test]
    fn tag_change_reports_missing_and_unchanged() {
        let mut record = sample_record(1);
        record.tags = vec!["a".into()];
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![record]));
        let service = service(repo, None);

        let result = futures::executor::block_on(service.change_tags(&TagChangeRequest {
            ids: vec![NovelId(1), NovelId(99)],
            action: TagAction::Add,
            tags: vec!["a".into()],
        }))
        .unwrap();
        assert_eq!(result.unchanged, vec![NovelId(1)]);
        assert_eq!(result.missing, vec![NovelId(99)]);
    }

    #[test]
    fn batch_freeze_synchronizes_tags_and_preserves_other_novels() {
        let mut record = sample_record(1);
        record.tags = vec!["404".into(), "custom".into()];
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![
            record, sample_record(2), sample_record(3),
        ]));
        let store = Arc::new(MemoryFreezeMutationStore::new());
        let service = NovelActionService::new(repo.clone(), store.clone(), store.clone(), Some(Arc::new(MemoryObjectStore::new())));
        futures::executor::block_on(async {
            service.freeze(&[NovelId(3)]).await.unwrap();
            let ids = [NovelId(2), NovelId(1), NovelId(2), NovelId(99)];
            let result = service.freeze(&ids).await.unwrap();
            assert_eq!(result.frozen, vec![NovelId(2), NovelId(1)]);
            assert_eq!(result.missing, vec![NovelId(99)]);
            assert_eq!(store.frozen_ids(), HashSet::from([1, 2, 3]));
            assert_eq!(repo.get(NovelId(1)).await.unwrap().unwrap().tags, vec!["404", "custom", "frozen"]);
            service.freeze(&ids).await.unwrap();
            assert_eq!(repo.get(NovelId(2)).await.unwrap().unwrap().tags, vec!["frozen"]);
            let result = service.unfreeze(&ids).await.unwrap();
            assert_eq!(result.unfrozen, vec![NovelId(2), NovelId(1)]);
            assert_eq!(store.frozen_ids(), HashSet::from([3]));
            assert_eq!(repo.get(NovelId(1)).await.unwrap().unwrap().tags, vec!["404", "custom"]);
            assert!(repo.get(NovelId(2)).await.unwrap().unwrap().tags.is_empty());
            assert!(service.is_frozen(NovelId(3)).await);
            assert!(!service.is_frozen(NovelId(1)).await);
        });
    }

    impl FreezeStore for MemoryFreezeMutationStore {
        fn frozen_ids<'a>(&'a self) -> PlatformFuture<'a, crate::error::Result<HashSet<i64>>> {
            Box::pin(async move { Ok(self.frozen_ids()) })
        }
    }

    #[test]
    fn toggle_freeze_handles_inventory_and_legacy_tag_only_states() {
        let mut legacy = sample_record(1);
        legacy.tags = vec!["frozen".into(), "404".into(), "custom".into()];
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![
            legacy, sample_record(2), sample_record(3),
        ]));
        let store = Arc::new(MemoryFreezeMutationStore::new());
        let service = NovelActionService::new(repo.clone(), store.clone(), store.clone(), Some(Arc::new(MemoryObjectStore::new())));
        futures::executor::block_on(async {
            store.set_frozen(&[NovelId(3)], true).await.unwrap();
            let ids = [NovelId(1), NovelId(2), NovelId(3), NovelId(1)];
            let result = service.toggle_freeze(&ids).await.unwrap();
            assert_eq!(result.frozen, vec![NovelId(2)]);
            assert_eq!(result.unfrozen, vec![NovelId(1), NovelId(3)]);
            assert_eq!(store.frozen_ids(), HashSet::from([2]));
            assert_eq!(repo.get(NovelId(1)).await.unwrap().unwrap().tags, vec!["404", "custom"]);
            assert_eq!(repo.get(NovelId(2)).await.unwrap().unwrap().tags, vec!["frozen"]);
            assert!(!service.is_frozen(NovelId(1)).await);
            assert!(!service.is_frozen(NovelId(3)).await);
        });
    }

    #[test]
    fn remove_without_fence_preserves_the_record() {
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![sample_record(1)]));
        let service = NovelActionService::new(repo.clone(), Arc::new(crate::application::events::SystemFreezeStore), Arc::new(MemoryFreezeMutationStore::new()), None);

        let result = futures::executor::block_on(service.remove(&RemoveRequest {
            ids: vec![NovelId(1)],
            delete_files: true,
        }))
        .unwrap_err();
        assert!(matches!(result, ApplicationError::Platform(_)));
        assert!(futures::executor::block_on(repo.get(NovelId(1))).unwrap().is_some());
    }

    #[test]
    fn remove_with_object_store_deletes_objects() {
        let objects = Arc::new(MemoryObjectStore::new());
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![sample_record(1)]).with_objects(objects.clone()));
        let keys = NovelObjectKeys::new("example", "title 1", false).unwrap();
        futures::executor::block_on(objects.write_small(&keys.toc(), b"toc".to_vec())).unwrap();
        futures::executor::block_on(
            objects.write_small(&keys.setting(), b"x = 1".to_vec()),
        )
        .unwrap();

        let service = service(repo.clone(), Some(objects.clone()));
        let result = futures::executor::block_on(service.remove(&RemoveRequest {
            ids: vec![NovelId(1)],
            delete_files: true,
        }))
        .unwrap();
        assert_eq!(result.removed, vec![NovelId(1)]);
        assert_eq!(result.files, vec![(NovelId(1), FileDeletionStatus::Deleted)]);
        assert!(!futures::executor::block_on(objects.exists(&keys.toc())).unwrap());
        assert!(!futures::executor::block_on(objects.exists(&keys.setting())).unwrap());
    }

    #[test]
    fn removal_closes_before_cleanup_and_recovers_without_touching_reused_bytes() {
        futures::executor::block_on(async {
            let backend = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = backend.clone();
            let repo = Arc::new(MemoryNovelRepository::from_records(vec![sample_record(1)]).with_objects(backend));
            let service = service(repo.clone(), Some(objects.clone()));
            let keys = NovelObjectKeys::new("example", "title 1", false).unwrap();
            let writer = NovelWriteGuard::capture_for(&objects, None, &keys).await.unwrap();
            let prefix = writer.prefix().clone();
            let body = keys.section("1", "body");
            let illustration = keys.illustration("0001.jpg").unwrap();
            for key in [&keys.toc(), &body, &illustration] { writer.write_small(key, b"old bytes".to_vec()).await.unwrap(); }
            let access = objects.access_fence().unwrap();
            let reader = access.admit_reader(&prefix, "removal-reader", crate::platform::READER_LEASE_SECS).await.unwrap();
            assert!(service.remove(&RemoveRequest { ids: vec![NovelId(1)], delete_files: true }).await.is_err());
            let closed = access.gate_info(&prefix).await.unwrap();
            assert_eq!(closed.state, GateState::Closed);
            assert_eq!(closed.epoch, writer.fence().epoch + 1);
            assert_eq!(closed.pending_readers, 1);
            assert!(repo.get_current(NovelId(1)).await.unwrap().is_some());
            assert_eq!(objects.read_small(&body).await.unwrap().unwrap(), b"old bytes");
            let late = keys.section("2", "late-writer");
            assert!(writer.write_small(&late, b"must not escape enumeration".to_vec()).await.is_err());
            assert!(!objects.exists(&late).await.unwrap());
            access.release_reader(&prefix, &reader).await.unwrap();

            service.recover_removals(None, 100, 1).await.unwrap();
            assert_eq!(access.gate_info(&prefix).await.unwrap().state, GateState::Closed);
            assert!(repo.get_current(NovelId(1)).await.unwrap().is_some(), "a bounded unfinished cleanup must not remove metadata or reopen");
            let listing = ObjectListRequest::new(prefix.clone(), std::num::NonZeroUsize::new(100).unwrap());
            assert_eq!(objects.list_page(&listing).await.unwrap().objects.len(), 2);
            service.recover_removals(None, 100, usize::MAX).await.unwrap();
            assert!(repo.get_current(NovelId(1)).await.unwrap().is_none());
            assert!(objects.list_page(&listing).await.unwrap().objects.is_empty());
            assert_eq!(access.gate_info(&prefix).await.unwrap().state, GateState::Open);

            let fresh = NovelWriteGuard::capture_for(&objects, None, &keys).await.unwrap();
            fresh.write_small(&body, b"reused namespace".to_vec()).await.unwrap();
            assert!(service.advance_removal(&prefix, closed.owner.as_deref().unwrap(), closed.epoch, NovelId(1), true, usize::MAX).await.unwrap());
            assert_eq!(objects.read_small(&body).await.unwrap().unwrap(), b"reused namespace");
            assert!(writer.write_small(&body, b"old operation".to_vec()).await.is_err());
            assert_eq!(objects.read_small(&body).await.unwrap().unwrap(), b"reused namespace");
        });
    }

    #[test]
    fn record_only_removal_retains_files_and_recovers_after_metadata_commit() {
        futures::executor::block_on(async {
            let backend = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = backend.clone();
            let repo = Arc::new(MemoryNovelRepository::from_records(vec![sample_record(1)]).with_objects(backend));
            let service = service(repo.clone(), Some(objects.clone()));
            let keys = NovelObjectKeys::new("example", "title 1", false).unwrap();
            let writer = NovelWriteGuard::capture_for(&objects, None, &keys).await.unwrap();
            writer.write_small(&keys.toc(), b"retained toc".to_vec()).await.unwrap();
            let owner = crate::platform::access_fence::new_removal_owner(NovelId(1), false).unwrap();
            let access = objects.access_fence().unwrap();
            let epoch = access.retire_writer(writer.fence(), &owner).await.unwrap();
            repo.remove_closed(writer.prefix(), &owner, epoch, NovelId(1)).await.unwrap();
            // Simulate losing the response before the original owner opens.
            assert_eq!(access.gate_info(writer.prefix()).await.unwrap().state, GateState::Closed);
            service.recover_removals(None, 100, usize::MAX).await.unwrap();
            assert_eq!(access.gate_info(writer.prefix()).await.unwrap().state, GateState::Open);
            assert!(repo.get_current(NovelId(1)).await.unwrap().is_none());
            assert_eq!(objects.read_small(&keys.toc()).await.unwrap().unwrap(), b"retained toc");
            assert!(writer.write_small(&keys.toc(), b"stale".to_vec()).await.is_err());
        });
    }

    /// `asset_backend=s3` のような分割ストアでは、小説プレフィックスの一覧は
    /// プライマリ側にしか行かない (SplitStore は `<prefix>/挿絵` の形だけを
    /// 挿絵側へ回す)。挿絵プレフィックスを明示的に消さないと、実体だけが
    /// 残ったまま「削除しました」と報告してしまう。
    #[test]
    fn remove_with_split_store_deletes_illustration_prefix_too() {
        let primary = Arc::new(MemoryObjectStore::new());
        let illustrations = Arc::new(MemoryObjectStore::new_with_shared_authority(primary.as_ref()));
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![sample_record(1)]).with_objects(primary.clone()));
        let split = Arc::new(crate::platform::SplitStore::new(
            (primary.clone(), primary.clone()),
            (illustrations.clone(), illustrations.clone()),
        ));
        let keys = NovelObjectKeys::new("example", "title 1", false).unwrap();
        futures::executor::block_on(split.write_small(&keys.toc(), b"toc".to_vec())).unwrap();
        let illustration = keys.illustration("0001.jpg").unwrap();
        futures::executor::block_on(
            crate::platform::AssetStore::write_stream(
                split.as_ref(),
                &illustration,
                Box::pin(futures::stream::iter(vec![Ok(b"jpg".to_vec())])),
            ),
        )
        .unwrap();
        // 別小説の似た名前のキーは消さない (prefix 境界)。
        let sibling = ObjectKey::try_new("novels/example/title 12/挿絵2.jpg").unwrap();
        futures::executor::block_on(primary.write_small(&sibling, b"x".to_vec())).unwrap();

        let service = service(repo.clone(), Some(split.clone()));
        let result = futures::executor::block_on(service.remove(&RemoveRequest {
            ids: vec![NovelId(1)],
            delete_files: true,
        }))
        .unwrap();
        assert_eq!(result.removed, vec![NovelId(1)]);
        assert_eq!(result.files, vec![(NovelId(1), FileDeletionStatus::Deleted)]);
        assert!(!futures::executor::block_on(primary.exists(&keys.toc())).unwrap());
        assert!(
            futures::executor::block_on(crate::platform::AssetStore::stat(
                illustrations.as_ref(),
                &illustration
            ))
            .unwrap().is_none(),
            "挿絵の実体が残っている"
        );
        assert!(
            futures::executor::block_on(primary.exists(&sibling)).unwrap(),
            "無関係なキーまで消してはいけない"
        );
    }

    #[test]
    fn tag_validation_rejects_control_characters() {
        let repo = Arc::new(MemoryNovelRepository::new());
        let service = service(repo, None);
        let err = futures::executor::block_on(service.change_tags(&TagChangeRequest {
            ids: vec![NovelId(1)],
            action: TagAction::Add,
            tags: vec!["ba\nd".into()],
        }))
        .unwrap_err();
        assert!(matches!(err, ApplicationError::InvalidRequest(_)));
    }

    #[test]
    fn mark_not_found_and_freeze_synchronizes_404_and_frozen_tags() {
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![sample_record(7)]));
        let freeze_mutations = Arc::new(MemoryFreezeMutationStore::new());
        let service = NovelActionService::new(
            repo.clone(),
            Arc::new(crate::application::events::SystemFreezeStore),
            freeze_mutations.clone(),
            Some(Arc::new(MemoryObjectStore::new())),
        );

        // Missing record → nothing to freeze (native NotFound parity).
        let guard = test_write(&service, 7);
        let done = futures::executor::block_on(service.mark_not_found_and_freeze(&guard, NovelId(9)))
            .unwrap();
        assert!(!done);

        let done = futures::executor::block_on(service.mark_not_found_and_freeze(&guard, NovelId(7)))
            .unwrap();
        assert!(done);
        assert_eq!(freeze_mutations.frozen_ids(), HashSet::from([7]));
        let record = futures::executor::block_on(repo.get(NovelId(7))).unwrap().unwrap();
        assert_eq!(record.tags, vec!["404", "frozen"]);

        // Idempotent: a second 404 must not duplicate the tag.
        futures::executor::block_on(service.mark_not_found_and_freeze(&guard, NovelId(7))).unwrap();
        let record = futures::executor::block_on(repo.get(NovelId(7))).unwrap().unwrap();
        assert_eq!(record.tags, vec!["404", "frozen"]);
    }

    #[test]
    fn record_update_check_clears_modified_stamps_date_and_syncs_end() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-27T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        // modified + stale end tag + no last_check_date → all three fixed.
        let mut record = sample_record(1);
        record.tags = vec!["modified".into(), "end".into()];
        record.end = false;
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![record]));
        let actions = service(repo.clone(), None);
        futures::executor::block_on(actions.record_update_check(&test_write(&actions, 1), NovelId(1), now)).unwrap();
        let record = futures::executor::block_on(repo.get(NovelId(1))).unwrap().unwrap();
        assert!(record.tags.is_empty(), "modified and stale end must go: {:?}", record.tags);
        assert_eq!(record.last_check_date, Some(now));

        // Finished novel without the tag gains `end`.
        let record = { let mut r = sample_record(2); r.end = true; r };
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![record]));
        let actions = service(repo.clone(), None);
        futures::executor::block_on(actions.record_update_check(&test_write(&actions, 2), NovelId(2), now)).unwrap();
        let record = futures::executor::block_on(repo.get(NovelId(2))).unwrap().unwrap();
        assert_eq!(record.tags, vec!["end"]);

        // Missing id is a clean no-op.
        let repo = Arc::new(MemoryNovelRepository::new());
        let actions = service(repo, None);
        let done = futures::executor::block_on(actions.record_update_check(&test_write(&actions, 5), NovelId(5), now)).unwrap();
        assert!(!done);
    }

    /// FreezeStore backed by an in-memory set for `is_frozen` tests.
    struct TestFreezeStore(HashSet<i64>);

    impl crate::application::events::FreezeStore for TestFreezeStore {
        fn frozen_ids<'a>(
            &'a self,
        ) -> PlatformFuture<'a, crate::error::Result<HashSet<i64>>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    #[test]
    fn is_frozen_reads_store_with_legacy_tag_fallback() {
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![
            sample_record(1),
            {
                let mut r = sample_record(2);
                r.tags = vec!["frozen".into()];
                r
            },
        ]));
        let actions = NovelActionService::new(
            repo,
            Arc::new(TestFreezeStore(HashSet::from([1]))),
            Arc::new(MemoryFreezeMutationStore::new()),
            None,
        );
        // Store membership wins.
        assert!(futures::executor::block_on(actions.is_frozen(NovelId(1))));
        // Legacy tag-only freeze is still honored on the read side.
        assert!(futures::executor::block_on(actions.is_frozen(NovelId(2))));
        assert!(!futures::executor::block_on(actions.is_frozen(NovelId(3))));
    }

    #[test]
    fn object_key_roundtrip_for_remove() {
        let keys = NovelObjectKeys::new("example", "n1234ab", true).unwrap();
        assert_eq!(keys.prefix().as_ref(), "novels/example/12/n1234ab");
        assert_eq!(
            keys.toc(),
            ObjectKey::try_new("novels/example/12/n1234ab/toc.yaml").unwrap()
        );
    }
}
