//! Virtual-book listings, ordered inputs and collision-free member assets.

use std::collections::BTreeMap;

use crate::db::NovelRecord;
use crate::downloader::persistence::{PersistenceService, SectionLoadFailure};
use crate::downloader::{SectionFile, TocFile};
use crate::error::{NarouError, Result};
use crate::illustration_store::{IllustrationIndex, is_remote_illustration_source};
use crate::platform::{NovelFilter, NovelId, NovelObjectKeys, NovelQuery, NovelRepository, NovelSort, ObjectKey};
use crate::virtual_novel::{AttributeComposition, VirtualNovelMembers};

pub struct VirtualBook {
    pub toc: TocFile,
    pub sections: Vec<SectionFile>,
    /// A book-local image name -> the original member's stored object.
    pub illustrations: BTreeMap<String, ObjectKey>,
}

/// Bound each result page instead of fetching each member separately.
async fn load_records(
    repository: &dyn NovelRepository,
    ids: &[i64],
) -> Result<BTreeMap<i64, NovelRecord>> {
    let mut records = BTreeMap::new();
    for chunk in ids.chunks(90) {
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
    Ok(records)
}

/// Load the requested real members independently of repository sort order.
pub async fn load_member_records(
    repository: &dyn NovelRepository,
    members: &[i64],
) -> Result<BTreeMap<i64, NovelRecord>> {
    let records = load_records(repository, members).await?;
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

pub async fn load_listing(
    repository: &dyn NovelRepository,
    groups: &VirtualNovelMembers,
    sites: &[crate::downloader::site_setting::SiteSetting],
) -> Result<Vec<serde_json::Value>> {
    let mut ids = Vec::with_capacity(groups.groups.len() + groups.groups.values().map(Vec::len).sum::<usize>());
    for (id, members) in &groups.groups {
        ids.push(*id);
        ids.extend_from_slice(members);
    }
    ids.sort_unstable();
    ids.dedup();
    let records = load_records(repository, &ids).await?;
    Ok(list_entries(groups, sites, |id| records.get(&id)))
}

/// Render from already-loaded records; fetching separate pages is not a database
/// transaction. Missing members retain their position and stored classification.
pub fn list_entries<'a>(
    groups: &VirtualNovelMembers,
    sites: &[crate::downloader::site_setting::SiteSetting],
    lookup: impl Fn(i64) -> Option<&'a NovelRecord>,
) -> Vec<serde_json::Value> {
    let mut entries = Vec::with_capacity(groups.groups.len());
    for (id, members) in &groups.groups {
        let Some(record) = lookup(*id) else { continue };
        let mut composition = crate::virtual_novel::validate_members(members).ok()
            .map(|()| AttributeComposition::default());
        let mut titles = Vec::with_capacity(members.len());
        for member_id in members {
            let member = lookup(*member_id);
            titles.push(member.map(|member| member.title.as_str()).unwrap_or_default());
            match member {
                Some(member) if !member.ncode.as_deref().is_some_and(crate::virtual_novel::is_virtual_ncode) => {
                    if let Some(composition) = &mut composition {
                        composition.add(Some(crate::application::attribute_tags::initial_tag(member, sites)));
                    }
                }
                _ => composition = None,
            }
        }
        let classification = composition.and_then(|composition| {
            composition.resolve(None, record.attribute_tag()).ok()
                .map(|tag| (composition.is_mixed(), tag))
        });
        let (mixed, attribute_tag) = classification.unwrap_or((
            false, record.attribute_tag().unwrap_or(crate::application::attribute_tags::NOVEL_TAG),
        ));
        entries.push(serde_json::json!({
            "id": record.id,
            "ncode": crate::virtual_novel::ncode(*id),
            "title": record.title,
            "author": record.author,
            "sitename": record.sitename,
            "domain": record.domain,
            "tags": record.tags,
            "members": members,
            "member_titles": titles,
            "mixed": mixed,
            "attribute_tag": attribute_tag,
            "attribute_editable": classification.is_some(),
        }));
    }
    entries
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::mocks::MemoryNovelRepository;

    fn record(id: i64, attribute: &str) -> NovelRecord {
        serde_yaml::from_str(&format!(
            "id: {id}\nauthor: author\ntitle: Book {id}\nfile_title: book-{id}\ntoc_url: https://fixture.test/{id}/\nsitename: fixture\nlast_update: 2026-01-01T00:00:00Z\nattribute_tag: {attribute}\n"
        )).unwrap()
    }

    #[test]
    fn bulk_listing_keeps_member_order_across_pages_and_missing_members() {
        let mut records: Vec<_> = (1..=100).map(|id| record(id, "小説")).collect();
        let mut parent = record(101, "漫画");
        parent.ncode = Some("vs_101".into());
        records.push(parent);
        let repository = MemoryNovelRepository::from_records(records);
        let mut groups = VirtualNovelMembers::default();
        let ids: Vec<_> = (1..=100).rev().collect();
        groups.insert(101, ids.clone()).unwrap();
        let list = futures::executor::block_on(load_listing(&repository, &groups, &[])).unwrap();
        assert_eq!(list[0]["members"], serde_json::json!(ids));
        assert_eq!(list[0]["member_titles"], serde_json::json!((1..=100).rev().map(|id| format!("Book {id}")).collect::<Vec<_>>()));
        assert_eq!(list[0]["attribute_tag"], "小説");
        assert_eq!(list[0]["attribute_editable"], true);

        groups.groups.insert(101, vec![100, 999, 1]);
        let list = futures::executor::block_on(load_listing(&repository, &groups, &[])).unwrap();
        assert_eq!(list[0]["member_titles"], serde_json::json!(["Book 100", "", "Book 1"]));
        assert_eq!(list[0]["attribute_tag"], "漫画");
        assert_eq!(list[0]["attribute_editable"], false);
    }
}
