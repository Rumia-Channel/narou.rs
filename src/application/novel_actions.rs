//! Novel action service: tag editing, freeze/unfreeze, and removal.
//!
//! Owns the business logic behind the web UI's per-novel and batch actions
//! (tag add/remove/replace, freeze/unfreeze, remove). All persistence goes
//! through injected platform ports:
//!
//! - [`NovelRepository`] for record reads and [`NovelRepository::apply_batch`]
//!   record mutations (one atomic batch per action),
//! - [`FreezeStore`] (read-only) to learn the current frozen set for toggles,
//! - [`FreezeMutationStore`] (write) to persist the frozen set,
//! - an optional [`ObjectStore`] for deleting a novel's files on removal.
//!
//! The service never touches the filesystem, the database globals, the
//! inventory, or the web framework. When no object store is injected, removal
//! still deletes the record but reports the file deletion as explicitly
//! unavailable — it never silently claims files were deleted.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::application::error::ApplicationError;
use crate::application::events::FreezeStore;
use crate::platform::{
    NovelId, NovelMutation, NovelObjectKeys, NovelRepository, ObjectListRequest, ObjectPrefix,
    ObjectStore, PlatformFuture,
};

/// Maximum number of tags accepted in one request (mirrors the web layer).
pub const MAX_TAGS_PER_REQUEST: usize = 128;
/// Maximum length of a single tag (mirrors the web layer).
pub const MAX_TAG_LENGTH: usize = 255;

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
/// This is the write side of [`FreezeStore`]; the service updates it after
/// the record batch so a failed persistence is reported as a partial
/// failure instead of being silently dropped.
pub trait FreezeMutationStore: Send + Sync {
    /// Mark `ids` as frozen (`frozen = true`) or unfrozen (`frozen = false`).
    fn set_frozen<'a>(
        &'a self,
        ids: &'a [NovelId],
        frozen: bool,
    ) -> PlatformFuture<'a, crate::error::Result<()>>;
}

/// In-memory freeze store for tests and for wiring before a real adapter
/// exists. Tracks the frozen set in a `HashSet`.
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

/// A freeze store that accepts mutations but persists nothing.
///
/// Useful when the caller only wants the record-side `frozen` tag and does
/// not track a separate frozen set (e.g. a Worker backend that derives
/// frozen status purely from tags).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopFreezeMutationStore;

impl FreezeMutationStore for NoopFreezeMutationStore {
    fn set_frozen<'a>(
        &'a self,
        _ids: &'a [NovelId],
        _frozen: bool,
    ) -> PlatformFuture<'a, crate::error::Result<()>> {
        Box::pin(async move { Ok(()) })
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
    /// Ids whose record tags were updated but whose freeze-store persistence
    /// failed. The caller may retry persistence for these.
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
    /// `delete_files` was requested but no [`ObjectStore`] was injected.
    Unavailable,
    /// The object store reported an error while deleting.
    Failed(String),
}

/// Outcome of a removal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoveResult {
    /// Ids whose records were removed.
    pub removed: Vec<NovelId>,
    /// Requested ids with no record in the repository.
    pub missing: Vec<NovelId>,
    /// File deletion status per removed id. Empty when `delete_files` was
    /// not requested. The record is removed regardless of file status; the
    /// caller decides how to surface partial file deletion.
    pub files: Vec<(NovelId, FileDeletionStatus)>,
}

/// Concrete novel action service.
///
/// All dependencies are injected platform ports; the service is testable
/// with the platform mocks and usable from both the native web layer and a
/// future Worker backend.
pub struct NovelActionService {
    novels: Arc<dyn NovelRepository>,
    freeze_store: Arc<dyn FreezeStore>,
    freeze_mutations: Arc<dyn FreezeMutationStore>,
    objects: Option<Arc<dyn ObjectStore>>,
}

impl NovelActionService {
    /// Create the service with injected dependencies.
    ///
    /// `objects` is optional: pass `Some` to enable file deletion on remove,
    /// `None` to keep removal record-only (file deletion is then reported as
    /// [`FileDeletionStatus::Unavailable`]).
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

    /// Apply a tag edit to several novels in one atomic batch.
    pub async fn change_tags(
        &self,
        request: &TagChangeRequest,
    ) -> Result<TagChangeResult, ApplicationError> {
        let tags = validate_tags(&request.tags)?;
        let mut updated = Vec::new();
        let mut unchanged = Vec::new();
        let mut missing = Vec::new();
        let mut mutations = Vec::new();

        for id in &request.ids {
            let Some(mut record) = self.novels.get(*id).await.map_err(ApplicationError::platform)?
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
                    record.tags = tags.clone();
                }
            }
            if record.tags == before {
                unchanged.push(*id);
            } else {
                updated.push(*id);
                mutations.push(NovelMutation::Upsert(record));
            }
        }

        if !mutations.is_empty() {
            self.novels
                .apply_batch(mutations)
                .await
                .map_err(ApplicationError::platform)?;
        }

        Ok(TagChangeResult {
            updated,
            unchanged,
            missing,
        })
    }

    /// Freeze the given novels: add the `frozen` tag and persist the ids in
    /// the freeze store.
    pub async fn freeze(&self, ids: &[NovelId]) -> Result<FreezeResult, ApplicationError> {
        self.set_frozen(ids, true).await
    }

    /// Unfreeze the given novels: remove the `frozen` and legacy `404` tags
    /// and drop the ids from the freeze store.
    pub async fn unfreeze(&self, ids: &[NovelId]) -> Result<FreezeResult, ApplicationError> {
        self.set_frozen(ids, false).await
    }

    /// Apply a freeze/unfreeze request.
    pub async fn apply_freeze(
        &self,
        request: &FreezeRequest,
    ) -> Result<FreezeResult, ApplicationError> {
        self.set_frozen(&request.ids, request.freeze).await
    }

    /// Flip the frozen state of the given novels based on the current
    /// [`FreezeStore`] contents.
    pub async fn toggle_freeze(&self, ids: &[NovelId]) -> Result<FreezeResult, ApplicationError> {
        let frozen_ids = self
            .freeze_store
            .frozen_ids()
            .await
            .map_err(ApplicationError::platform)?;
        let (to_freeze, to_unfreeze): (Vec<NovelId>, Vec<NovelId>) = ids
            .iter()
            .copied()
            .partition(|id| !frozen_ids.contains(&id.0));
        let mut result = FreezeResult::default();
        if !to_freeze.is_empty() {
            let frozen = self.set_frozen(&to_freeze, true).await?;
            result.frozen.extend(frozen.frozen);
            result.missing.extend(frozen.missing);
            result.store_failed.extend(frozen.store_failed);
        }
        if !to_unfreeze.is_empty() {
            let unfrozen = self.set_frozen(&to_unfreeze, false).await?;
            result.unfrozen.extend(unfrozen.unfrozen);
            result.missing.extend(unfrozen.missing);
            result.store_failed.extend(unfrozen.store_failed);
        }
        Ok(result)
    }

    /// Remove records (and optionally their files) in one atomic batch.
    ///
    /// The record is always removed when it exists. File deletion is
    /// reported per id and never silently assumed: without an injected
    /// [`ObjectStore`] every id is reported as
    /// [`FileDeletionStatus::Unavailable`].
    pub async fn remove(
        &self,
        request: &RemoveRequest,
    ) -> Result<RemoveResult, ApplicationError> {
        let mut removed = Vec::new();
        let mut missing = Vec::new();
        let mut files = Vec::new();
        let mut mutations = Vec::new();

        for id in &request.ids {
            let Some(record) = self.novels.get(*id).await.map_err(ApplicationError::platform)?
            else {
                missing.push(*id);
                continue;
            };
            removed.push(*id);
            mutations.push(NovelMutation::Remove(*id));

            if !request.delete_files {
                continue;
            }
            let Some(objects) = &self.objects else {
                files.push((*id, FileDeletionStatus::Unavailable));
                continue;
            };
            let keys = NovelObjectKeys::new(
                &record.sitename,
                &record.file_title,
                record.use_subdirectory,
            )
            .map_err(ApplicationError::platform)?;
            match delete_novel_objects(objects.as_ref(), &keys).await {
                Ok(()) => files.push((*id, FileDeletionStatus::Deleted)),
                Err(message) => files.push((*id, FileDeletionStatus::Failed(message))),
            }
        }

        if !mutations.is_empty() {
            self.novels
                .apply_batch(mutations)
                .await
                .map_err(ApplicationError::platform)?;
        }

        Ok(RemoveResult {
            removed,
            missing,
            files,
        })
    }

    async fn set_frozen(
        &self,
        ids: &[NovelId],
        frozen: bool,
    ) -> Result<FreezeResult, ApplicationError> {
        let mut frozen_ids = Vec::new();
        let mut unfrozen_ids = Vec::new();
        let mut persisted_ids = Vec::new();
        let mut missing = Vec::new();
        let mut mutations = Vec::new();

        for id in ids {
            let Some(mut record) = self.novels.get(*id).await.map_err(ApplicationError::platform)?
            else {
                missing.push(*id);
                continue;
            };
            let was_frozen = record.tags.iter().any(|tag| tag == "frozen");
            if frozen {
                if !was_frozen {
                    record.tags.push("frozen".to_string());
                }
                frozen_ids.push(*id);
            } else {
                record.tags.retain(|tag| tag != "frozen" && tag != "404");
                unfrozen_ids.push(*id);
            }
            persisted_ids.push(*id);
            mutations.push(NovelMutation::Upsert(record));
        }

        if !mutations.is_empty() {
            self.novels
                .apply_batch(mutations)
                .await
                .map_err(ApplicationError::platform)?;
        }

        let mut store_failed = Vec::new();
        if !persisted_ids.is_empty()
            && self
                .freeze_mutations
                .set_frozen(&persisted_ids, frozen)
                .await
                .is_err()
        {
            store_failed.extend(persisted_ids);
        }

        Ok(FreezeResult {
            frozen: frozen_ids,
            unfrozen: unfrozen_ids,
            missing,
            store_failed,
        })
    }
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
async fn delete_novel_objects(
    objects: &dyn ObjectStore,
    keys: &NovelObjectKeys,
) -> Result<(), String> {
    let prefix = ObjectPrefix::from(keys.prefix().clone());
    let mut cursor: Option<String> = None;
    loop {
        let request = ObjectListRequest::new(prefix.clone(), NonZeroUsize::new(100).unwrap());
        let request = match &cursor {
            Some(cursor) => request.after(cursor.clone()),
            None => request,
        };
        let page = objects
            .list_page(&request)
            .await
            .map_err(|e| e.to_string())?;
        for object in &page.objects {
            objects.delete(&object.key).await.map_err(|e| e.to_string())?;
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(())
}

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
            objects,
        )
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
    fn freeze_adds_tag_and_unfreeze_removes_frozen_and_404() {
        let mut record = sample_record(1);
        record.tags = vec!["404".into()];
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![record]));
        let service = service(repo.clone(), None);

        let result = futures::executor::block_on(service.freeze(&[NovelId(1)])).unwrap();
        assert_eq!(result.frozen, vec![NovelId(1)]);
        let record = futures::executor::block_on(repo.get(NovelId(1))).unwrap().unwrap();
        assert_eq!(record.tags, vec!["404", "frozen"]);

        let result = futures::executor::block_on(service.unfreeze(&[NovelId(1)])).unwrap();
        assert_eq!(result.unfrozen, vec![NovelId(1)]);
        let record = futures::executor::block_on(repo.get(NovelId(1))).unwrap().unwrap();
        assert!(record.tags.is_empty());
    }

    #[test]
    fn remove_without_object_store_reports_unavailable() {
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![sample_record(1)]));
        let service = service(repo.clone(), None);

        let result = futures::executor::block_on(service.remove(&RemoveRequest {
            ids: vec![NovelId(1)],
            delete_files: true,
        }))
        .unwrap();
        assert_eq!(result.removed, vec![NovelId(1)]);
        assert_eq!(result.files, vec![(NovelId(1), FileDeletionStatus::Unavailable)]);
        assert!(futures::executor::block_on(repo.get(NovelId(1))).unwrap().is_none());
    }

    #[test]
    fn remove_with_object_store_deletes_objects() {
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![sample_record(1)]));
        let objects = Arc::new(MemoryObjectStore::new());
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
    fn object_key_roundtrip_for_remove() {
        let keys = NovelObjectKeys::new("example", "n1234ab", true).unwrap();
        assert_eq!(keys.prefix().as_ref(), "novels/example/12/n1234ab");
        assert_eq!(
            keys.toc(),
            ObjectKey::new("novels/example/12/n1234ab/toc.yaml")
        );
    }
}
