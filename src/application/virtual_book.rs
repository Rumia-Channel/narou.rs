//! Ordered virtual-book inputs and collision-free references to member assets.

use std::collections::BTreeMap;

use crate::db::NovelRecord;
use crate::downloader::persistence::{PersistenceService, SectionLoadFailure};
use crate::downloader::{SectionFile, TocFile};
use crate::error::{NarouError, Result};
use crate::illustration_store::{IllustrationIndex, is_remote_illustration_source};
use crate::platform::{NovelFilter, NovelId, NovelObjectKeys, NovelQuery, NovelRepository, NovelSort, ObjectKey};

pub struct VirtualBook {
    pub toc: TocFile,
    pub sections: Vec<SectionFile>,
    /// A book-local image name -> the original member's stored object.
    pub illustrations: BTreeMap<String, ObjectKey>,
}

/// Load only the requested members, retaining registration order independently
/// of repository sort order. Ninety IDs leave room below D1's binding limit.
pub async fn load_member_records(
    repository: &dyn NovelRepository,
    members: &[i64],
) -> Result<BTreeMap<i64, NovelRecord>> {
    let mut records = BTreeMap::new();
    for chunk in members.chunks(90) {
        let query = NovelQuery::page(
            NovelFilter {
                ids: Some(chunk.iter().copied().map(NovelId).collect()),
                ..Default::default()
            },
            NovelSort::default(),
            0,
            chunk.len(),
        );
        for record in repository.query(&query).await? {
            records.insert(record.id, record);
        }
    }
    for id in members {
        let record = records.get(id).ok_or_else(|| NarouError::NotFound(format!(
            "バーチャル小説のメンバー ID {id} が見つかりません",
        )))?;
        if record.ncode.as_deref().is_some_and(crate::virtual_novel::is_virtual_ncode) {
            return Err(NarouError::InvalidTarget(format!("仮想小説をメンバーにはできません: {id}")));
        }
    }
    Ok(records)
}

pub async fn load(
    record: &NovelRecord,
    members: &[i64],
    repository: &dyn NovelRepository,
    persistence: &PersistenceService,
) -> Result<VirtualBook> {
    if members.is_empty() {
        return Err(NarouError::InvalidTarget("バーチャル小説にメンバーがありません".into()));
    }
    let records = load_member_records(repository, members).await?;
    let mut subtitles = Vec::new();
    let mut sections = Vec::new();
    let mut sources: BTreeMap<ObjectKey, String> = BTreeMap::new();
    for id in members {
        let member = &records[id];
        let keys = NovelObjectKeys::new(&member.sitename, &member.file_title, member.use_subdirectory)?;
        let toc = persistence.load_toc(&keys).await?
            .ok_or_else(|| NarouError::NotFound(keys.toc().to_string()))?;
        let (loaded, index) = futures::join!(
            persistence.load_sections_ordered(&keys, &toc.subtitles),
            persistence.load_cache(&keys),
        );
        let loaded = match loaded {
            Ok(sections) => sections,
            Err(SectionLoadFailure::Missing { section_index }) => {
                let subtitle = &toc.subtitles[section_index];
                return Err(NarouError::NotFound(keys.section(&subtitle.index, &subtitle.file_subtitle).to_string()));
            }
            Err(SectionLoadFailure::Read(error)) => return Err(error),
        };
        let index = index?;
        for (position, (mut subtitle, mut section)) in toc.subtitles.into_iter().zip(loaded).enumerate() {
            let sequence = (sections.len() + 1).to_string();
            section.index.clone_from(&sequence);
            section.href.clone_from(&subtitle.href);
            section.chapter = if position == 0 { member.title.clone() } else { String::new() };
            section.subchapter.clear();
            subtitle.index = sequence;
            subtitle.chapter.clone_from(&section.chapter);
            subtitle.subchapter.clear();
            for text in [&mut section.element.introduction, &mut section.element.body, &mut section.element.postscript] {
                let (rewritten, _) = crate::illustration_refs::rewrite(std::mem::take(text), |reference| {
                    let key = image_key(&keys, index.as_ref(), reference)?;
                    let ordinal = sources.len();
                    match sources.entry(key) {
                        std::collections::btree_map::Entry::Occupied(entry) => Some(entry.get().clone()),
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            let (_, extension) = entry.key().as_ref().rsplit_once('.')?;
                            if !extension.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
                                return None;
                            }
                            let alias = format!("挿絵/v{id}-{ordinal}.{extension}");
                            entry.insert(alias.clone());
                            Some(alias)
                        }
                    }
                });
                *text = rewritten;
            }
            subtitles.push(subtitle);
            sections.push(section);
        }
    }
    Ok(VirtualBook {
        toc: TocFile {
            title: record.title.clone(),
            author: record.author.clone(),
            toc_url: record.toc_url.clone(),
            story: None,
            subtitles,
            novel_type: Some(record.novel_type),
        },
        sections,
        illustrations: sources.into_iter().map(|(key, name)| (name, key)).collect(),
    })
}

fn image_key(keys: &NovelObjectKeys, index: Option<&IllustrationIndex>, reference: &str) -> Option<ObjectKey> {
    let decoded = reference.contains('&').then(|| crate::downloader::util::decode_html_text(reference));
    let reference = decoded.as_deref().unwrap_or(reference);
    if let Some(filename) = index.and_then(|index| {
        index.filename_for_source(reference).or_else(|| {
            crate::illustration_store::mitemin_illustration_id(reference)
                .and_then(|id| index.filename_for_mitemin_id(&id))
        })
    }) {
        return keys.illustration(filename).ok();
    }
    if is_remote_illustration_source(reference) {
        return None;
    }
    let normalized;
    let reference = if reference.contains('\\') {
        normalized = reference.replace('\\', "/");
        normalized.as_str()
    } else {
        reference
    };
    let relative = reference.trim_start_matches("./");
    if relative.starts_with('/') || relative.contains(':') {
        return None;
    }
    if let Some(filename) = relative.strip_prefix("挿絵/")
        && !filename.contains('/')
    {
        keys.illustration(filename).ok()
    } else {
        keys.prefix().join(relative).ok()
    }
}
