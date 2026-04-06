use std::net::SocketAddr;

use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "narou", about = "narou.rs - A Rust port of narou.rb")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    Web {
        #[arg(short, long, default_value_t = 3000)]
        port: u16,
        #[arg(short, long, default_value_t = false)]
        no_browser: bool,
    },
    Download {
        targets: Vec<String>,
    },
    Update {
        ids: Option<Vec<i64>>,
        #[arg(long)]
        all: bool,
    },
    Convert {
        targets: Vec<String>,
    },
    List {
        #[arg(short, long)]
        tag: Option<String>,
        #[arg(long)]
        frozen: bool,
    },
    Tag {
        #[arg(short, long)]
        add: Option<String>,
        #[arg(short, long)]
        remove: Option<String>,
        targets: Vec<String>,
    },
    Freeze {
        targets: Vec<String>,
        #[arg(long)]
        off: bool,
    },
    Remove {
        targets: Vec<String>,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Web { port, no_browser } => {
            run_web_server(port, no_browser).await;
        }
        Commands::Download { targets } => {
            if targets.is_empty() {
                eprintln!("Usage: narou download <url|ncode|id>...");
                std::process::exit(1);
            }
            cmd_download(&targets);
        }
        Commands::Update { ids, all } => {
            cmd_update(ids, all);
        }
        Commands::Convert { targets } => {
            if targets.is_empty() {
                eprintln!("Usage: narou convert <url|ncode|id>...");
                std::process::exit(1);
            }
            cmd_convert(&targets);
        }
        Commands::List { tag, frozen } => {
            cmd_list(tag.as_deref(), frozen);
        }
        Commands::Tag {
            add,
            remove,
            targets,
        } => {
            if targets.is_empty() {
                eprintln!("Usage: narou tag --add <tag> <targets>...");
                std::process::exit(1);
            }
            cmd_tag(add.as_deref(), remove.as_deref(), &targets);
        }
        Commands::Freeze { targets, off } => {
            if targets.is_empty() {
                eprintln!("Usage: narou freeze <targets>...");
                std::process::exit(1);
            }
            cmd_freeze(&targets, off);
        }
        Commands::Remove { targets } => {
            if targets.is_empty() {
                eprintln!("Usage: narou remove <targets>...");
                std::process::exit(1);
            }
            cmd_remove(&targets);
        }
    }
}

async fn run_web_server(port: u16, no_browser: bool) {
    use narou_rs::web;

    info!("Starting narou.rs web server on port {}", port);

    let app = web::create_router();
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("Listening on http://localhost:{}", port);

    if !no_browser {
        let url = format!("http://localhost:{}", port);
        let _ = open::that(&url);
    }

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn cmd_download(targets: &[String]) {
    use narou_rs::downloader::Downloader;

    let targets = targets.to_vec();
    let result = std::thread::spawn(move || {
        if let Err(e) = narou_rs::db::init_database() {
            eprintln!("Error initializing database: {}", e);
            std::process::exit(1);
        }

        let mut downloader = match Downloader::new() {
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

fn cmd_update(ids: Option<Vec<i64>>, all: bool) {
    let result = std::thread::spawn(move || {
        use narou_rs::downloader::Downloader;

        if let Err(e) = narou_rs::db::init_database() {
            eprintln!("Error initializing database: {}", e);
            std::process::exit(1);
        }

        let mut downloader = match Downloader::new() {
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

fn cmd_convert(targets: &[String]) {
    use narou_rs::converter::NovelConverter;
    use narou_rs::converter::settings::NovelSettings;

    if let Err(e) = narou_rs::db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

    let settings = NovelSettings::default();
    let mut converter = NovelConverter::new(settings);

    for target in targets {
        println!("Converting: {}", target);

        let id: i64 = match target.parse() {
            Ok(i) => i,
            Err(_) => {
                match narou_rs::db::with_database(|db| Ok(db.find_by_title(target).map(|r| r.id)))
                    .ok()
                    .flatten()
                {
                    Some(i) => i,
                    None => {
                        eprintln!("  Not found: {}", target);
                        continue;
                    }
                }
            }
        };

        let novel_dir = match narou_rs::db::with_database(|db| {
            let record = db
                .get(id)
                .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
            let archive_root = db.archive_root();
            let mut dir = std::path::PathBuf::from(archive_root);
            dir.push(&record.sitename);
            if record.use_subdirectory {
                if let Some(ref ncode) = record.ncode {
                    if ncode.len() >= 2 {
                        dir.push(&ncode[..2]);
                    }
                }
            }
            dir.push(&record.file_title);
            Ok::<std::path::PathBuf, narou_rs::error::NarouError>(dir)
        }) {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!("  Error: {}", e);
                continue;
            }
        };

        match converter.convert_novel_by_id(id, &novel_dir) {
            Ok(output_path) => {
                println!("  Output: {}", output_path);
            }
            Err(e) => {
                eprintln!("  Error: {}", e);
            }
        }
    }
}

fn cmd_list(tag: Option<&str>, frozen: bool) {
    use narou_rs::db;

    if let Err(e) = db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

    let records = db::with_database(|db| {
        let mut list: Vec<_> = db.all_records().values().collect();
        list.sort_by_key(|r| r.id);

        if let Some(tag_filter) = tag {
            list.retain(|r| r.tags.iter().any(|t| t == tag_filter));
        }
        if frozen {
            list.retain(|r| r.tags.iter().any(|t| t == "frozen"));
        }

        for r in &list {
            let type_str = match r.novel_type {
                1 => "連載",
                2 => "短編",
                _ => "?",
            };
            let end_str = if r.end { " [完]" } else { "" };
            let tags_str = if r.tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", r.tags.join(", "))
            };
            println!(
                " ID:{:>4} | {} | {}{} | {} | {}",
                r.id, type_str, r.title, end_str, r.author, tags_str
            );
        }

        Ok(list.len())
    })
    .unwrap_or(0);

    println!();
    println!("Total: {} novels", records);
}

fn cmd_tag(add: Option<&str>, remove: Option<&str>, targets: &[String]) {
    use narou_rs::db;

    if let Err(e) = db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

    for target in targets {
        let id: i64 = match target.parse() {
            Ok(i) => i,
            Err(_) => {
                match db::with_database(|db| Ok(db.find_by_title(target).map(|r| r.id)))
                    .unwrap_or(None)
                {
                    Some(i) => i,
                    None => {
                        eprintln!("  Not found: {}", target);
                        continue;
                    }
                }
            }
        };

        let result = db::with_database_mut(|db| {
            let record = db
                .get(id)
                .cloned()
                .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
            let mut updated = record;
            if let Some(tag) = add {
                if !updated.tags.contains(&tag.to_string()) {
                    updated.tags.push(tag.to_string());
                }
            }
            if let Some(tag) = remove {
                updated.tags.retain(|t| t != tag);
            }
            db.insert(updated);
            db.save()
        });

        match result {
            Ok(()) => println!("  Tagged ID: {}", id),
            Err(e) => eprintln!("  Error: {}", e),
        }
    }
}

fn cmd_freeze(targets: &[String], off: bool) {
    use narou_rs::db;

    if let Err(e) = db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

    for target in targets {
        let id: i64 = match target.parse() {
            Ok(i) => i,
            Err(_) => {
                match db::with_database(|db| Ok(db.find_by_title(target).map(|r| r.id)))
                    .unwrap_or(None)
                {
                    Some(i) => i,
                    None => {
                        eprintln!("  Not found: {}", target);
                        continue;
                    }
                }
            }
        };

        let result = db::with_database_mut(|db| {
            let record = db
                .get(id)
                .cloned()
                .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
            let mut updated = record;
            if off {
                updated.tags.retain(|t| t != "frozen");
            } else if !updated.tags.contains(&"frozen".to_string()) {
                updated.tags.push("frozen".to_string());
            }
            db.insert(updated);
            db.save()
        });

        let action = if off { "Unfroze" } else { "Froze" };
        match result {
            Ok(()) => println!("  {} ID: {}", action, id),
            Err(e) => eprintln!("  Error: {}", e),
        }
    }
}

fn cmd_remove(targets: &[String]) {
    use narou_rs::db;

    if let Err(e) = db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

    for target in targets {
        let id: i64 = match target.parse() {
            Ok(i) => i,
            Err(_) => {
                match db::with_database(|db| Ok(db.find_by_title(target).map(|r| r.id)))
                    .unwrap_or(None)
                {
                    Some(i) => i,
                    None => {
                        eprintln!("  Not found: {}", target);
                        continue;
                    }
                }
            }
        };

        let result = db::with_database_mut(|db| {
            if let Some(record) = db.remove(id) {
                let novel_dir = db.archive_root().join(&record.sitename);
                if record.use_subdirectory {
                    if let Some(ref ncode) = record.ncode {
                        if ncode.len() >= 2 {
                            let dir = novel_dir.join(&ncode[..2]).join(&record.file_title);
                            let _ = std::fs::remove_dir_all(&dir);
                        }
                    }
                } else {
                    let dir = novel_dir.join(&record.file_title);
                    let _ = std::fs::remove_dir_all(&dir);
                }
                db.save()?;
                Ok::<String, narou_rs::error::NarouError>(record.title)
            } else {
                Err(narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))
            }
        });

        match result {
            Ok(title) => println!("  Removed: {} (ID: {})", title, id),
            Err(e) => eprintln!("  Error: {}", e),
        }
    }
}
