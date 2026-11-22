//! Platform-neutral access to serialized novel content objects.
//!
//! The service resolves a novel's logical object namespace through the
//! repository and reads bounded content objects through `ObjectStore`. It
//! deliberately returns bytes so the application layer does not depend on
//! downloader-specific serialization types; presentation code may decode the
//! existing YAML format at its boundary.

use std::sync::Arc;

use crate::application::error::ApplicationError;
use crate::application::novel_access::NovelReadScope;
use crate::platform::{NovelId, NovelObjectKeys, NovelRepository, ObjectPrefix, ObjectStore};

pub struct NovelContentService {
    novels: Arc<dyn NovelRepository>,
    objects: Arc<dyn ObjectStore>,
}

impl NovelContentService {
    pub fn new(novels: Arc<dyn NovelRepository>, objects: Arc<dyn ObjectStore>) -> Self {
        Self { novels, objects }
    }

    pub fn read_scope(&self, hint: &str) -> NovelReadScope {
        NovelReadScope::new(&self.objects, hint)
    }

    pub fn object_store(&self) -> &dyn ObjectStore {
        self.objects.as_ref()
    }

    /// The first lookup discovers only the namespace. Ordinary metadata is
    /// re-read under the original admission and validated again after await.
    pub async fn read_record(
        &self,
        scope: &mut NovelReadScope,
        id: NovelId,
    ) -> Result<Option<crate::db::NovelRecord>, ApplicationError> {
        let Some(initial) = self.novels.get_current(id).await.map_err(ApplicationError::platform)? else { return Ok(None); };
        let keys = NovelObjectKeys::new(&initial.sitename, &initial.file_title, initial.use_subdirectory)
            .map_err(ApplicationError::platform)?;
        let prefix = ObjectPrefix::from(keys.prefix().clone());
        let mut lease = scope.lease_for(&prefix).await.map_err(ApplicationError::platform)?;
        lease.validate().await.map_err(ApplicationError::platform)?;
        let current = self.novels.get_current(id).await.map_err(ApplicationError::platform);
        lease.validate().await.map_err(ApplicationError::platform)?;
        let Some(record) = current? else { return Ok(None); };
        let current_keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)
            .map_err(ApplicationError::platform)?;
        if current_keys.prefix() != keys.prefix() {
            return Err(ApplicationError::Conflict("novel namespace changed during reader admission".into()));
        }
        Ok(Some(record))
    }

    /// Batched metadata snapshots retain every original prefix admission.
    /// Native YAML is read once per pass, not once per returned novel.
    pub async fn read_records(
        &self,
        scope: &mut NovelReadScope,
        ids: &[NovelId],
    ) -> Result<Vec<Option<crate::db::NovelRecord>>, ApplicationError> {
        Self::read_records_from(self.novels.as_ref(), scope, ids).await
    }

    pub(crate) async fn read_records_from(
        novels: &dyn NovelRepository,
        scope: &mut NovelReadScope,
        ids: &[NovelId],
    ) -> Result<Vec<Option<crate::db::NovelRecord>>, ApplicationError> {
        if ids.is_empty() { return Ok(Vec::new()); }
        let initial = novels.get_many_current(ids).await.map_err(ApplicationError::platform)?;
        if initial.len() != ids.len() { return Err(ApplicationError::Platform("current metadata batch is not aligned".into())); }
        let prefixes = initial.into_iter().map(|record| record.map(|record| Self::record_prefix(&record)).transpose()).collect::<Result<Vec<_>, _>>()?;
        Self::read_admitted_records_from(novels, scope, ids, prefixes).await
    }

    pub async fn read_discovered_records(
        &self, scope: &mut NovelReadScope, records: &[crate::db::NovelRecord],
    ) -> Result<Vec<Option<crate::db::NovelRecord>>, ApplicationError> {
        Self::read_discovered_records_from(self.novels.as_ref(), scope, records).await
    }

    pub(crate) async fn read_discovered_records_from(
        novels: &dyn NovelRepository, scope: &mut NovelReadScope, records: &[crate::db::NovelRecord],
    ) -> Result<Vec<Option<crate::db::NovelRecord>>, ApplicationError> {
        let ids: Vec<_> = records.iter().map(|record| NovelId(record.id)).collect();
        let prefixes = records.iter().map(|record| Self::record_prefix(record).map(Some)).collect::<Result<Vec<_>, _>>()?;
        Self::read_admitted_records_from(novels, scope, &ids, prefixes).await
    }

    fn record_prefix(record: &crate::db::NovelRecord) -> Result<ObjectPrefix, ApplicationError> {
        let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory).map_err(ApplicationError::platform)?;
        Ok(ObjectPrefix::from(keys.prefix().clone()))
    }

    async fn read_admitted_records_from(
        novels: &dyn NovelRepository, scope: &mut NovelReadScope, ids: &[NovelId], prefixes: Vec<Option<ObjectPrefix>>,
    ) -> Result<Vec<Option<crate::db::NovelRecord>>, ApplicationError> {
        if ids.is_empty() { return Ok(Vec::new()); }
        for prefix in prefixes.iter().flatten() {
            scope.lease_for(prefix).await.map_err(ApplicationError::platform)?
                .validate().await.map_err(ApplicationError::platform)?;
        }
        let current = novels.get_many_current(ids).await.map_err(ApplicationError::platform)?;
        if current.len() != ids.len() { return Err(ApplicationError::Platform("current metadata batch is not aligned".into())); }
        for (prefix, record) in prefixes.into_iter().zip(&current) {
            let Some(prefix) = prefix else {
                if record.is_some() { return Err(ApplicationError::Conflict("novel appeared during reader admission".into())); }
                continue;
            };
            scope.lease_for(&prefix).await.map_err(ApplicationError::platform)?
                .validate().await.map_err(ApplicationError::platform)?;
            if let Some(record) = record {
                let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory).map_err(ApplicationError::platform)?;
                if keys.prefix().as_ref() != prefix.as_ref().trim_end_matches('/') {
                    return Err(ApplicationError::Conflict("novel namespace changed during reader admission".into()));
                }
            }
        }
        Ok(current)
    }

    /// 1 論理操作 (`scope`) の admission で制御 object を読む。
    /// `read_small` の unguarded 近道は使わず、各 logical open を
    /// `read_stream_leased` 経由にして gate / generation の CAS を通す。
    async fn read_leased(
        &self,
        scope: &mut NovelReadScope,
        key: &crate::platform::ObjectKey,
        prefix: &ObjectPrefix,
    ) -> Result<Option<Vec<u8>>, ApplicationError> {
        scope
            .lease_for(prefix)
            .await
            .map_err(ApplicationError::platform)?
            .read_small(key)
            .await
            .map_err(ApplicationError::platform)
    }

    /// `toc.yaml` を `scope` の reader lease 経由で読む。
    pub async fn toc(
        &self,
        scope: &mut NovelReadScope,
        id: NovelId,
    ) -> Result<Option<Vec<u8>>, ApplicationError> {
        let keys = self.keys_for(scope, id).await?;
        let prefix = ObjectPrefix::from(keys.prefix().clone());
        self.read_leased(scope, &keys.toc(), &prefix).await
    }
    /// `diff.txt` を `scope` の reader lease 経由で読む。
    pub async fn diff(
        &self,
        scope: &mut NovelReadScope,
        id: NovelId,
    ) -> Result<Option<Vec<u8>>, ApplicationError> {
        let keys = self.keys_for(scope, id).await?;
        let prefix = ObjectPrefix::from(keys.prefix().clone());
        self.read_leased(scope, &keys.diff(), &prefix).await
    }

    /// 1 話分の本文 object を `scope` の reader lease 経由で読む。
    pub async fn section(
        &self,
        scope: &mut NovelReadScope,
        id: NovelId,
        index: &str,
        file_subtitle: &str,
    ) -> Result<Option<Vec<u8>>, ApplicationError> {
        let keys = self.keys_for(scope, id).await?;
        let prefix = ObjectPrefix::from(keys.prefix().clone());
        let key = keys.section(index, file_subtitle);
        if let Some(bytes) = self.read_leased(scope, &key, &prefix).await? {
            return Ok(Some(bytes));
        }
        for fallback in keys.legacy_section_fallbacks(index, file_subtitle) {
            if let Some(bytes) = self.read_leased(scope, &fallback, &prefix).await? {
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }

    async fn keys_for(&self, scope: &mut NovelReadScope, id: NovelId) -> Result<NovelObjectKeys, ApplicationError> {
        let record = self
            .read_record(scope, id)
            .await?
            .ok_or_else(|| ApplicationError::NotFound(format!("ID: {}", id.0)))?;
        NovelObjectKeys::new(
            &record.sitename,
            &record.file_title,
            record.use_subdirectory,
        )
        .map_err(ApplicationError::platform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NovelRecord;
    use crate::platform::mocks::{MemoryNovelRepository, MemoryObjectStore};
    use chrono::Utc;

    fn record() -> NovelRecord {
        NovelRecord {
            id: 1,
            author: "author".into(),
            title: "title".into(),
            file_title: "title".into(),
            toc_url: "https://example.com/novel/1".into(),
            sitename: "example".into(),
            novel_type: 1,
            end: false,
            last_update: Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: Some("n1234ab".into()),
            domain: Some("example.com".into()),
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

    #[test]
    fn reads_toc_through_logical_key() {
        let novels = Arc::new(MemoryNovelRepository::from_records(vec![record()]));
        let objects: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
        let keys = NovelObjectKeys::new("example", "title", false).unwrap();
        futures::executor::block_on(objects.write_small(&keys.toc(), b"toc".to_vec())).unwrap();
        let service = NovelContentService::new(novels, objects.clone());
        let mut scope = NovelReadScope::new(&objects, "content-test");
        let bytes = futures::executor::block_on(service.toc(&mut scope, NovelId(1)))
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"toc");
        futures::executor::block_on(scope.release_all()).unwrap();
    }

    #[test]
    fn stale_toc_reader_cannot_open_a_later_generation_section() {
        use crate::platform::{AssetStore, ObjectPrefix};
        futures::executor::block_on(async {
            let novels = Arc::new(MemoryNovelRepository::from_records(vec![record()]));
            let backend = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = backend.clone();
            let keys = NovelObjectKeys::new("example", "title", false).unwrap();
            let prefix = ObjectPrefix::from(keys.prefix().clone());
            let section = keys.section("1", "body");
            objects.write_small(&keys.toc(), b"old toc bytes".to_vec()).await.unwrap();
            objects.write_small(&section, b"old section bytes".to_vec()).await.unwrap();
            let service = NovelContentService::new(novels, objects.clone());
            let mut scope = service.read_scope("stale-section-reader");
            assert_eq!(service.toc(&mut scope, NovelId(1)).await.unwrap().unwrap(), b"old toc bytes");
            let old_lease = scope.lease_for(&prefix).await.unwrap().lease().clone();
            let gate = objects.access_fence().unwrap();
            let epoch = gate.close_gate(&prefix, "replace").await.unwrap();
            assert!(service.section(&mut scope, NovelId(1), "1", "body").await.is_err());
            scope.release_all().await.unwrap();

            let stage = backend.stage_stream(crate::platform::staging::owned_bytes_stream(b"new section bytes".to_vec())).await.unwrap();
            gate.commit_staged(&prefix, "replace", epoch, &stage.key, &section).await.unwrap();
            gate.open_gate(&prefix, "replace", epoch).await.unwrap();
            assert_eq!(objects.read_small(&section).await.unwrap().unwrap(), b"new section bytes");
            assert!(objects.read_stream_leased(&prefix, &old_lease, &section, 0..u64::MAX).await.is_err(), "an old TOC lease must not expose the new section bytes");
        });
    }

    #[test]
    fn bulk_metadata_keeps_order_missing_ids_and_original_admissions() {
        futures::executor::block_on(async {
            let first = record();
            let mut second = record();
            second.id = 2;
            second.file_title = "second".into();
            let repository = Arc::new(MemoryNovelRepository::from_records(vec![first, second]));
            let objects: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
            let content = NovelContentService::new(repository, objects.clone());
            let mut scope = content.read_scope("metadata-batch");
            let ids = [NovelId(2), NovelId(99), NovelId(1)];
            let records = content.read_records(&mut scope, &ids).await.unwrap();
            assert_eq!(records.iter().map(|record| record.as_ref().map(|record| record.id)).collect::<Vec<_>>(), [Some(2), None, Some(1)]);
            let keys = NovelObjectKeys::new("example", "title", false).unwrap();
            objects.access_fence().unwrap().close_gate(&keys.prefix().clone().into(), "metadata-replacement").await.unwrap();
            assert!(content.read_records(&mut scope, &ids).await.is_err(), "a repeated batch must not acquire a replacement epoch");
            scope.release_all().await.unwrap();
        });
    }

    #[test]
    fn section_falls_back_to_raw_and_pre_head_80_char_truncated_keys() {
        futures::executor::block_on(async {
            let novels = Arc::new(MemoryNovelRepository::from_records(vec![record()]));
            let objects: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
            let keys = NovelObjectKeys::new("example", "title", false).unwrap();

            // 1. Ruby-era trailing space raw key
            let raw_key = keys.prefix().join("本文").unwrap().join("1 テスト .yaml").unwrap();
            objects.write_small(&raw_key, b"raw-body".to_vec()).await.unwrap();

            // 2. Pre-HEAD (v0.6.10) 80-char truncated key for a >80-char subtitle
            let long_subtitle = "あ".repeat(90);
            let truncated_key = keys.prefix().join("本文").unwrap()
                .join(format!("2 {}.yaml", "あ".repeat(80))).unwrap();
            objects.write_small(&truncated_key, b"truncated-body".to_vec()).await.unwrap();

            let service = NovelContentService::new(novels, objects.clone());
            let mut scope = service.read_scope("legacy-fallback-reader");
            assert_eq!(service.section(&mut scope, NovelId(1), "1", "テスト ").await.unwrap().as_deref(), Some(&b"raw-body"[..]));
            assert_eq!(service.section(&mut scope, NovelId(1), "2", &long_subtitle).await.unwrap().as_deref(), Some(&b"truncated-body"[..]));

            // Canonical key takes precedence once written
            let canonical_long = keys.section("2", &long_subtitle);
            objects.write_small(&canonical_long, b"canonical-body".to_vec()).await.unwrap();
            assert_eq!(service.section(&mut scope, NovelId(1), "2", &long_subtitle).await.unwrap().as_deref(), Some(&b"canonical-body"[..]));
            scope.release_all().await.unwrap();
        });
    }
}
