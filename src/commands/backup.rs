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
