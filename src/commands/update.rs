use narou_rs::converter::settings::NovelSettings;
use narou_rs::converter::user_converter::UserConverter;
use narou_rs::converter::NovelConverter;
use narou_rs::downloader::Downloader;
use narou_rs::progress::{CliProgress, ProgressReporter};

pub fn cmd_update(ids: Option<Vec<i64>>, all: bool, user_agent: Option<String>) {
    let result = std::thread::spawn(move || {
        if let Err(e) = narou_rs::db::init_database() {
            eprintln!("Error initializing database: {}", e);
            std::process::exit(1);
        }

        let mut downloader = match Downloader::with_user_agent(user_agent.as_deref()) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Error creating downloader: {}", e);
                std::process::exit(1);
            }
        };

        let target_ids: Vec<i64> = if all {
            narou_rs::db::with_database(|db| Ok(db.ids())).unwrap_or_default()
        } else if let Some(id_list) = ids {
            id_list
        } else {
            eprintln!("Usage: narou update --all | narou update <id>...");
            std::process::exit(1);
        };

        let total = target_ids.len();
        let mut success = 0usize;
        let mut errors = 0usize;

        let multi = CliProgress::multi();
        let multi_clone = multi.clone();

        let overall = CliProgress::with_multi_spinner(
            &format!("Updating {} novels", total),
            multi_clone.clone(),
        );
        overall.set_length(total as u64);

        for id in &target_ids {
            let progress = CliProgress::with_multi(
                &format!("DL {}", id),
                multi_clone.clone(),
            );
            downloader.set_progress(Box::new(progress));

            match downloader.download_novel(&id.to_string()) {
                Ok(dl) => {
                    let _ = multi_clone.println(&format!(
                        "  DL: {} (ID: {}, {}/{})",
                        dl.title, dl.id, dl.updated_count, dl.total_count
                    ));

                    if dl.updated_count > 0 || dl.new_novel {
                        if let Err(e) = auto_convert(&multi_clone, &dl) {
                            let _ = multi_clone.println(&format!("  Convert error: {}", e));
                        }
                    }
                    success += 1;
                }
                Err(e) => {
                    let _ = multi_clone.println(&format!(
                        "  Error updating ID {}: {}", id, e
                    ));
                    errors += 1;
                }
            }
            overall.inc(1);
        }

        overall.finish_with_message(&format!(
            "Update complete: {}/{} succeeded, {} failed",
            success, total, errors
        ));
        drop(multi);
    })
    .join();

    if let Err(e) = result {
        eprintln!("Thread panicked: {:?}", e);
    }
}

fn auto_convert(
    multi: &std::sync::Arc<indicatif::MultiProgress>,
    dl: &narou_rs::downloader::DownloadResult,
) -> Result<(), String> {
    let settings = NovelSettings::load_for_novel(
        dl.id,
        &dl.title,
        &dl.author,
        &dl.novel_dir,
    );
    let mut converter =
        if let Some(uc) = UserConverter::load_with_title(&dl.novel_dir, &dl.title) {
            NovelConverter::with_user_converter(settings, uc)
        } else {
            NovelConverter::new(settings)
        };

    let progress = CliProgress::with_multi(
        &format!("Convert {}", dl.title),
        multi.clone(),
    );
    converter.set_progress(Box::new(progress));

    match converter.convert_novel_by_id(dl.id, &dl.novel_dir) {
        Ok(path) => {
            let _ = multi.println(&format!("  Converted: {}", path));
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}
