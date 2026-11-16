#[cfg(feature = "native-runtime")]
use std::path::Path;

#[cfg(feature = "native-runtime")]
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

/// Native-only: rewrites the stored record and renames generated output files
/// on disk after a title-prefix setting change.
#[cfg(feature = "native-runtime")]
pub fn sync_title_projection(write: &crate::application::novel_access::NovelWriteGuard, id: i64) -> Result<()> {
    let novels = crate::native::novel_repository::NativeNovelRepository::new();
    let record = novels
        .get_current_sync(id.into())?
        .ok_or_else(|| NarouError::NotFound(format!("ID: {id}")))?;
    let previous_dir = crate::native::materialization::directory_for_origin(write)?;
    let keys = crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
    if write.prefix().as_ref() != keys.prefix().as_ref() { return Err(NarouError::Platform("title projection changed its operation origin".into())); }
    let raw_title = record.raw_title().to_string();
    let (_snapshot, settings) = crate::native::materialization::conversion_snapshot_with_origin(
        &previous_dir, write, Some(&[]), |path| {
            let settings = crate::converter::settings::NovelSettings::load_for_novel(id, &raw_title, &record.author, path)?;
            Ok((settings, crate::native::materialization::ConversionInputSelection { illustrations: false, cover: false }))
        },
    )?;
    if !settings.enable_strip_title_prefix && !record.has_raw_title() {
        return Ok(());
    }

    let display_title = project_title(&raw_title, settings.enable_strip_title_prefix);
    novels.update_existing_guarded_sync(write.fence(), id.into(), |current| {
        if current.raw_title() != record.raw_title() || current.title != record.title {
            return Err(NarouError::Platform("novel_busy: title changed during projection".into()));
        }
        current.title = display_title.clone();
        current.set_raw_title(raw_title);
        Ok(())
    })?;
    rename_projected_outputs(write, &previous_dir, &record.author, &record.title, &display_title)?;
    crate::native::materialization::block_on(async {
        let stores = crate::native::object_store::NativeStores::for_current_root()?;
        let mut lease = crate::application::novel_access::NovelReadLease::admit_for(&stores.objects, &keys, "title-projection").await?;
        let result = async {
            if lease.epoch() != write.epoch() { return Err(NarouError::Platform("title projection origin changed".into())); }
            if let Some(bytes) = lease.read_small(&keys.toc()).await? {
                let mut toc: crate::downloader::TocFile = serde_yaml::from_slice(&bytes)?;
                if toc.title != display_title {
                    toc.title = display_title;
                    write.write_small(&keys.toc(), crate::downloader::persistence::serialize_toc(&toc)?.into_bytes()).await?;
                }
            }
            Ok(())
        }.await;
        let released = lease.release().await;
        result.and(released)
    })?;
    Ok(())
}

/// Native-only: renames generated output files (txt/epub/mobi/kepub/zip)
/// when the projected title changes. Worker builds have no generated files.
#[cfg(feature = "native-runtime")]
pub fn rename_projected_outputs(
    write: &crate::application::novel_access::NovelWriteGuard,
    _novel_dir: &Path, author: &str, previous_title: &str, current_title: &str,
) -> Result<()> {
    if previous_title == current_title { return Ok(()); }
    let stores = crate::native::object_store::NativeStores::for_current_root()?;
    let previous_base = crate::converter::output::default_output_basename(author, previous_title);
    let current_base = crate::converter::output::default_output_basename(author, current_title);
    crate::native::materialization::block_on(async {
        let mut lease = crate::application::novel_access::NovelReadLease::admit(
            &stores.objects, write.prefix(), "title-output-rename",
        ).await?;
        let result = async {
            let prefix = crate::platform::ObjectKey::try_new(write.prefix().as_ref())?;
            for suffix in [".txt", ".epub", ".mobi", ".kepub.epub", ".zip"] {
                let old = prefix.join(format!("{previous_base}{suffix}"))?;
                let new = prefix.join(format!("{current_base}{suffix}"))?;
                if stores.objects.stat(&new).await?.is_some() { continue; }
                if let Some(stream) = lease.open_all(&old).await? {
                    write.write_stream(&new, stream).await?;
                    write.delete(&old).await?;
                }
            }
            Ok(())
        }.await;
        let released = lease.release().await;
        result.and(released)
    })
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
        std::fs::write(raw_dir.join("toc.yaml"), serde_yaml::to_string(
            &TocFile {
                title: "【書籍化】作品名".to_string(),
                author: "author".to_string(),
                toc_url: "https://example.com/1/".to_string(),
                story: None,
                subtitles: Vec::new(),
                novel_type: Some(1),
            },
        ).unwrap())
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

        let write = crate::native::materialization::capture_write_for_id(1).unwrap();
        sync_title_projection(&write, 1).unwrap();

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
        let toc: TocFile = serde_yaml::from_slice(&std::fs::read(raw_dir.join("toc.yaml")).unwrap()).unwrap();
        assert_eq!(toc.title, "作品名");

        std::fs::write(
            raw_dir.join("setting.ini"),
            "enable_strip_title_prefix=false\n",
        )
        .unwrap();
        let write = crate::native::materialization::capture_write_for_id(1).unwrap();
        sync_title_projection(&write, 1).unwrap();

        let restored = db::with_database(|db| Ok(db.get(1).cloned().unwrap())).unwrap();
        assert_eq!(restored.title, "【書籍化】作品名");
        assert_eq!(restored.raw_title(), "【書籍化】作品名");
        assert!(raw_dir.exists());
        assert!(!projected_dir.exists());
        assert!(raw_dir.join("[author] 【書籍化】作品名.epub").exists());
        let toc: TocFile = serde_yaml::from_slice(&std::fs::read(raw_dir.join("toc.yaml")).unwrap()).unwrap();
        assert_eq!(toc.title, "【書籍化】作品名");
    }
}
