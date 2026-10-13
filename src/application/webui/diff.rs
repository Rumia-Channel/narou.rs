//! 差分 (`/api/diff_list` / `/api/diff_clean`) の共有規則。
//!
//! キャッシュの分類・列挙・デコード・削除計画は ObjectStore 上で共有する。
//! native は旧 YAML レイアウトの空ディレクトリと列挙失敗だけを補足する。

use std::collections::BTreeMap;
use std::num::NonZeroUsize;

use serde_json::Value;

use crate::downloader::types::{SECTION_SAVE_DIR, SectionFile};
use crate::error::{NarouError, Result as StorageResult};
use crate::platform::{ObjectKey, ObjectListRequest, ObjectPrefix, ObjectStore};

use super::html_escape;

/// `diff.txt` が無いときの表示。
pub const DIFF_NO_DIFF: &str = "No diff";
/// 差分本文を UTF-8 として読めない / 読み取り自体が失敗したときの表示。
pub const DIFF_READ_ERROR: &str = "読み取りエラー";
/// レコードが存在しない id の表示。
pub const DIFF_NOVEL_NOT_FOUND: &str = "Novel not found";
/// `/api/diff_clean` の失敗文言。
pub const DIFF_CLEAN_FAILED: &str = "差分の削除に失敗しました";

/// Native/Ruby cache layouts accepted by both runtimes.
pub const DIFF_CACHE_DIR_NAMES: [&str; 2] = ["cache", ".cache"];
const CACHE_LIST_PAGE_SIZE: usize = 1000;

/// Read-only native directory evidence. An absent mirror needs no hint; a
/// known enumeration failure must never become a successful empty version.
#[derive(Debug, Clone)]
pub struct CacheVersionHint {
    pub version: String,
    pub enumeration_succeeded: bool,
}

/// Only direct YAML sections and the non-YAML version record establish a
/// version. Nested files and unrelated files cannot become diff history.
pub(crate) fn cached_object_parts<'a>(
    novel_prefix: &str,
    key: &'a str,
) -> Option<(&'a str, &'a str)> {
    let rest = key
        .strip_prefix(novel_prefix)?
        .strip_prefix('/')?
        .strip_prefix(SECTION_SAVE_DIR)?
        .strip_prefix('/')?;
    let (cache_dir, rest) = rest.split_once('/')?;
    if !DIFF_CACHE_DIR_NAMES.contains(&cache_dir) {
        return None;
    }
    let (version, file_name) = rest.split_once('/')?;
    if version.is_empty()
        || file_name.contains('/')
        || (file_name != ".version" && !file_name.ends_with(".yaml"))
    {
        return None;
    }
    Some((version, file_name))
}

/// Native cache-move cleanup uses the same logical layout classification as
/// the catalog; other asset namespaces and version markers are not sections.
#[cfg(feature = "native-runtime")]
pub(crate) fn is_diff_cache_section(key: &ObjectKey) -> bool {
    if !key.as_ref().starts_with("novels/") {
        return false;
    }
    let Some((prefix, _)) = key.as_ref().rsplit_once("/本文/") else {
        return false;
    };
    cached_object_parts(prefix, key.as_ref()).is_some_and(|(_, filename)| filename != ".version")
}

async fn list_cache_objects(
    objects: &dyn ObjectStore,
    novel_prefix: &ObjectKey,
) -> StorageResult<Vec<ObjectKey>> {
    let sections = novel_prefix.join(SECTION_SAVE_DIR)?;
    let mut result = Vec::new();
    for name in DIFF_CACHE_DIR_NAMES {
        let prefix = ObjectPrefix::from(sections.join(name)?);
        let mut request = ObjectListRequest::new(
            prefix,
            NonZeroUsize::new(CACHE_LIST_PAGE_SIZE).expect("cache page size is non-zero"),
        );
        loop {
            let page = objects.list_page(&request).await?;
            for object in page.objects {
                if cached_object_parts(novel_prefix.as_ref(), object.key.as_ref()).is_some() {
                    result.push(object.key);
                }
            }
            let Some(cursor) = page.next_cursor else {
                break;
            };
            if request.cursor.as_ref().is_some_and(|previous| cursor <= *previous) {
                return Err(NarouError::Platform("cache listing cursor did not advance".to_string()));
            }
            request = request.after(cursor);
        }
    }
    Ok(result)
}

fn section_file_index(filename: &str) -> usize {
    let stem = filename.strip_suffix(".yaml").unwrap_or(filename);
    let index = stem.split_once(' ').map_or(stem, |(index, _)| index);
    index.parse().unwrap_or(0)
}

/// Read the same persisted cache versions on native (including no-mirror
/// SQLite) and Worker. A listed section counts even when reading/decoding it
/// fails; only its unreadable title is skipped.
pub async fn read_diff_cache_catalog(
    objects: &dyn ObjectStore,
    novel_prefix: &ObjectKey,
    native_hints: &[CacheVersionHint],
) -> StorageResult<Vec<DiffListVersion>> {
    #[derive(Default)]
    struct Version<'a> {
        enumeration_failed: bool,
        sections: Vec<(usize, &'a str, &'a ObjectKey)>,
    }

    let cache_objects = list_cache_objects(objects, novel_prefix).await?;
    let mut versions: BTreeMap<&str, Version<'_>> = BTreeMap::new();
    for hint in native_hints {
        versions.entry(&hint.version).or_default().enumeration_failed |=
            !hint.enumeration_succeeded;
    }
    for key in &cache_objects {
        let Some((version, filename)) = cached_object_parts(novel_prefix.as_ref(), key.as_ref())
        else {
            continue;
        };
        let group = versions.entry(version).or_default();
        if filename != ".version" {
            group.sections.push((section_file_index(filename), filename, key));
        }
    }
    let mut result = Vec::with_capacity(versions.len());
    for (version, mut group) in versions.into_iter().rev() {
        group.sections.sort_unstable_by(|a, b| a.cmp(b));
        let only_latest_update = group.sections.is_empty() && !group.enumeration_failed;
        let mut entries = Vec::with_capacity(group.sections.len());
        for (_, _, key) in group.sections {
            let Ok(Some(bytes)) = objects.read_small(key).await else {
                continue;
            };
            let Ok(section) = serde_yaml::from_slice::<SectionFile>(&bytes) else {
                continue;
            };
            entries.push((section.index, section.subtitle.trim_end().to_string()));
        }
        result.push(DiffListVersion {
            version: version.to_string(),
            only_latest_update,
            entries,
        });
    }
    Ok(result)
}

/// Snapshot the complete deletion set before mutating the store. Version
/// records and sections in either cache layout are removed together, without
/// reading section bodies or touching unrelated novel objects.
pub async fn plan_diff_cache_clean(
    objects: &dyn ObjectStore,
    novel_prefix: &ObjectKey,
) -> StorageResult<Vec<ObjectKey>> {
    list_cache_objects(objects, novel_prefix).await
}

/// 差分本体の表示文字列。`Ok(Some(bytes))` は本文 (UTF-8 として読めなければ
/// [`DIFF_READ_ERROR`])、`Ok(None)` は [`DIFF_NO_DIFF`]、`Err(_)` は
/// [`DIFF_READ_ERROR`]。
pub fn diff_content_label<E>(diff: Result<Option<Vec<u8>>, E>) -> String {
    match diff {
        Ok(Some(bytes)) => {
            String::from_utf8(bytes).unwrap_or_else(|_| DIFF_READ_ERROR.to_string())
        }
        Ok(None) => DIFF_NO_DIFF.to_string(),
        Err(_) => DIFF_READ_ERROR.to_string(),
    }
}

/// `/api/diff_list` の 1 件 (`{"id", "title", "content"}`)。
pub fn diff_list_entry(id: i64, title: &str, content: &str) -> Value {
    serde_json::json!({
        "id": id,
        "title": title,
        "content": content,
    })
}

/// レコードが無い id の 1 件 (タイトルは `ID: n`、本文は
/// [`DIFF_NOVEL_NOT_FOUND`])。
pub fn diff_missing_entry(id: i64) -> Value {
    diff_list_entry(id, &format!("ID: {}", id), DIFF_NOVEL_NOT_FOUND)
}

/// `/api/diff_clean` の成功文言。
pub fn diff_cleaned_message(target: &str) -> String {
    format!("Diff cleaned for {}", target)
}

/// 差分一覧の 1 バージョン分 (`GET /api/diff_list`)。
#[derive(Debug, Clone)]
pub struct DiffListVersion {
    /// バージョン名 (降順に並べた順で渡す)。
    pub version: String,
    /// そのバージョンに差分ファイルが 1 つも無かったか
    /// (`(最新話のみのアップデート)` を出す条件)。列挙自体に失敗した
    /// バージョンは `false` (native はその場合もマーカーを出さない)。
    pub only_latest_update: bool,
    /// 読み取れたファイルの `(index, subtitle)`。`index` は本文の
    /// `SectionFile.index`、`subtitle` は末尾空白を落としたもの。読めなかった
    /// ファイルは含めない。
    pub entries: Vec<(String, String)>,
}

/// 差分一覧の HTML 断片 (native `render_diff_list_html_for_target` /
/// Worker `render_diff_list_html` と同一)。`versions` は表示順 (バージョン
/// 名の降順) に並べて渡し、番号はその順で 1 から振る。
pub fn render_diff_list_html(versions: &[DiffListVersion]) -> String {
    let mut html = String::new();
    for (number, version) in versions.iter().enumerate() {
        html.push_str("<div class=\"diff-list-group\">");
        html.push_str(&format!(
            "<div class=\"diff-list-version\">{}&nbsp;&nbsp;-{}</div>",
            html_escape(&version.version),
            number + 1
        ));
        if version.only_latest_update {
            html.push_str("<div class=\"diff-list-entry\">(最新話のみのアップデート)</div></div>");
            continue;
        }
        for (index, subtitle) in &version.entries {
            html.push_str(&format!(
                "<div class=\"diff-list-entry\">第{}部分　{}</div>",
                html_escape(index),
                html_escape(subtitle)
            ));
        }
        html.push_str("</div>");
    }
    html
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::NovelObjectKeys;
    use crate::platform::mocks::MemoryObjectStore;

    fn section_bytes(index: usize) -> Vec<u8> {
        serde_yaml::to_string(&SectionFile {
            index: index.to_string(),
            href: format!("https://example.test/{index}"),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: format!("第{index}話 \n"),
            file_subtitle: format!("第{index}話"),
            subdate: String::new(),
            subupdate: None,
            download_time: None,
            element: crate::downloader::types::SectionElement {
                data_type: "text".to_string(),
                introduction: String::new(),
                postscript: String::new(),
                body: "本文".to_string(),
            },
        })
        .unwrap()
        .into_bytes()
    }

    #[test]
    fn stored_catalog_pages_orders_and_cleans_sections_and_markers() {
        futures::executor::block_on(async {
            let store = MemoryObjectStore::new();
            let keys = NovelObjectKeys::new("site", "title", true).unwrap();
            // More than one real page; key order differs from section order.
            for index in (1..=1002).rev() {
                store.write_small(
                    &keys.cached_section("v2", &index.to_string(), &format!("第{index}話")),
                    section_bytes(index),
                ).await.unwrap();
            }
            let legacy = ObjectKey::try_new(format!("{}/本文/.cache/v3/.version", keys.prefix())).unwrap();
            store.write_small(&legacy, Vec::new()).await.unwrap();
            store.write_small(&keys.cached_version_marker("v1"), Vec::new()).await.unwrap();
            store.write_small(&keys.cached_section("v4", "1", "破損"), b"not: [yaml".to_vec()).await.unwrap();
            let nested = ObjectKey::try_new(format!("{}/本文/cache/ignored/nested/1 x.yaml", keys.prefix())).unwrap();
            store.write_small(&nested, section_bytes(1)).await.unwrap();
            store.write_small(&keys.section("1", "現在"), section_bytes(1)).await.unwrap();

            let versions = read_diff_cache_catalog(&store, keys.prefix(), &[]).await.unwrap();
            assert_eq!(versions.iter().map(|version| version.version.as_str()).collect::<Vec<_>>(),
                ["v4", "v3", "v2", "v1"]);
            assert!(!versions[0].only_latest_update);
            assert!(versions[0].entries.is_empty(), "corruption skips a title, not the section's existence");
            assert!(versions[1].only_latest_update);
            assert!(versions[3].only_latest_update);
            let entries = &versions[2].entries;
            assert_eq!(entries.len(), 1002, "every page must be consumed");
            for (index, (stored_index, title)) in entries.iter().enumerate() {
                assert_eq!(stored_index, &(index + 1).to_string());
                assert_eq!(title, &format!("第{}話", index + 1), "subtitle whitespace is trimmed");
            }
            let hints = [
                CacheVersionHint { version: "v0".to_string(), enumeration_succeeded: true },
                CacheVersionHint { version: "v3".to_string(), enumeration_succeeded: false },
            ];
            let hinted = read_diff_cache_catalog(&store, keys.prefix(), &hints).await.unwrap();
            assert!(!hinted.iter().find(|version| version.version == "v3").unwrap().only_latest_update);
            assert!(hinted.iter().find(|version| version.version == "v0").unwrap().only_latest_update);

            let plan = plan_diff_cache_clean(&store, keys.prefix()).await.unwrap();
            assert_eq!(plan.len(), 1005);
            for key in plan {
                store.delete(&key).await.unwrap();
            }
            assert!(read_diff_cache_catalog(&store, keys.prefix(), &[]).await.unwrap().is_empty());
            assert!(store.read_small(&nested).await.unwrap().is_some());
            assert!(store.read_small(&keys.section("1", "現在")).await.unwrap().is_some());
        });
    }

    #[test]
    fn diff_content_labels_match_native_wording() {
        let present: Result<Option<Vec<u8>>, std::io::Error> =
            Ok(Some(b"*** 1,3 ****".to_vec()));
        assert_eq!(diff_content_label(present), "*** 1,3 ****");
        let missing: Result<Option<Vec<u8>>, std::io::Error> = Ok(None);
        assert_eq!(diff_content_label(missing), DIFF_NO_DIFF);
        let invalid: Result<Option<Vec<u8>>, std::io::Error> = Ok(Some(vec![0xff, 0xfe]));
        assert_eq!(diff_content_label(invalid), DIFF_READ_ERROR);
        assert_eq!(diff_content_label(Err("boom")), DIFF_READ_ERROR);
    }

    #[test]
    fn diff_entries_use_id_title_fallback() {
        assert_eq!(
            diff_missing_entry(7),
            serde_json::json!({ "id": 7, "title": "ID: 7", "content": "Novel not found" })
        );
        assert_eq!(
            diff_list_entry(7, "title", "No diff"),
            serde_json::json!({ "id": 7, "title": "title", "content": "No diff" })
        );
    }

    #[test]
    fn render_diff_list_matches_native_html() {
        let versions = vec![
            DiffListVersion {
                version: "2024-01-02".to_string(),
                only_latest_update: false,
                entries: vec![
                    ("3".to_string(), "第三章　<&>".to_string()),
                    ("4".to_string(), "第四章".to_string()),
                ],
            },
            DiffListVersion {
                version: "2024-01-01".to_string(),
                only_latest_update: true,
                entries: Vec::new(),
            },
        ];
        assert_eq!(
            render_diff_list_html(&versions),
            "<div class=\"diff-list-group\">\
<div class=\"diff-list-version\">2024-01-02&nbsp;&nbsp;-1</div>\
<div class=\"diff-list-entry\">第3部分　第三章　&lt;&amp;&gt;</div>\
<div class=\"diff-list-entry\">第4部分　第四章</div>\
</div>\
<div class=\"diff-list-group\">\
<div class=\"diff-list-version\">2024-01-01&nbsp;&nbsp;-2</div>\
<div class=\"diff-list-entry\">(最新話のみのアップデート)</div></div>"
        );
    }

    #[test]
    fn render_diff_list_omits_marker_when_entries_are_unreadable() {
        // ファイルは存在したが 1 つも読めなかった場合、native はマーカーを
        // 出さない (グループだけが残る)。
        let versions = vec![DiffListVersion {
            version: "v1".to_string(),
            only_latest_update: false,
            entries: Vec::new(),
        }];
        assert_eq!(
            render_diff_list_html(&versions),
            "<div class=\"diff-list-group\">\
<div class=\"diff-list-version\">v1&nbsp;&nbsp;-1</div>\
</div>"
        );
    }

    #[test]
    fn clean_message_uses_target() {
        assert_eq!(diff_cleaned_message("42"), "Diff cleaned for 42");
        assert_eq!(DIFF_CLEAN_FAILED, "差分の削除に失敗しました");
    }
}
