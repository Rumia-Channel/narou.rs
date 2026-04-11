use narou_rs::downloader::Downloader;

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

        for id in &target_ids {
            match downloader.download_novel(&id.to_string()) {
                Ok(result) => {
                    println!(
                        "  Updated: {} (ID: {}, {}/{})",
                        result.title, result.id, result.updated_count, result.total_count
                    );
                    success += 1;
                }
                Err(e) => {
                    eprintln!("  Error updating ID {}: {}", id, e);
                    errors += 1;
                }
            }
        }

        println!();
        println!(
            "Update complete: {}/{} succeeded, {} failed ",
            success, total, errors
        );
    })
    .join();

    if let Err(e) = result {
        eprintln!("Thread panicked: {:?}", e);
    }
}
