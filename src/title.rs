use std::path::Path;

use crate::error::{NarouError, Result};

pub fn strip_title_prefix(title: &str) -> &str {
    let mut rest = title.trim_start();

    loop {
        let Some(open) = rest.chars().next() else {
            return title;
        };
        let close = match open {
            '【' => '】',
            '《' => '》',
            '〈' => '〉',
            '［' => '］',
            '[' => ']',
            _ => break,
        };
        let Some(close_offset) = rest.find(close) else {
            break;
        };
        rest = rest[close_offset + close.len_utf8()..].trim_start();
    }

    if rest.is_empty() { title } else { rest }
}

pub fn project_title(raw_title: &str, strip_prefix: bool) -> String {
    if strip_prefix {
        strip_title_prefix(raw_title).to_string()
    } else {
        raw_title.to_string()
    }
}

pub fn sync_title_projection(id: i64) -> Result<()> {
    let (record, archive_root) = crate::db::with_database(|db| {
        let record = db
            .get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {id}")))?;
        Ok((record, db.archive_root().to_path_buf()))
    })?;
    let previous_dir = crate::db::existing_novel_dir_for_record(&archive_root, &record);
    let raw_title = record.raw_title().to_string();
    let settings = crate::converter::settings::NovelSettings::load_for_novel(
        id,
        &raw_title,
        &record.author,
        &previous_dir,
    );
    if !settings.enable_strip_title_prefix && !record.has_raw_title() {
        return Ok(());
    }

    let display_title = project_title(&raw_title, settings.enable_strip_title_prefix);
    let mut projected = record.clone();
    projected.title = display_title.clone();
    projected.set_raw_title(raw_title);

    crate::db::with_database_mut(|db| {
        db.insert(projected);
        db.save()
    })?;
    rename_projected_outputs(&previous_dir, &record.author, &record.title, &display_title)?;

    if let Some(mut toc) = crate::downloader::persistence::load_toc_file(&previous_dir) {
        if toc.title != display_title {
            toc.title = display_title;
            crate::downloader::persistence::save_toc_file(&previous_dir, &toc)?;
        }
    }
    Ok(())
}

pub fn rename_projected_outputs(
    novel_dir: &Path,
    author: &str,
    previous_title: &str,
    current_title: &str,
) -> Result<()> {
    if previous_title == current_title || !novel_dir.exists() {
        return Ok(());
    }
    let previous_base = crate::converter::output::default_output_basename(author, previous_title);
    let current_base = crate::converter::output::default_output_basename(author, current_title);
    for suffix in [".txt", ".epub", ".mobi", ".kepub.epub", ".zip"] {
        let previous = novel_dir.join(format!("{previous_base}{suffix}"));
        if !previous.exists() {
            continue;
        }
        let current = novel_dir.join(format!("{current_base}{suffix}"));
        if !current.exists() {
            std::fs::rename(previous, current)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::db::{self, Database, NovelRecord};
    use crate::downloader::TocFile;

    use super::{strip_title_prefix, sync_title_projection};

    struct DatabaseGuard(Option<Database>);

    impl Drop for DatabaseGuard {
        fn drop(&mut self) {
            *db::DATABASE.lock() = self.0.take();
        }
    }

    #[test]
    fn strips_consecutive_bracketed_title_prefixes() {
        assert_eq!(
            strip_title_prefix("【3/17第1巻発売】《コミカライズ企画進行中》 悪役令息が破滅フラグ"),
            "悪役令息が破滅フラグ"
        );
        assert_eq!(strip_title_prefix("［書籍化］ 作品名"), "作品名");
    }

    #[test]
    fn keeps_unclosed_or_entirely_bracketed_titles() {
        assert_eq!(strip_title_prefix("【本当のタイトル】"), "【本当のタイトル】");
        assert_eq!(strip_title_prefix("【未閉じの作品名"), "【未閉じの作品名");
    }

    #[test]
    fn sync_projection_updates_display_data_and_preserves_raw_title() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let raw_dir = temp
            .path()
            .join("小説データ")
            .join("Example")
            .join("【書籍化】作品名");
        std::fs::create_dir_all(&raw_dir).unwrap();
        std::fs::write(
            raw_dir.join("setting.ini"),
            "enable_strip_title_prefix=true\n",
        )
        .unwrap();
        std::fs::write(
            raw_dir.join("[author] 【書籍化】作品名.epub"),
            "epub",
        )
        .unwrap();
        crate::downloader::persistence::save_toc_file(
            &raw_dir,
            &TocFile {
                title: "【書籍化】作品名".to_string(),
                author: "author".to_string(),
                toc_url: "https://example.com/1/".to_string(),
                story: None,
                subtitles: Vec::new(),
                novel_type: Some(1),
            },
        )
        .unwrap();

        let record: NovelRecord = serde_yaml::from_str(
            r#"id: 1
author: author
title: 【書籍化】作品名
file_title: 【書籍化】作品名
toc_url: https://example.com/1/
sitename: Example
last_update: 2026-04-20 00:00:00.000000000 +09:00
"#,
        )
        .unwrap();
        let mut database = Database::new().unwrap();
        database.insert(record);
        database.save().unwrap();
        let mut slot = db::DATABASE.lock();
        let previous = slot.take();
        *slot = Some(database);
        drop(slot);
        let _db_guard = DatabaseGuard(previous);

        sync_title_projection(1).unwrap();

        let projected = db::with_database(|db| Ok(db.get(1).cloned().unwrap())).unwrap();
        assert_eq!(projected.title, "作品名");
        assert_eq!(projected.raw_title(), "【書籍化】作品名");
        assert_eq!(projected.file_title, "【書籍化】作品名");
        let projected_dir = temp
            .path()
            .join("小説データ")
            .join("Example")
            .join("作品名");
        assert!(!projected_dir.exists());
        assert!(raw_dir.exists());
        assert!(raw_dir.join("setting.ini").exists());
        assert!(raw_dir.join("[author] 作品名.epub").exists());
        assert!(
            !raw_dir
                .join("[author] 【書籍化】作品名.epub")
                .exists()
        );
        let toc = crate::downloader::persistence::load_toc_file(&raw_dir).unwrap();
        assert_eq!(toc.title, "作品名");

        std::fs::write(
            raw_dir.join("setting.ini"),
            "enable_strip_title_prefix=false\n",
        )
        .unwrap();
        sync_title_projection(1).unwrap();

        let restored = db::with_database(|db| Ok(db.get(1).cloned().unwrap())).unwrap();
        assert_eq!(restored.title, "【書籍化】作品名");
        assert_eq!(restored.raw_title(), "【書籍化】作品名");
        assert!(raw_dir.exists());
        assert!(!projected_dir.exists());
        assert!(raw_dir.join("[author] 【書籍化】作品名.epub").exists());
        let toc = crate::downloader::persistence::load_toc_file(&raw_dir).unwrap();
        assert_eq!(toc.title, "【書籍化】作品名");
    }
}
