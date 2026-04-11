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
                Ok(result) => {
                    let msg = if result.new_novel {
                        format!(
                            "  New novel: {} (ID: {}, {} sections)",
                            result.title, result.id, result.total_count
                        )
                    } else {
                        format!(
                            "  Updated: {} (ID: {}, {}/{})",
                            result.title, result.id, result.updated_count, result.total_count
                        )
                    };
                    let _ = multi_clone.println(&msg);
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
