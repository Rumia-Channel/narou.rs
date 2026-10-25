//! `narou export` / `narou import` — native CLI front end for the portable
//! transfer archive.
//!
//! The archive format lives in `narou_rs::transfer` (shared with the Worker and
//! the browser WASM client) and the filesystem/DB work lives in
//! `narou_rs::native::transfer` (shared with the native Web API). This module
//! only resolves CLI targets, reads/writes the ZIP file and prints a summary.

use std::path::{Path, PathBuf};

use chrono::Local;
use narou_rs::error::{NarouError, Result};
use narou_rs::native::transfer;
use narou_rs::transfer::ConflictPolicy;

use super::resolve_target_to_id;

/// Default output name when `-o/--output` is omitted (`narou-export-*.zip`).
pub fn default_export_path() -> PathBuf {
    PathBuf::from(format!(
        "narou-export-{}.zip",
        Local::now().format("%Y%m%d%H%M%S")
    ))
}

/// `--overwrite` selects replacement; the default is conservative skipping.
pub fn conflict_policy(overwrite: bool) -> ConflictPolicy {
    if overwrite {
        ConflictPolicy::Overwrite
    } else {
        ConflictPolicy::Skip
    }
}

pub fn cmd_export(targets: &[String], output: Option<&str>) -> Result<()> {
    if targets.is_empty() {
        return Err(NarouError::InvalidTarget(
            "書き出す小説を指定してください (ID / URL / 作品名 / Nコード / 別名)".to_string(),
        ));
    }

    narou_rs::db::init_database()?;

    let mut ids = Vec::with_capacity(targets.len());
    let mut missing = Vec::new();
    for target in targets {
        match resolve_target_to_id(target) {
            Some(id) => ids.push(id),
            None => missing.push(target.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(NarouError::NotFound(format!(
            "{} は存在しません",
            missing.join(", ")
        )));
    }

    println!("書き出し対象を収集しています...");
    let exported = transfer::export_archive_bytes(&ids)?;

    let path = output
        .map(PathBuf::from)
        .unwrap_or_else(default_export_path);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &exported.bytes)?;

    for novel in &exported.novels {
        println!(
            "ID:{}　{} ({} ファイル)",
            novel.id, novel.title, novel.file_count
        );
    }
    println!(
        "{} 件の小説を {} に書き出しました",
        exported.novels.len(),
        path.display()
    );
    Ok(())
}

pub fn cmd_import(archive: &str, overwrite: bool) -> Result<()> {
    narou_rs::db::init_database()?;

    let path = Path::new(archive);
    let bytes = std::fs::read(path).map_err(|error| {
        NarouError::Io(std::io::Error::new(
            error.kind(),
            format!("{} を読み込めません: {error}", path.display()),
        ))
    })?;

    println!("{} を読み込んでいます...", path.display());
    let outcome = transfer::import_archive_bytes(&bytes, conflict_policy(overwrite))?;

    println!(
        "新規: {} 件 / 上書き: {} 件 / スキップ: {} 件",
        outcome.created, outcome.overwritten, outcome.skipped
    );
    for error in &outcome.errors {
        println!("取り込みエラー: {error}");
    }
    if outcome.success() {
        Ok(())
    } else {
        Err(NarouError::Platform(format!(
            "{} 件の小説を取り込めませんでした",
            outcome.errors.len()
        )))
    }
}

#[cfg(test)]
mod transfer_archive_tests {
    use super::*;
    use narou_rs::db::NovelRecord;
    use narou_rs::native::transfer::{
        archive_novel_from_record, collect_novel_files, is_archivable_relative_path,
    };
    use narou_rs::transfer::{
        ArchiveFile, ArchiveNovel, ImportAction, ParsedArchive, ParsedNovel, plan_import,
    };

    fn record() -> NovelRecord {
        use chrono::{TimeZone, Utc};
        NovelRecord {
            id: 1,
            author: "作者".to_string(),
            title: "作品名".to_string(),
            file_title: "n1234ab 作品名".to_string(),
            toc_url: "https://example.test/n1234ab/".to_string(),
            sitename: "ExampleSite".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.with_ymd_and_hms(2026, 4, 20, 0, 0, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: vec!["タグ".to_string()],
            ncode: Some("n1234ab".to_string()),
            domain: Some("example.test".to_string()),
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
    fn transfer_archive_collection_excludes_backup_and_runtime_files() {
        let temp = tempfile::tempdir().unwrap();
        let novel_dir = temp.path().join("novel");
        std::fs::create_dir_all(novel_dir.join("本文")).unwrap();
        std::fs::create_dir_all(novel_dir.join("backup")).unwrap();
        std::fs::write(novel_dir.join("toc.yaml"), b"toc").unwrap();
        std::fs::write(novel_dir.join("本文/1 第一話.yaml"), b"body").unwrap();
        std::fs::write(novel_dir.join("setting.ini.lock"), b"lock").unwrap();
        std::fs::write(novel_dir.join("backup/old.zip"), b"zip").unwrap();

        let files = collect_novel_files(&novel_dir).unwrap();
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, vec!["toc.yaml", "本文/1 第一話.yaml"]);
        assert!(!is_archivable_relative_path("../escape"));
    }

    #[test]
    fn transfer_archive_manifest_metadata_keeps_record_fields() {
        let metadata = archive_novel_from_record(
            &record(),
            vec![ArchiveFile::new("toc.yaml", b"toc".to_vec())],
            None,
        );
        let manuscript = metadata.clone();
        assert_eq!(manuscript.title, "作品名");
        assert_eq!(manuscript.author, "作者");
        assert_eq!(manuscript.domain.as_deref(), Some("example.test"));
        assert_eq!(manuscript.tags, vec!["タグ".to_string()]);
        assert_eq!(manuscript.files[0].path, "toc.yaml");
    }

    fn parsed(toc_urls: &[(&str, &str)]) -> ParsedArchive {
        let novels: Vec<ArchiveNovel> = toc_urls
            .iter()
            .map(|(toc_url, title)| ArchiveNovel {
                toc_url: (*toc_url).to_string(),
                title: (*title).to_string(),
                ..ArchiveNovel::default()
            })
            .collect();
        ParsedArchive {
            novels: novels
                .iter()
                .enumerate()
                .map(|(index, novel)| ParsedNovel {
                    index,
                    toc_url: novel.toc_url.clone(),
                    title: novel.title.clone(),
                    files: Vec::new(),
                })
                .collect(),
            manifest: narou_rs::transfer::ArchiveManifest {
                novels,
                ..narou_rs::transfer::ArchiveManifest::default()
            },
        }
    }

    #[test]
    fn transfer_archive_import_plan_skips_by_default_and_overwrites_on_flag() {
        let archive = parsed(&[
            ("https://example.test/1/", "One"),
            ("https://example.test/2/", "Two"),
        ]);
        let existing = [narou_rs::transfer::ExistingNovel {
            toc_url: "https://example.test/1/".to_string(),
            title: "One".to_string(),
        }];

        let skip = plan_import(&archive, &existing, conflict_policy(false));
        assert_eq!(skip.entries[0].action, ImportAction::Skip);
        assert_eq!(skip.entries[1].action, ImportAction::Create);
        assert_eq!(skip.count(ImportAction::Skip), 1);

        let overwrite = plan_import(&archive, &existing, conflict_policy(true));
        assert_eq!(overwrite.entries[0].action, ImportAction::Overwrite);
        assert_eq!(overwrite.entries[1].action, ImportAction::Create);
        assert_eq!(overwrite.count(ImportAction::Overwrite), 1);
    }

    #[test]
    fn transfer_archive_default_output_name_is_a_zip_in_the_current_directory() {
        let path = default_export_path();
        assert_eq!(path.components().count(), 1, "{}", path.display());
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("narou-export-"), "{name}");
        assert!(name.ends_with(".zip"), "{name}");
    }
}
