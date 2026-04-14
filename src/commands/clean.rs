use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use narou_rs::db;
use narou_rs::db::paths::novel_dir_for_record;
use narou_rs::downloader::persistence::load_toc_file;
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
        if let Some(dir) = resolve_novel_dir(&target) {
            clean_novel_dir(&dir, remove)?;
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
    let dirs = db::with_database(|db| {
        let archive_root = db.archive_root().to_path_buf();
        let mut dirs = Vec::new();
        for record in db.all_records().values() {
            if narou_rs::compat::record_is_frozen(record, &frozen_ids) {
                continue;
            }
            dirs.push(novel_dir_for_record(&archive_root, record));
        }
        Ok::<Vec<PathBuf>, narou_rs::error::NarouError>(dirs)
    })
    .map_err(|e| e.to_string())?;

    for dir in dirs {
        clean_novel_dir(&dir, remove)?;
    }
    Ok(())
}

fn resolve_novel_dir(target: &str) -> Option<PathBuf> {
    let id = super::resolve_target_to_id(target)?;
    db::with_database(|db| {
        let archive_root = db.archive_root().to_path_buf();
        Ok(db
            .get(id)
            .map(|record| novel_dir_for_record(&archive_root, record)))
    })
    .ok()
    .flatten()
}

fn clean_novel_dir(novel_dir: &PathBuf, remove: bool) -> Result<(), String> {
    if !novel_dir.is_dir() || !novel_dir.join("toc.yaml").exists() {
        return Ok(());
    }

    for orphan in find_orphan_files(novel_dir)? {
        println!("{}", orphan.display());
        if remove {
            fs::remove_file(&orphan).map_err(|e| e.to_string())?;
        }
    }

    Ok(())
}

fn find_orphan_files(novel_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let Some(toc) = load_toc_file(&novel_dir.to_path_buf()) else {
        return Ok(Vec::new());
    };

    let expected = toc
        .subtitles
        .iter()
        .map(|subtitle| format!("{} {}", subtitle.index, subtitle.file_subtitle))
        .collect::<HashSet<_>>();

    let mut orphans = collect_orphans(&novel_dir.join(RAW_DATA_DIR), &expected, &["html", "txt"])?;
    orphans.extend(collect_orphans(
        &novel_dir.join(SECTION_SAVE_DIR),
        &expected,
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use narou_rs::downloader::persistence::save_toc_file;
    use narou_rs::downloader::{SubtitleInfo, TocFile};

    use super::find_orphan_files;

    #[test]
    fn clean_detects_orphan_raw_and_section_files() {
        let base = std::env::temp_dir().join(format!(
            "narou-rs-clean-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
        save_toc_file(&base, &toc).unwrap();
        std::fs::write(base.join("raw").join("1 keep.html"), "").unwrap();
        std::fs::write(base.join("raw").join("orphan.html"), "").unwrap();
        std::fs::write(base.join("本文").join("1 keep.yaml"), "").unwrap();
        std::fs::write(base.join("本文").join("orphan.yaml"), "").unwrap();

        let orphans = find_orphan_files(&base).unwrap();
        let orphan_names = orphans
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(orphan_names, vec!["orphan.html", "orphan.yaml"]);

        std::fs::remove_dir_all(base).unwrap();
    }
}
