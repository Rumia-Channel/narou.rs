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
        Commands::Tag { add, remove, targets } => {
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
    println!("Downloading: {:?}", targets);
    println!("(Download not yet fully implemented)");
}

fn cmd_update(ids: Option<Vec<i64>>, all: bool) {
    if all {
        println!("Updating all novels");
    } else if let Some(id_list) = ids {
        println!("Updating novels: {:?}", id_list);
    } else {
        println!("Usage: narou update --all | narou update <id>...");
        std::process::exit(1);
    }
    println!("(Update not yet fully implemented)");
}

fn cmd_convert(targets: &[String]) {
    println!("Converting: {:?}", targets);
    println!("(Convert not yet fully implemented)");
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
                1 => "\u{9023}\u{8F09}",
                2 => "\u{77ED}\u{7DE8}",
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

    println!("\nTotal: {} novels", records);
}

fn cmd_tag(add: Option<&str>, remove: Option<&str>, targets: &[String]) {
    println!("Tag operation: add={:?}, remove={:?}, targets={:?}", add, remove, targets);
    println!("(Tag not yet fully implemented)");
}

fn cmd_freeze(targets: &[String], off: bool) {
    let action = if off { "Unfreezing" } else { "Freezing" };
    println!("{}: {:?}", action, targets);
    println!("(Freeze not yet fully implemented)");
}

fn cmd_remove(targets: &[String]) {
    println!("Removing: {:?}", targets);
    println!("(Remove not yet fully implemented)");
}
