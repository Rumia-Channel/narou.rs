use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::platform::{
    AssetStore, Clock, NovelObjectKeys, ObjectListRequest, ObjectPrefix, ObjectStore, SystemClock,
};

use super::types::{SectionElement, SectionFile, SubtitleInfo, TocFile};

pub fn compute_section_hash(section: &SectionElement) -> String {
    let mut hasher = Sha256::new();
    hasher.update(section.body.as_bytes());
    hasher.update(section.introduction.as_bytes());
    hasher.update(section.postscript.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn section_filename(subtitle: &SubtitleInfo) -> String {
    let safe_subtitle =
        crate::downloader::util::sanitize_filename_with_limit(&subtitle.file_subtitle, None);
    format!("{} {}.yaml", subtitle.index, safe_subtitle)
}

pub fn raw_filename(subtitle: &SubtitleInfo) -> String {
    let section = section_filename(subtitle);
    let stem = section.strip_suffix(".yaml").unwrap_or(&section);
    format!("{stem}.html")
}

/// Storage-backed downloader persistence.
///
/// The codec functions below remain pure; this service owns only the
/// ObjectStore/AssetStore calls and the injected clock.
#[derive(Clone)]
pub struct PersistenceService {
    objects: Arc<dyn ObjectStore>,
    assets: Arc<dyn AssetStore>,
    clock: Arc<dyn Clock>,
}

impl PersistenceService {
    pub fn new(objects: Arc<dyn ObjectStore>, assets: Arc<dyn AssetStore>) -> Self {
        Self::with_clock(objects, assets, Arc::new(SystemClock))
    }

    pub fn with_clock(
        objects: Arc<dyn ObjectStore>,
        assets: Arc<dyn AssetStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            objects,
            assets,
            clock,
        }
    }

    pub fn objects(&self) -> &Arc<dyn ObjectStore> {
        &self.objects
    }

    pub async fn load_toc(&self, keys: &NovelObjectKeys) -> Result<Option<TocFile>> {
        let Some(bytes) = self.objects.read_small(&keys.toc()).await? else {
            return Ok(None);
        };
        Ok(Some(deserialize_toc(&bytes)?))
    }

    pub async fn save_toc(&self, keys: &NovelObjectKeys, toc: &TocFile) -> Result<()> {
        self.objects
            .write_small(&keys.toc(), serialize_toc(toc)?.into_bytes())
            .await
    }

    pub async fn load_section(
        &self,
        keys: &NovelObjectKeys,
        subtitle: &SubtitleInfo,
    ) -> Result<Option<SectionFile>> {
        let Some(bytes) = self
            .objects
            .read_small(&keys.section(&subtitle.index, &subtitle.file_subtitle))
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(deserialize_section(&bytes)?))
    }

    pub async fn save_section(
        &self,
        keys: &NovelObjectKeys,
        subtitle: &SubtitleInfo,
        section: &SectionElement,
    ) -> Result<String> {
        let download_time = self
            .clock
            .now_utc()
            .format("%Y-%m-%d %H:%M:%S%.6f %z")
            .to_string();
        let file_data = SectionFile {
            index: subtitle.index.clone(),
            href: subtitle.href.clone(),
            chapter: subtitle.chapter.clone(),
            subchapter: subtitle.subchapter.clone(),
            subtitle: subtitle.subtitle.clone(),
            file_subtitle: subtitle.file_subtitle.clone(),
            subdate: subtitle.subdate.clone(),
            subupdate: subtitle.subupdate.clone(),
            download_time: Some(download_time.clone()),
            element: section.clone(),
        };
        self.objects
            .write_small(
                &keys.section(&subtitle.index, &subtitle.file_subtitle),
                serialize_section(&file_data)?.into_bytes(),
            )
            .await?;
        Ok(download_time)
    }

    pub async fn section_needs_update(
        &self,
        keys: &NovelObjectKeys,
        subtitle: &SubtitleInfo,
        new_section: &SectionElement,
    ) -> Result<bool> {
        let Some(existing) = self.load_section(keys, subtitle).await? else {
            return Ok(true);
        };
        Ok(compute_section_hash(&existing.element) != compute_section_hash(new_section))
    }

    /// List every object name under the novel prefix in one listing pass.
    ///
    /// One `list_page` round trip (or one bounded directory walk on native)
    /// replaces a per-section `read_small`/`stat` probe. Callers that only
    /// need existence should prefer this over N individual reads.
    pub async fn list_novel_object_names(
        &self,
        keys: &NovelObjectKeys,
    ) -> Result<HashSet<String>> {
        let prefix = ObjectPrefix::from(keys.prefix().clone());
        let mut names = HashSet::new();
        let mut request = ObjectListRequest::new(prefix, NonZeroUsize::new(1000).unwrap());
        loop {
            let page = self.objects.list_page(&request).await?;
            for object in &page.objects {
                if let Some(name) = object
                    .key
                    .as_ref()
                    .strip_prefix(keys.prefix().as_ref())
                    .and_then(|rest| rest.strip_prefix('/'))
                {
                    names.insert(name.to_string());
                }
            }
            match page.next_cursor {
                Some(cursor) => request = request.after(cursor),
                None => break,
            }
        }
        Ok(names)
    }

    /// Section indices that already have a persisted `本文/` file.
    ///
    /// Matches both the canonical `<index> <file_subtitle>.yaml` name and the
    /// legacy `<index>.yaml` / `<index> <old subtitle>.yaml` fallback that
    /// `resolve_read_path` accepts on native.
    pub fn existing_section_indices(names: &HashSet<String>) -> HashSet<String> {
        names
            .iter()
            .filter_map(|name| {
                let file = name.strip_prefix("本文/")?;
                if file.starts_with('.') || !file.ends_with(".yaml") {
                    return None;
                }
                let stem = file.strip_suffix(".yaml")?;
                let index = stem.split_once(' ').map(|(index, _)| index).unwrap_or(stem);
                if index.is_empty() {
                    None
                } else {
                    Some(index.to_string())
                }
            })
            .collect()
    }

    pub async fn save_raw(
        &self,
        keys: &NovelObjectKeys,
        subtitle: &SubtitleInfo,
        raw_html: &str,
    ) -> Result<()> {
        if raw_html.is_empty() {
            return Ok(());
        }
        self.objects
            .write_small(
                &keys.raw_section(&subtitle.index, &subtitle.file_subtitle),
                raw_html.as_bytes().to_vec(),
            )
            .await
    }

    pub async fn move_section_to_cache(
        &self,
        keys: &NovelObjectKeys,
        timestamp: &str,
        subtitle: &SubtitleInfo,
    ) -> Result<()> {
        let source = keys.section(&subtitle.index, &subtitle.file_subtitle);
        let destination = keys.cached_section(timestamp, &subtitle.index, &subtitle.file_subtitle);
        self.assets.move_or_copy(&source, &destination).await
    }

    /// Write `setting.ini` / `replace.txt` defaults for files that do not yet
    /// exist. When `existing_names` is provided (from
    /// [`Self::list_novel_object_names`]) existence is decided from the set
    /// with no extra storage round trips; otherwise each file is probed with
    /// `stat`.
    pub async fn ensure_default_files(
        &self,
        keys: &NovelObjectKeys,
        title: &str,
        author: &str,
        toc_url: &str,
        existing_names: Option<&HashSet<String>>,
    ) -> Result<()> {
        let setting_exists = match existing_names {
            Some(names) => names.contains("setting.ini"),
            None => self.objects.stat(&keys.setting()).await?.is_some(),
        };
        if !setting_exists {
            self.objects
                .write_small(&keys.setting(), default_setting_ini().into_bytes())
                .await?;
        }
        let replace_exists = match existing_names {
            Some(names) => names.contains("replace.txt"),
            None => self.objects.stat(&keys.replace()).await?.is_some(),
        };
        if !replace_exists {
            self.objects
                .write_small(
                    &keys.replace(),
                    default_replace_txt(title, author, toc_url).into_bytes(),
                )
                .await?;
        }
        Ok(())
    }

    pub async fn load_cache(
        &self,
        keys: &NovelObjectKeys,
    ) -> Result<Option<crate::illustration_store::IllustrationIndex>> {
        let Some(bytes) = self.objects.read_small(&keys.illustration_cache()).await? else {
            return Ok(None);
        };
        Ok(Some(serde_yaml::from_slice(&bytes)?))
    }

    pub async fn save_cache(
        &self,
        keys: &NovelObjectKeys,
        index: &crate::illustration_store::IllustrationIndex,
    ) -> Result<()> {
        let body = index.to_cache_yaml()?;
        self.objects
            .write_small(&keys.illustration_cache(), body)
            .await
    }
}

pub fn default_setting_ini() -> String {
    crate::converter::ini::IniData::new().to_ini_string()
}

pub fn default_replace_txt(title: &str, author: &str, toc_url: &str) -> String {
    format!(
        "; 単純置換用ファイル\n;\n; 対象小説情報\n; タイトル: {}\n; 作者: {}\n; URL: {}\n;\n; 書式\n; 置換対象<tab>置換文字\n;\n; サンプル\n; 一〇歳\t十歳\n; 第一章\t［＃ゴシック体］第一章［＃ゴシック体終わり］\n;\n; 正規表現での置換などは converter.yaml で対応して下さい\n",
        title, author, toc_url
    )
}

pub fn serialize_section(section: &SectionFile) -> Result<String> {
    let yaml_body = serde_yaml::to_string(section)?;
    Ok(format!("---\n{}\n", yaml_body))
}

pub fn deserialize_section(bytes: &[u8]) -> Result<SectionFile> {
    Ok(serde_yaml::from_slice(bytes)?)
}

pub fn serialize_toc(toc: &TocFile) -> Result<String> {
    let yaml_body = serde_yaml::to_string(toc)?;
    Ok(format!("---\n{}\n", fix_yaml_block_scalar(&yaml_body)))
}

pub fn deserialize_toc(bytes: &[u8]) -> Result<TocFile> {
    Ok(serde_yaml::from_slice(bytes)?)
}

pub fn fix_yaml_block_scalar(yaml: &str) -> String {
    let re = regex::Regex::new(r"(?m)^story:\s*\|[-+]?\s*$").unwrap();
    re.replace_all(yaml, "story: |-").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::types::SectionElement;
    use crate::platform::mocks::MemoryObjectStore;
    use crate::platform::{AssetStore, Clock, ObjectStore};
    use chrono::{DateTime, Utc};
    use std::sync::Arc;

    struct FixedClock(DateTime<Utc>);

    impl Clock for FixedClock {
        fn now_utc(&self) -> DateTime<Utc> {
            self.0
        }
    }

    fn subtitle() -> SubtitleInfo {
        SubtitleInfo {
            index: "1".to_string(),
            href: "https://example.test/1".to_string(),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: "第一話".to_string(),
            file_subtitle: "第一話".to_string(),
            subdate: "2026-08-01".to_string(),
            subupdate: None,
            download_time: None,
        }
    }

    #[test]
    fn persistence_roundtrip_uses_logical_keys_and_fixed_clock() {
        let store = Arc::new(MemoryObjectStore::new());
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store;
        let clock = Arc::new(FixedClock(
            DateTime::parse_from_rfc3339("2026-08-09T01:02:03Z")
                .unwrap()
                .with_timezone(&Utc),
        ));
        let service = PersistenceService::with_clock(objects.clone(), assets, clock);
        let keys = NovelObjectKeys::new("example.test", "n1234ab", true).unwrap();
        let subtitle = subtitle();
        let section = SectionElement {
            data_type: "text".to_string(),
            introduction: String::new(),
            postscript: String::new(),
            body: "本文".to_string(),
        };

        futures::executor::block_on(async {
            let time = service
                .save_section(&keys, &subtitle, &section)
                .await
                .unwrap();
            assert_eq!(time, "2026-08-09 01:02:03.000000 +0000");
            let loaded = service
                .load_section(&keys, &subtitle)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(loaded.element.body, "本文");
            assert_eq!(loaded.download_time.as_deref(), Some(time.as_str()));

            service
                .ensure_default_files(&keys, "題名", "作者", "https://example.test", None)
                .await
                .unwrap();
            service
                .save_toc(
                    &keys,
                    &TocFile {
                        title: "題名".to_string(),
                        author: "作者".to_string(),
                        toc_url: "https://example.test".to_string(),
                        story: None,
                        subtitles: vec![subtitle.clone()],
                        novel_type: Some(1),
                    },
                )
                .await
                .unwrap();
            service
                .ensure_default_files(&keys, "題名", "作者", "https://example.test", None)
                .await
                .unwrap();
        });

        let section_key = keys.section(&subtitle.index, &subtitle.file_subtitle);
        assert!(
            futures::executor::block_on(objects.read_small(&section_key))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            futures::executor::block_on(objects.read_small(&keys.setting()))
                .unwrap()
                .unwrap(),
            default_setting_ini().into_bytes()
        );
    }
}
