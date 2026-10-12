use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use narou_rs::db;
use narou_rs::db::paths::novel_dir_for_record;
use narou_rs::downloader::persistence::section_filename;
use narou_rs::downloader::{RAW_DATA_DIR, SECTION_SAVE_DIR};
use narou_rs::native::legacy_persistence::load_toc_file;

use super::download;
use super::log;

pub fn cmd_clean(targets: &[String], force: bool, dry_run: bool, all: bool) -> i32 {
    match cmd_clean_inner(targets, force, dry_run, all) {
        Ok(()) => 0,
        Err(err) => {
            log::report_error(&err);
            1
        }
    }
}

fn cmd_clean_inner(
    targets: &[String],
    force: bool,
    dry_run: bool,
    all: bool,
) -> Result<(), String> {
    db::init_database().map_err(|e| e.to_string())?;

    let remove = force && !dry_run;
    if all {
        clean_all(remove)?;
        return Ok(());
    }

    if targets.is_empty() {
        let Some(target) = super::latest_convert_target() else {
            return Ok(());
        };
        if let Some(dir) = resolve_novel_dir(&target) {
            clean_novel_dir(&dir, remove)?;
        } else {
            // Ruby は直前に変換した小説が解決できない場合も error を出す。
            log::report_error(&format!("{} は存在しません", target));
        }
        return Ok(());
    }

    let expanded = download::tagname_to_ids(targets);
    for target in expanded {
        let Some(dir) = resolve_novel_dir(&target) else {
            log::report_error(&format!("{} は存在しません", target));
            continue;
        };
        clean_novel_dir(&dir, remove)?;
    }

    Ok(())
}

fn clean_all(remove: bool) -> Result<(), String> {
    let frozen_ids = narou_rs::compat::load_frozen_ids().map_err(|e| e.to_string())?;
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let ids = novels
        .scan_ids_sync(&narou_rs::platform::NovelFilter::all(), None, usize::MAX)
        .map_err(|e| e.to_string())?;
    let archive_root = narou_rs::db::with_database(|db| Ok(db.archive_root().to_path_buf()))
        .map_err(|e| e.to_string())?;
    let mut dirs = Vec::new();
    for id in ids {
        let Ok(Some(record)) = novels.get_sync(id) else {
            continue;
        };
        if narou_rs::compat::record_is_frozen(&record, &frozen_ids) {
            continue;
        }
        dirs.push(novel_dir_for_record(&archive_root, &record));
    }

    for dir in dirs {
        clean_novel_dir(&dir, remove)?;
    }
    Ok(())
}

fn resolve_novel_dir(target: &str) -> Option<PathBuf> {
    let id = super::resolve_target_to_id(target)?;
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let record = novels.get_sync(id.into()).ok().flatten()?;
    let archive_root = db::with_database(|db| Ok(db.archive_root().to_path_buf()))
        .unwrap_or_else(|_| PathBuf::from(narou_rs::downloader::ARCHIVE_ROOT_DIR));
    Some(novel_dir_for_record(&archive_root, &record))
}

fn clean_novel_dir(novel_dir: &Path, remove: bool) -> Result<(), String> {
    let store = narou_rs::native::object_store::NativeStore::for_current_root()
        .map_err(|e| e.to_string())?;
    let archive_root = db::with_database(|db| Ok(db.archive_root().to_path_buf()))
        .map_err(|e| e.to_string())?;
    let prefix = narou_rs::native::object_store::novel_object_prefix(&archive_root, novel_dir);

    // mirror-files=false の構成では実ファイルが無いので、ストア上の
    // オブジェクトだけでも対象を見つけられるようにする。
    let local_available = novel_dir.is_dir() && novel_dir.join("toc.yaml").exists();
    let stored_toc = match &prefix {
        Some(prefix) => {
            let key = prefix.join("toc.yaml").map_err(|e| e.to_string())?;
            futures::executor::block_on(narou_rs::platform::ObjectStore::stat(&store, &key))
                .map_err(|e| e.to_string())?
                .is_some()
        }
        None => false,
    };
    if !local_available && !stored_toc {
        return Ok(());
    }

    let toc = match &prefix {
        Some(prefix) => load_toc_from_store(&store, prefix)?,
        None => load_toc_file(novel_dir),
    };
    let Some(toc) = toc else {
        return Err(format!("目次を読み取れません: {}", novel_dir.display()));
    };

    let expected = expected_section_stems(&toc);

    let mut listed = HashSet::new();
    for orphan in find_orphan_files_with_expected(novel_dir, &expected)? {
        println!("{}", orphan.display());
        if remove {
            remove_orphan(&store, &archive_root, &orphan)?;
        }
        listed.insert(orphan);
    }

    if let Some(prefix) = &prefix {
        for (path, key) in collect_store_orphans(&store, prefix, novel_dir, &expected)? {
            if !listed.insert(path.clone()) {
                continue;
            }
            println!("{}", path.display());
            if remove {
                futures::executor::block_on(narou_rs::platform::ObjectStore::delete(&store, &key))
                    .map_err(|e| e.to_string())?;
                if path.is_file() {
                    fs::remove_file(&path).map_err(|e| e.to_string())?;
                }
            }
        }
    }

    Ok(())
}

/// トークンに現れる話数 (本文/raw) の期待ファイル名 stem。
fn expected_section_stems(toc: &narou_rs::downloader::TocFile) -> HashSet<String> {
    toc.subtitles
        .iter()
        .map(|subtitle| {
            let filename = section_filename(subtitle);
            filename
                .strip_suffix(".yaml")
                .unwrap_or(&filename)
                .to_string()
        })
        .collect()
}

/// ストア上の不要オブジェクト (raw/ と本文/ の直下) を (表示パス, キー) で返す。
fn collect_store_orphans(
    store: &dyn narou_rs::platform::ObjectStore,
    prefix: &narou_rs::platform::ObjectKey,
    novel_dir: &Path,
    expected: &HashSet<String>,
) -> Result<Vec<(PathBuf, narou_rs::platform::ObjectKey)>, String> {
    let prefix_text = prefix.as_ref();
    let child_prefix = format!("{prefix_text}/");
    let mut cursor: Option<String> = None;
    let mut orphans = Vec::new();
    loop {
        let request = narou_rs::platform::ObjectListRequest::new(
            narou_rs::platform::ObjectPrefix::new(prefix_text).map_err(|e| e.to_string())?,
            std::num::NonZeroUsize::new(512).expect("page size is non-zero"),
        );
        let request = match &cursor {
            Some(next) => request.after(next.clone()),
            None => request,
        };
        let page =
            futures::executor::block_on(narou_rs::platform::ObjectStore::list_page(store, &request))
                .map_err(|e| e.to_string())?;
        for metadata in &page.objects {
            let key_text = metadata.key.as_ref();
            let Some(relative) = key_text.strip_prefix(&child_prefix) else {
                continue;
            };
            let Some((dir, name)) = relative.split_once('/') else {
                continue;
            };
            if name.contains('/') {
                continue;
            }
            let wanted = match dir {
                "raw" => matches!(lower_extension(name).as_str(), "html" | "txt"),
                "本文" => lower_extension(name) == "yaml",
                _ => false,
            };
            if !wanted {
                continue;
            }
            let Some((stem, _)) = name.rsplit_once('.') else {
                continue;
            };
            if expected.contains(stem) {
                continue;
            }
            let path = novel_dir.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
            orphans.push((path, metadata.key.clone()));
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(orphans)
}

fn lower_extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default()
}

/// 孤児をストアとミラーの両方から消す。
///
/// mirror-files=false ではストアの delete が実ファイルに触らないため、
/// 残っていればファイルも消す (どちらか一方だけが残ると不整合になる)。
fn remove_orphan(
    store: &narou_rs::native::object_store::NativeStore,
    archive_root: &Path,
    path: &Path,
) -> Result<(), String> {
    if let Some(key) = narou_rs::native::object_store::object_key_for_file(archive_root, path) {
        futures::executor::block_on(narou_rs::platform::ObjectStore::delete(store, &key))
            .map_err(|e| e.to_string())?;
    }
    if path.is_file() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn load_toc_from_store(
    store: &narou_rs::native::object_store::NativeStore,
    prefix: &narou_rs::platform::ObjectKey,
) -> Result<Option<narou_rs::downloader::TocFile>, String> {
    let key = prefix.join("toc.yaml").map_err(|error| error.to_string())?;
    let Some(bytes) = futures::executor::block_on(narou_rs::platform::ObjectStore::read_small(
        store, &key,
    )).map_err(|error| error.to_string())? else {
        return Ok(None);
    };
    serde_yaml::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("目次を解析できません ({key}): {error}"))
}

fn find_orphan_files_with_expected(
    novel_dir: &Path,
    expected: &HashSet<String>,
) -> Result<Vec<PathBuf>, String> {
    let mut orphans = collect_orphans(&novel_dir.join(RAW_DATA_DIR), expected, &["html", "txt"])?;
    orphans.extend(collect_orphans(
        &novel_dir.join(SECTION_SAVE_DIR),
        expected,
        &["yaml"],
    )?);
    Ok(orphans)
}

fn collect_orphans(
    dir: &Path,
    expected: &HashSet<String>,
    exts: &[&str],
) -> Result<Vec<PathBuf>, String> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if !exts
            .iter()
            .any(|candidate| ext.eq_ignore_ascii_case(candidate))
        {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !expected.contains(stem) {
            files.push(path);
        }
    }

    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use narou_rs::downloader::{SubtitleInfo, TocFile};

    use super::{expected_section_stems, find_orphan_files_with_expected};
    use std::collections::HashSet;
    use std::path::PathBuf;

    /// ストア上だけにある本文/raw の孤児を検出できること
    /// (mirror-files=false でも clean が空振りしない)。
    #[test]
    fn clean_store_orphans_detect_unreferenced_objects() {
        use narou_rs::platform::mocks::MemoryObjectStore;
        use narou_rs::platform::{ObjectKey, ObjectStore};

        let store = MemoryObjectStore::new();
        let prefix = ObjectKey::try_new("novels/site/title").unwrap();
        futures::executor::block_on(async {
            for (key, body) in [
                ("novels/site/title/toc.yaml", "toc"),
                ("novels/site/title/raw/1 keep.html", "keep"),
                ("novels/site/title/raw/9 orphan.html", "orphan"),
                ("novels/site/title/本文/1 keep.yaml", "keep"),
                ("novels/site/title/本文/9 orphan.yaml", "orphan"),
                ("novels/site/title/本文/cache/20260101000000/1 keep.yaml", "cache"),
                ("novels/site/title/setting.ini", "ini"),
            ] {
                store
                    .write_small(
                        &ObjectKey::try_new(key).unwrap(),
                        body.as_bytes().to_vec(),
                    )
                    .await
                    .unwrap();
            }
        });

        let novel_dir = PathBuf::from("site").join("title");
        let expected: HashSet<String> = ["1 keep".to_string()].into_iter().collect();
        let orphans =
            super::collect_store_orphans(&store, &prefix, &novel_dir, &expected).unwrap();
        let names: Vec<String> = orphans
            .iter()
            .map(|(path, _)| path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["9 orphan.html", "9 orphan.yaml"]);
    }

    #[test]
    fn clean_detects_orphan_raw_and_section_files() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path();
        std::fs::create_dir_all(base.join("raw")).unwrap();
        std::fs::create_dir_all(base.join("本文")).unwrap();

        let toc = TocFile {
            title: "title".into(),
            author: "author".into(),
            toc_url: "https://example.com".into(),
            story: None,
            subtitles: vec![SubtitleInfo {
                index: "1".into(),
                href: "1".into(),
                chapter: String::new(),
                subchapter: String::new(),
                subtitle: "sub".into(),
                file_subtitle: "keep".into(),
                subdate: "2024-01-01".into(),
                subupdate: None,
                download_time: None,
            }],
            novel_type: None,
        };
        std::fs::write(base.join("raw").join("1 keep.html"), "").unwrap();
        std::fs::write(base.join("raw").join("orphan.html"), "").unwrap();
        std::fs::write(base.join("本文").join("1 keep.yaml"), "").unwrap();
        std::fs::write(base.join("本文").join("orphan.yaml"), "").unwrap();

        let orphans = find_orphan_files_with_expected(base, &expected_section_stems(&toc)).unwrap();
        let orphan_names = orphans
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(orphan_names, vec!["orphan.html", "orphan.yaml"]);

    }
}
