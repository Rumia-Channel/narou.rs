use std::io::{self, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use crate::commands::{download, help};
use narou_rs::compat;
use narou_rs::error::{NarouError, Result};

struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Spinner {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                print!(".");
                let _ = io::stdout().flush();
                thread::sleep(Duration::from_millis(500));
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn cmd_backup(targets: &[String]) -> Result<()> {
    if targets.is_empty() {
        help::display_command_help("backup");
        return Ok(());
    }

    narou_rs::db::init_database()?;

    let targets = download::tagname_to_ids(targets);

    for (index, target) in targets.iter().enumerate() {
        if index > 0 {
            println!("{}", "―".repeat(35));
        }

        let Some(data) = download::get_data_by_target(target) else {
            println!("{} は存在しません", target);
            continue;
        };

        let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
        let record = novels
            .get_sync(data.id.into())
            .map_err(|e| NarouError::Database(e.to_string()))?
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", data.id)))?;
        let archive_root = narou_rs::db::with_database(|db| Ok(db.archive_root().to_path_buf()))
            .map_err(|e| NarouError::Database(e.to_string()))?;
        let novel_dir = narou_rs::db::existing_novel_dir_for_record(&archive_root, &record);
        if !novel_dir.exists() {
            // mirror-files=false の構成では有効な作品でもフォルダが無い。
            // ストアに本文が残っているなら記録を消さず、下で取り出して
            // バックアップする。ストアを確認できないときも消さない。
            match narou_rs::native::object_store::novel_content_stored(&archive_root, &novel_dir) {
                Ok(false) => {
                    novels
                        .apply_batch_sync(vec![narou_rs::platform::NovelMutation::Remove(
                            data.id.into(),
                        )])
                        .map_err(|e| NarouError::Database(e.to_string()))?;
                    return Err(NarouError::NotFound(format!(
                        "{} が見つかりません。\n保存フォルダが消去されていたため、データベースのインデックスを削除しました。",
                        novel_dir.display()
                    )));
                }
                Ok(true) => {}
                Err(error) => return Err(error),
            }
        }

        // DB/S3 にだけ実体がある構成では、zip を組む間だけ本文・設定・挿絵を
        // 取り出す。この時点から失敗してもライブラリの記録は変更しない
        // (ガードが取り出した分を片付けてからエラーを返す)。
        let store = narou_rs::native::object_store::NativeStore::for_current_root()?;
        let _materialized_files = store.materialize_novel_files(&novel_dir)?;
        let _materialized_illustrations = narou_rs::native::illustrations::materialize_blocking(&novel_dir)?;

        println!("ID:{}　{}", data.id, data.title);
        print!("バックアップを作成しています");
        let _ = io::stdout().flush();
        let spinner = Spinner::start();
        let backup_name = compat::create_backup(&novel_dir, &record.title)?;
        drop(spinner);
        println!();
        println!("{} を作成しました", backup_name);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> narou_rs::db::NovelRecord {
        use chrono::{TimeZone, Utc};
        narou_rs::db::NovelRecord {
            id: 1,
            author: "author".to_string(),
            title: "BackupTest".to_string(),
            file_title: "backup-test".to_string(),
            toc_url: "https://example.test/1".to_string(),
            sitename: "example.test".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.with_ymd_and_hms(2026, 4, 20, 0, 0, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
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

    /// mirror-files=false の作品: レコードを消さず、DB 上の本文・設定を
    /// 含む zip を作り、取り出したファイルは片付ける。
    #[test]
    fn backup_materializes_db_only_novel_and_keeps_record() {
        use narou_rs::native::sqlite::state;
        use narou_rs::platform::{NovelMutation, NovelObjectKeys, ObjectStore};

        let root = tempfile::tempdir().unwrap();
        let narou_dir = root.path().join(".narou");
        std::fs::create_dir_all(&narou_dir).unwrap();
        state::write_mode(&narou_dir, state::StorageMode::Sqlite).unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(root.path());
        narou_rs::db::init_database().unwrap();
        narou_rs::db::settings::update(narou_rs::setting_core::SettingScope::Local, |settings| {
            settings.insert("sqlite.mirror-files".to_string(), serde_yaml::Value::Bool(false));
            Ok(())
        }).unwrap();

        let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
        novels
            .apply_batch_sync(vec![NovelMutation::Upsert(sample_record())])
            .unwrap();

        let archive_root = root.path().join("小説データ");
        let keys = NovelObjectKeys::new("example.test", "backup-test", false).unwrap();
        let handle = state::active_for(&narou_dir).expect("sqlite state");
        let mirror = narou_rs::native::object_store::NativeObjectStore::from_root(
            archive_root.clone(),
        )
        .unwrap();
        let store = narou_rs::native::sqlite::object_store::SqliteObjectStore::with_mirror_files(
            handle.conn_ref().clone(),
            mirror,
            false,
        );
        futures::executor::block_on(async {
            store
                .write_small(
                    &keys.toc(),
                    b"title: BackupTest\nauthor: author\ntoc_url: https://example.test/1\nsubtitles: []\n"
                        .to_vec(),
                )
                .await
                .unwrap();
            store
                .write_small(&keys.setting(), b"enable_yokogaki = true\n".to_vec())
                .await
                .unwrap();
            store
                .write_small(
                    &keys.section("1", "第一話"),
                    "index: '1'\nhref: '1'\nchapter: ''\nsubchapter: ''\nsubtitle: 第一話\nfile_subtitle: 第一話\nsubdate: '2026-01-01'\n"
                        .as_bytes()
                        .to_vec(),
                )
                .await
                .unwrap();
        });

        cmd_backup(&["1".to_string()]).unwrap();

        // 誤ってインデックスを消していないこと。
        let record = novels
            .get_sync(1i64.into())
            .unwrap()
            .expect("record must survive a DB-only backup");
        assert_eq!(record.id, 1);

        let novel_dir = narou_rs::db::existing_novel_dir_for_record(&archive_root, &record);
        let zip_path = std::fs::read_dir(novel_dir.join("backup"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .next()
            .expect("backup zip exists");
        let mut archive = zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap()).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().unwrap().to_string())
            .collect();
        assert!(names.iter().any(|name| name == "toc.yaml"), "{names:?}");
        assert!(names.iter().any(|name| name == "setting.ini"), "{names:?}");
        assert!(
            names.iter().any(|name| name.contains("第一話.yaml")),
            "{names:?}"
        );

        // 取り出したファイルはガードが片付ける。
        assert!(!novel_dir.join("toc.yaml").exists());
        assert!(!novel_dir.join("setting.ini").exists());

        *narou_rs::db::DATABASE.lock() = None;
    }
}
