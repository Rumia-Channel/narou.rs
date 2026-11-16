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
            .get_current_sync(data.id.into())
            .map_err(|e| NarouError::Database(e.to_string()))?
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", data.id)))?;
        let write = narou_rs::native::materialization::capture_write_for_record(&record)?;
        let archive_root = narou_rs::db::with_database(|db| Ok(db.archive_root().to_path_buf()))
            .map_err(|e| NarouError::Database(e.to_string()))?;
        let novel_dir = narou_rs::db::existing_novel_dir_for_record(&archive_root, &record);
        if !novel_dir.try_exists()? {
            // Absence of a mirror is not absence of a novel. Prove canonical
            // toc absence under the original admission, releasing our reader
            // before retiring that same writer into a durable removal owner.
            let stores = narou_rs::native::object_store::NativeStores::for_current_root()?;
            let stored = narou_rs::native::materialization::block_on(async {
                let mut lease = narou_rs::application::novel_access::NovelReadLease::admit(&stores.objects, write.prefix(), "backup-content-check").await?;
                let result = async {
                    if lease.epoch() != write.epoch() {
                        return Err(NarouError::Platform("novel_busy: backup origin changed".into()));
                    }
                    let key = narou_rs::platform::ObjectKey::try_new(write.prefix().as_ref())?.join("toc.yaml")?;
                    Ok(lease.read_small(&key).await?.is_some())
                }.await;
                let released = lease.release().await;
                match result { Ok(value) => { released?; Ok(value) }, Err(error) => Err(error) }
            })?;
            if !stored {
                use narou_rs::platform::NovelAccessFence;
                let owner = narou_rs::platform::access_fence::new_removal_owner(data.id.into(), false)?;
                narou_rs::native::materialization::block_on(stores.primary.retire_writer(write.fence(), &owner))?;
                narou_rs::native::application::recover_removals()?;
                if novels.get_current_sync(data.id.into())?.is_some() {
                    return Err(NarouError::Platform("novel_busy: missing novel index removal is still recovering".into()));
                }
                return Err(NarouError::NotFound(format!(
                    "{} が見つかりません。\n保存フォルダが消去されていたため、データベースのインデックスを削除しました。",
                    novel_dir.display()
                )));
            }
        }

        // Read all stored files in a private directory, never in a converter's
        // live output directory. A failed snapshot does not change the record.

        println!("ID:{}　{}", data.id, data.title);
        print!("バックアップを作成しています");
        let _ = io::stdout().flush();
        let spinner = Spinner::start();
        let backup_name = compat::create_backup(&write, &record.title)?;
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
        use narou_rs::platform::{NovelObjectKeys, NovelRepository};

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
        let stores = narou_rs::native::object_store::NativeStores::for_narou_root(root.path()).unwrap();
        let keys = NovelObjectKeys::new("example.test", "backup-test", false).unwrap();
        let (record, write) = narou_rs::native::materialization::block_on(async {
            let reservation = novels.allocate_id().await?;
            let mut record = sample_record();
            record.id = reservation.id().0;
            let mut write = narou_rs::application::novel_access::NovelWriteGuard::capture_for(&stores.objects, Some(&stores.assets), &keys).await?;
            write.retain_reservation(&reservation)?;
            novels.reserve_create(write.fence(), &reservation, &record.toc_url).await?;
            novels.apply_create_guarded(write.fence(), &reservation, record.clone()).await?;
            novels.release_id(reservation).await?;
            Ok((record, write))
        }).unwrap();
        let archive_root = root.path().join("小説データ");
        futures::executor::block_on(async {
            write
                .write_small(
                    &keys.toc(),
                    b"title: BackupTest\nauthor: author\ntoc_url: https://example.test/1\nsubtitles: []\n"
                        .to_vec(),
                )
                .await
                .unwrap();
            write
                .write_small(&keys.setting(), b"enable_yokogaki = true\n".to_vec())
                .await
                .unwrap();
            write
                .write_small(
                    &keys.section("1", "第一話"),
                    "index: '1'\nhref: '1'\nchapter: ''\nsubchapter: ''\nsubtitle: 第一話\nfile_subtitle: 第一話\nsubdate: '2026-01-01'\n"
                        .as_bytes()
                        .to_vec(),
                )
                .await
                .unwrap();
            write.write_small(&keys.prefix().join("user-notes.txt").unwrap(), b"keep user notes".to_vec()).await.unwrap();
            write.write_small(&keys.prefix().join("previous.epub").unwrap(), b"keep generated output".to_vec()).await.unwrap();
        });

        cmd_backup(&[record.id.to_string()]).unwrap();

        // 誤ってインデックスを消していないこと。
        let record = novels
            .get_current_sync(record.id.into())
            .unwrap()
            .expect("record must survive a DB-only backup");
        assert_eq!(record.file_title, "backup-test");

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
        assert!(names.iter().any(|name| name == "user-notes.txt"));
        assert!(names.iter().any(|name| name == "previous.epub"));
        assert!(!names.iter().any(|name| name.ends_with(zip_path.file_name().unwrap().to_str().unwrap())),
            "a backup must never contain the zip currently being written");

        // 取り出したファイルはガードが片付ける。
        assert!(!novel_dir.join("toc.yaml").exists());
        assert!(!novel_dir.join("setting.ini").exists());

        *narou_rs::db::DATABASE.lock() = None;
    }
}
