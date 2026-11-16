use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use narou_rs::db;
use narou_rs::application::novel_access::NovelWriteGuard;
use narou_rs::downloader::persistence::section_filename;
use narou_rs::downloader::{RAW_DATA_DIR, SECTION_SAVE_DIR};

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
        if let Some(write) = resolve_novel_origin(&target)? {
            clean_novel(write, remove)?;
        } else {
            // Ruby は直前に変換した小説が解決できない場合も error を出す。
            log::report_error(&format!("{} は存在しません", target));
        }
        return Ok(());
    }

    let expanded = download::tagname_to_ids(targets);
    for target in expanded {
        let Some(write) = resolve_novel_origin(&target)? else {
            log::report_error(&format!("{} は存在しません", target));
            continue;
        };
        clean_novel(write, remove)?;
    }

    Ok(())
}

fn clean_all(remove: bool) -> Result<(), String> {
    let frozen_ids = narou_rs::compat::load_frozen_ids().map_err(|e| e.to_string())?;
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let ids = novels
        .scan_ids_sync(&narou_rs::platform::NovelFilter::all(), None, usize::MAX)
        .map_err(|e| e.to_string())?;
    let mut origins = Vec::new();
    for record in novels.get_many_current_sync(&ids).map_err(|error| error.to_string())?.into_iter().flatten() {
        if narou_rs::compat::record_is_frozen(&record, &frozen_ids) { continue; }
        origins.push(narou_rs::native::materialization::capture_write_for_record(&record).map_err(|error| error.to_string())?);
    }
    for write in origins { clean_novel(write, remove)?; }
    Ok(())
}

fn resolve_novel_origin(target: &str) -> Result<Option<NovelWriteGuard>, String> {
    let Some(id) = super::resolve_target_to_id(target) else { return Ok(None); };
    narou_rs::native::materialization::capture_write_for_id(id).map(Some).map_err(|error| error.to_string())
}

fn clean_novel(write: NovelWriteGuard, remove: bool) -> Result<(), String> {
    let novel_dir = narou_rs::native::materialization::directory_for_origin(&write).map_err(|error| error.to_string())?;
    let snapshot = narou_rs::native::materialization::NativeMutationSnapshot::begin_with_origin(&novel_dir, write)
        .map_err(|error| error.to_string())?;
    let bytes = match fs::read(snapshot.path().join("toc.yaml")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let toc: narou_rs::downloader::TocFile = serde_yaml::from_slice(&bytes).map_err(|error| error.to_string())?;
    let expected = expected_section_stems(&toc);
    for orphan in find_orphan_files_with_expected(snapshot.path(), &expected)? {
        let relative = orphan.strip_prefix(snapshot.path()).map_err(|error| error.to_string())?;
        println!("{}", novel_dir.join(relative).display());
        if remove { fs::remove_file(&orphan).map_err(|error| error.to_string())?; }
    }
    if remove { snapshot.publish_changes().map_err(|error| error.to_string())?; }
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

    #[test]
    fn clean_snapshot_orphans_do_not_touch_nested_history_or_settings() {
        let temp = tempfile::tempdir().unwrap();
        for relative in ["raw/1 keep.html", "raw/9 orphan.html", "本文/1 keep.yaml", "本文/9 orphan.yaml", "本文/cache/old/1 keep.yaml", "setting.ini"] {
            let path = temp.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, relative.as_bytes()).unwrap();
        }
        let expected = ["1 keep".to_string()].into_iter().collect();
        let orphans = find_orphan_files_with_expected(temp.path(), &expected).unwrap();
        assert_eq!(orphans.len(), 2);
        for path in orphans { std::fs::remove_file(path).unwrap(); }
        assert_eq!(std::fs::read(temp.path().join("raw/1 keep.html")).unwrap(), b"raw/1 keep.html");
        assert!(temp.path().join("本文/cache/old/1 keep.yaml").is_file());
        assert!(temp.path().join("setting.ini").is_file());
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
                download_revision: 0,
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
