use narou_rs::converter::settings::NovelSettings;
use narou_rs::converter::user_converter::UserConverter;
use narou_rs::converter::NovelConverter;
use narou_rs::downloader::Downloader;
use narou_rs::progress::CliProgress;

pub fn cmd_download(targets: &[String], user_agent: Option<String>) {
    let targets = targets.to_vec();
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

        let multi = CliProgress::multi();
        let multi_clone = multi.clone();

        for target in targets {
            let progress = CliProgress::with_multi(
                &format!("DL {}", target),
                multi_clone.clone(),
            );
            downloader.set_progress(Box::new(progress));

            match downloader.download_novel(&target) {
                Ok(dl) => {
                    let msg = if dl.new_novel {
                        format!(
                            "  DL: {} (ID: {}, {} sections)",
                            dl.title, dl.id, dl.total_count
                        )
                    } else {
                        format!(
                            "  DL: {} (ID: {}, {}/{})",
                            dl.title, dl.id, dl.updated_count, dl.total_count
                        )
                    };
                    let _ = multi_clone.println(&msg);

                    if dl.updated_count > 0 || dl.new_novel {
                        if let Err(e) = auto_convert(&multi_clone, &dl) {
                            let _ = multi_clone.println(&format!("  Convert error: {}", e));
                        }
                    }
                }
                Err(e) => {
                    let _ = multi_clone.println(&format!("  Error: {}", e));
                }
            }
        }

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
