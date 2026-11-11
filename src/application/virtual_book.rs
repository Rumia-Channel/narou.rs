//! Virtual-book listings, ordered inputs and collision-free member assets.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::application::convert::SectionStream;
use sha2::{Digest, Sha256};

use crate::db::NovelRecord;
use crate::downloader::persistence::{PersistenceService, deserialize_section, deserialize_toc};
use crate::downloader::TocFile;
use crate::error::{NarouError, Result};
use crate::illustration_store::{IllustrationIndex, is_remote_illustration_source};
use crate::platform::{NovelFilter, NovelId, NovelObjectKeys, NovelQuery, NovelRepository, NovelSort, ObjectKey};
use crate::virtual_novel::{AttributeComposition, VirtualNovelMembers};


pub struct VirtualBook {
    pub toc: TocFile,
    /// Pull-driven, hash-pinned member references; no member body is retained.
    pub sections: SectionStream,
    /// A book-local image name -> the original member's stored object.
    pub illustrations: BTreeMap<String, ObjectKey>,
}

struct SectionReference {
    key: ObjectKey,
    sha256: [u8; 32],
    index: String,
    href: String,
    chapter: String,
    aliases: Arc<BTreeMap<String, String>>,
}

struct TocReference {
    key: ObjectKey,
    sha256: [u8; 32],
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
    let mut references = Vec::new();
    let mut tocs = Vec::new();
    let mut sources: BTreeMap<ObjectKey, String> = BTreeMap::new();
    for id in members {
        let member = &records[id];
        let keys = NovelObjectKeys::new(&member.sitename, &member.file_title, member.use_subdirectory)?;
        let toc_key = keys.toc();
        let toc_bytes = persistence.objects().read_small(&toc_key).await?
            .ok_or_else(|| NarouError::NotFound(toc_key.to_string()))?;
        let toc = deserialize_toc(&toc_bytes)?;
        tocs.push(TocReference { key: toc_key, sha256: Sha256::digest(&toc_bytes).into() });
        drop(toc_bytes);
        let image_index = persistence.load_cache(&keys).await?;
        let mut aliases = BTreeMap::new();
        let member_start = references.len();
        for (position, mut subtitle) in toc.subtitles.into_iter().enumerate() {
            let key = keys.section(&subtitle.index, &subtitle.file_subtitle);
            let bytes = persistence.objects().read_small(&key).await?
                .ok_or_else(|| NarouError::NotFound(key.to_string()))?;
            let sha256 = Sha256::digest(&bytes).into();
            let section = deserialize_section(&bytes)?;
            if section.download_revision.max(1) != subtitle.download_revision.max(1)
                || subtitle.download_time.as_ref().is_some_and(|time| section.download_time.as_ref() != Some(time))
            {
                return Err(NarouError::Conversion(format!("変換中に入力が更新されました: {key}")));
            }
            drop(bytes);
            // This discovery pass retains only references. The output pass must
            // read exactly these revisions, not whatever is current by then.
            for text in [&section.element.introduction, &section.element.body, &section.element.postscript] {
                for reference in crate::illustration_refs::collect(text) {
                    let Some(key) = image_key(&keys, image_index.as_ref(), &reference) else { continue };
                    let ordinal = sources.len();
                    let alias = match sources.entry(key) {
                        std::collections::btree_map::Entry::Occupied(entry) => entry.get().clone(),
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            let Some((_, extension)) = entry.key().as_ref().rsplit_once('.') else { continue };
                            if !extension.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
                                continue;
                            }
                            let alias = format!("挿絵/v{id}-{ordinal}.{extension}");
                            entry.insert(alias.clone());
                            alias
                        }
                    };
                    aliases.insert(reference, alias);
                }
            }
            let sequence = (references.len() + 1).to_string();
            subtitle.index.clone_from(&sequence);
            subtitle.chapter = if position == 0 { member.title.clone() } else { String::new() };
            subtitle.subchapter.clear();
            references.push(SectionReference {
                key,
                sha256,
                index: sequence,
                href: subtitle.href.clone(),
                chapter: subtitle.chapter.clone(),
                aliases: Arc::default(),
            });
            subtitles.push(subtitle);
        }
        let aliases = Arc::new(aliases);
        for reference in &mut references[member_start..] {
            reference.aliases = aliases.clone();
        }
    }
    let objects = persistence.objects().clone();
    let sections = Box::pin(futures::stream::try_unfold(
        (references.into_iter(), tocs, objects),
        |(mut references, tocs, objects)| async move {
            let Some(reference) = references.next() else {
                // A changed table of contents invalidates the whole staged
                // conversion; no partially old/new virtual book is published.
                for toc in tocs {
                    let bytes = objects.read_small(&toc.key).await?
                        .ok_or_else(|| NarouError::NotFound(toc.key.to_string()))?;
                    verify_revision(&toc.key, &bytes, &toc.sha256)?;
                }
                return Ok(None);
            };
            let bytes = objects.read_small(&reference.key).await?
                .ok_or_else(|| NarouError::NotFound(reference.key.to_string()))?;
            verify_revision(&reference.key, &bytes, &reference.sha256)?;
            let mut section = deserialize_section(&bytes)?;
            section.index = reference.index;
            section.href = reference.href;
            section.chapter = reference.chapter;
            section.subchapter.clear();
            for text in [&mut section.element.introduction, &mut section.element.body, &mut section.element.postscript] {
                let (rewritten, _) = crate::illustration_refs::rewrite(std::mem::take(text), |value| {
                    reference.aliases.get(value).cloned()
                });
                *text = rewritten;
            }
            Ok(Some((section, (references, tocs, objects))))
        },
    ));
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

fn verify_revision(key: &ObjectKey, bytes: &[u8], expected: &[u8; 32]) -> Result<()> {
    let actual: [u8; 32] = Sha256::digest(bytes).into();
    if actual != *expected {
        return Err(NarouError::Conversion(format!("変換中に入力が更新されました: {key}")));
    }
    Ok(())
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

    async fn save_member(store: &Arc<crate::platform::mocks::MemoryObjectStore>, member: &NovelRecord, text: &str) -> (NovelObjectKeys, TocFile) {
        let keys = NovelObjectKeys::new(&member.sitename, &member.file_title, member.use_subdirectory).unwrap();
        let subtitle = crate::downloader::SubtitleInfo {
            index: "1".into(), href: format!("https://fixture.test/{}/1", member.id),
            chapter: "old chapter".into(), subchapter: "old subchapter".into(),
            subtitle: "episode".into(), file_subtitle: "episode".into(),
            subdate: "2026-01-01".into(), subupdate: None, download_time: None, download_revision: 1,
        };
        let mut toc = TocFile {
            title: member.title.clone(), author: member.author.clone(), toc_url: member.toc_url.clone(),
            story: None, subtitles: vec![subtitle], novel_type: Some(1),
        };
        let persistence = PersistenceService::new(store.clone(), store.clone());
        let element = crate::downloader::SectionElement {
            data_type: "text".into(),
            introduction: "member introduction".into(), body: text.into(), postscript: "member postscript".into(),
        };
        let time = persistence.save_section(&keys, &toc.subtitles[0], &element, 1).await.unwrap();
        toc.subtitles[0].download_time = Some(time);
        persistence.save_toc(&keys, &toc).await.unwrap();
        (keys, toc)
    }

    #[test]
    fn virtual_sections_replay_in_member_order_without_copying_member_bodies() {
        use futures::StreamExt;
        futures::executor::block_on(async {
            let store = Arc::new(crate::platform::mocks::MemoryObjectStore::new());
            let one = record(1, "小説");
            let two = record(2, "小説");
            save_member(&store, &one, "first body").await;
            save_member(&store, &two, "second body").await;
            let repository = MemoryNovelRepository::from_records(vec![one, two]);
            let persistence = PersistenceService::new(store.clone(), store);
            let mut book = load(&record(9, "小説"), &[2, 1], &repository, &persistence).await.unwrap();
            let first = book.sections.next().await.unwrap().unwrap();
            assert_eq!((first.index.as_str(), first.chapter.as_str(), first.element.body.as_str()), ("1", "Book 2", "second body"));
            assert_eq!(first.element.introduction, "member introduction");
            assert_eq!(first.element.postscript, "member postscript");
            assert!(first.subchapter.is_empty());
            let second = book.sections.next().await.unwrap().unwrap();
            assert_eq!((second.index.as_str(), second.chapter.as_str(), second.element.body.as_str()), ("2", "Book 1", "first body"));
            assert!(book.sections.next().await.is_none());
        });
    }

    #[test]
    fn virtual_member_revision_change_is_an_error_not_mixed_content() {
        use futures::StreamExt;
        use crate::platform::ObjectStore;
        futures::executor::block_on(async {
            let store = Arc::new(crate::platform::mocks::MemoryObjectStore::new());
            let member = record(1, "小説");
            let (keys, toc) = save_member(&store, &member, "original").await;
            let repository = MemoryNovelRepository::from_records(vec![member]);
            let persistence = PersistenceService::new(store.clone(), store.clone());
            let mut book = load(&record(9, "小説"), &[1], &repository, &persistence).await.unwrap();
            let key = keys.section("1", "episode");
            let bytes = store.read_small(&key).await.unwrap().unwrap();
            let mut section = deserialize_section(&bytes).unwrap();
            // Even a writer that forgot to bump download_revision cannot cause
            // a mixed response: the prepared reference pins actual content.
            section.element.body = "replacement".into();
            store.write_small(&key, crate::downloader::persistence::serialize_section(&section).unwrap().into_bytes()).await.unwrap();
            assert!(book.sections.next().await.unwrap().is_err());

            let mut book = load(&record(9, "小説"), &[1], &repository, &persistence).await.unwrap();
            assert!(book.sections.next().await.unwrap().is_ok());
            let mut changed_toc = toc;
            changed_toc.title = "new generation".into();
            persistence.save_toc(&keys, &changed_toc).await.unwrap();
            assert!(book.sections.next().await.unwrap().is_err());
        });
    }
}
