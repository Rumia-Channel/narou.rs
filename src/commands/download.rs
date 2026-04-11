use narou_rs::downloader::Downloader;

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

        for target in targets {
            println!("Downloading: {}", target);
            match downloader.download_novel(&target) {
                Ok(result) => {
                    if result.new_novel {
                        println!(
                            "  New novel: {} (ID: {}, {} sections)",
                            result.title, result.id, result.total_count
                        );
                    } else {
                        println!(
                            "  Updated: {} (ID: {}, {}/{})",
                            result.title, result.id, result.updated_count, result.total_count
                        );
                    }
                }
                Err(e) => {
                    eprintln!("  Error: {}", e);
                }
            }
        }
    })
    .join();

    if let Err(e) = result {
        eprintln!("Thread panicked: {:?}", e);
    }
}
