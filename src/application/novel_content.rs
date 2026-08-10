//! Platform-neutral access to serialized novel content objects.
//!
//! The service resolves a novel's logical object namespace through the
//! repository and reads bounded content objects through `ObjectStore`. It
//! deliberately returns bytes so the application layer does not depend on
//! downloader-specific serialization types; presentation code may decode the
//! existing YAML format at its boundary.

use std::sync::Arc;

use crate::application::error::ApplicationError;
use crate::platform::{NovelId, NovelObjectKeys, NovelRepository, ObjectStore};

pub struct NovelContentService {
    novels: Arc<dyn NovelRepository>,
    objects: Arc<dyn ObjectStore>,
}

impl NovelContentService {
    pub fn new(novels: Arc<dyn NovelRepository>, objects: Arc<dyn ObjectStore>) -> Self {
        Self { novels, objects }
    }

    pub async fn toc(&self, id: NovelId) -> Result<Option<Vec<u8>>, ApplicationError> {
        let keys = self.keys_for(id).await?;
        self.objects
            .read_small(&keys.toc())
            .await
            .map_err(ApplicationError::platform)
    }
    pub async fn diff(&self, id: NovelId) -> Result<Option<Vec<u8>>, ApplicationError> {
        let keys = self.keys_for(id).await?;
        self.objects
            .read_small(&keys.diff())
            .await
            .map_err(ApplicationError::platform)
    }

    pub async fn section(
        &self,
        id: NovelId,
        index: &str,
        file_subtitle: &str,
    ) -> Result<Option<Vec<u8>>, ApplicationError> {
        let keys = self.keys_for(id).await?;
        let key = keys.section(index, file_subtitle);
        self.objects
            .read_small(&key)
            .await
            .map_err(ApplicationError::platform)
    }

    async fn keys_for(&self, id: NovelId) -> Result<NovelObjectKeys, ApplicationError> {
        let record = self
            .novels
            .get(id)
            .await
            .map_err(ApplicationError::platform)?
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
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn reads_toc_through_logical_key() {
        let novels = Arc::new(MemoryNovelRepository::from_records(vec![record()]));
        let objects = Arc::new(MemoryObjectStore::new());
        let keys = NovelObjectKeys::new("example", "title", false).unwrap();
        futures::executor::block_on(objects.write_small(&keys.toc(), b"toc".to_vec())).unwrap();
        let service = NovelContentService::new(novels, objects);
        let bytes = futures::executor::block_on(service.toc(NovelId(1)))
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"toc");
    }
}
