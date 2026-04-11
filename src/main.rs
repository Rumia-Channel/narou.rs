use std::io::{self, IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

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
    Init {
        #[arg(short = 'p', long = "path")]
        aozora_path: Option<String>,
        #[arg(short = 'l', long = "line-height")]
        line_height: Option<f64>,
    },
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
        Commands::Init {
            aozora_path,
            line_height,
        } => {
            if let Err(e) = cmd_init(aozora_path.as_deref(), line_height) {
                eprintln!("Error initializing: {}", e);
                std::process::exit(1);
            }
        }
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

fn cmd_init(aozora_path: Option<&str>, line_height: Option<f64>) -> narou_rs::error::Result<()> {
    let cwd = std::env::current_dir()?;
    let already_root = find_existing_narou_root(&cwd);
    let root = already_root.clone().unwrap_or(cwd);

    if already_root.is_none() {
        std::fs::create_dir_all(root.join(".narou"))?;
        println!(".narou/ を作成しました");

        let archive_root = root.join("小説データ");
        std::fs::create_dir_all(&archive_root)?;
        println!("小説データ/ を作成しました");

        let user_webnovel_dir = root.join("webnovel");
        std::fs::create_dir_all(&user_webnovel_dir)?;
        let copied = copy_bundled_webnovel_files(&user_webnovel_dir)?;
        if copied == 0 {
            println!("webnovel/ を作成しました");
        } else {
            println!("webnovel/ を作成しました ({} files)", copied);
        }
    } else {
        println!("既に初期化済みです: {}", root.display());
    }

    let created_inventory = ensure_dot_narou_files(&root)?;
    if created_inventory > 0 {
        println!(
            ".narou/ に初期ファイルを作成しました ({} files)",
            created_inventory
        );
    }

    init_aozoraepub3_settings(aozora_path, line_height, already_root.is_some())?;

    if already_root.is_none() {
        println!("初期化が完了しました！");
    }

    Ok(())
}

async fn run_web_server(port: u16, no_browser: bool) {
    use narou_rs::web;

    info!("Starting narou.rs web server on port {}", port);

    if let Err(e) = narou_rs::db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

    let app = web::create_router(port);
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
    use narou_rs::converter::user_converter::UserConverter;

    if let Err(e) = narou_rs::db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

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

        let (novel_dir, title, author) = match narou_rs::db::with_database(|db| {
            let record = db
                .get(id)
                .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
            let dir = narou_rs::db::existing_novel_dir_for_record(db.archive_root(), record);
            Ok::<(std::path::PathBuf, String, String), narou_rs::error::NarouError>((
                dir,
                record.title.clone(),
                record.author.clone(),
            ))
        }) {
            Ok(data) => data,
            Err(e) => {
                eprintln!("  Error: {}", e);
                continue;
            }
        };

        let settings = NovelSettings::load_for_novel(id, &title, &author, &novel_dir);
        let mut converter =
            if let Some(user_converter) = UserConverter::load_with_title(&novel_dir, &title) {
                NovelConverter::with_user_converter(settings, user_converter)
            } else {
                NovelConverter::new(settings)
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
                let dir = db::existing_novel_dir_for_record(db.archive_root(), &record);
                let _ = std::fs::remove_dir_all(&dir);
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

fn find_existing_narou_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".narou").is_dir() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn ensure_dot_narou_files(root: &Path) -> narou_rs::error::Result<usize> {
    let dir = root.join(".narou");
    std::fs::create_dir_all(&dir)?;

    let files = [
        ("local_setting.yaml", "--- {}\n"),
        ("database.yaml", "--- {}\n"),
        (
            "database_index.yaml",
            "---\nby_toc_url: {}\nby_title: {}\nmeta: {}\n",
        ),
        ("alias.yaml", "--- {}\n"),
        ("freeze.yaml", "--- {}\n"),
        ("tag_colors.yaml", "--- {}\n"),
        ("latest_convert.yaml", "--- {}\n"),
        ("queue.yaml", "---\njobs: []\ncompleted: []\nfailed: []\n"),
        ("notepad.txt", ""),
    ];

    let mut created = 0usize;
    for (name, content) in files {
        let path = dir.join(name);
        if !path.exists() {
            std::fs::write(path, content)?;
            created += 1;
        }
    }
    Ok(created)
}

fn copy_bundled_webnovel_files(destination: &Path) -> narou_rs::error::Result<usize> {
    let source = bundled_webnovel_dir();
    let Some(source) = source else {
        return Ok(0);
    };

    let mut copied = 0usize;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let is_yaml = matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("yaml") | Some("yml")
        );
        if !is_yaml {
            continue;
        }
        let filename = match path.file_name() {
            Some(name) => name,
            None => continue,
        };
        let target = destination.join(filename);
        if !target.exists() {
            std::fs::copy(&path, &target)?;
            copied += 1;
        }
    }
    Ok(copied)
}

fn bundled_webnovel_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("webnovel"));
        }
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("webnovel"));

    candidates.into_iter().find(|path| path.is_dir())
}

fn init_aozoraepub3_settings(
    aozora_path: Option<&str>,
    line_height: Option<f64>,
    force: bool,
) -> narou_rs::error::Result<()> {
    let global_dir = home_dir().join(".narousetting");
    let global_path = global_dir.join("global_setting.yaml");

    let mut settings = if global_path.exists() {
        let raw = std::fs::read_to_string(&global_path)?;
        serde_yaml::from_str::<std::collections::BTreeMap<String, serde_yaml::Value>>(&raw)
            .unwrap_or_default()
    } else {
        std::collections::BTreeMap::new()
    };

    if !force && aozora_path.is_none() && line_height.is_none() && settings.contains_key("aozoraepub3dir") {
        return Ok(());
    }

    println!("AozoraEpub3の設定を行います");
    if !settings.contains_key("aozoraepub3dir") {
        println!("!!!WARNING!!!");
        println!("AozoraEpub3の構成ファイルを書き換えます。narouコマンド用に別途新規インストールしておくことをオススメします");
    }

    let resolved_aozora_path = resolve_init_aozora_path(aozora_path, &settings)?;
    let Some(resolved_aozora_path) = resolved_aozora_path else {
        if aozora_path.is_some() {
            println!("指定されたフォルダにAozoraEpub3がありません。");
        }
        println!("AozoraEpub3 の設定をスキップしました");
        return Ok(());
    };

    let height = match line_height {
        Some(height) => height,
        None if io::stdin().is_terminal() => ask_line_height(&settings)?,
        None => settings
            .get("line-height")
            .and_then(|value| value.as_f64())
            .unwrap_or(1.8),
    };

    settings.insert(
        "aozoraepub3dir".to_string(),
        serde_yaml::Value::String(resolved_aozora_path.clone()),
    );
    settings.insert(
        "line-height".to_string(),
        serde_yaml::to_value(height).unwrap_or(serde_yaml::Value::Null),
    );

    rewrite_aozoraepub3_files(&resolved_aozora_path, height)?;

    let content = serde_yaml::to_string(&settings)?;
    std::fs::create_dir_all(&global_dir)?;
    std::fs::write(global_path, content)?;
    println!("グローバル設定を保存しました");

    Ok(())
}

fn resolve_init_aozora_path(
    aozora_path: Option<&str>,
    settings: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> narou_rs::error::Result<Option<String>> {
    match aozora_path {
        Some(":keep") => Ok(settings
            .get("aozoraepub3dir")
            .and_then(|value| value.as_str())
            .and_then(validate_aozoraepub3_path)),
        Some(path) => Ok(validate_aozoraepub3_path(path)),
        None if io::stdin().is_terminal() => ask_aozoraepub3_path(settings),
        None => Ok(settings
            .get("aozoraepub3dir")
            .and_then(|value| value.as_str())
            .and_then(validate_aozoraepub3_path)),
    }
}

fn ask_aozoraepub3_path(
    settings: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> narou_rs::error::Result<Option<String>> {
    let current_path = settings.get("aozoraepub3dir").and_then(|value| value.as_str());
    println!();
    println!("AozoraEpub3のあるフォルダを入力して下さい:");
    if let Some(current_path) = current_path {
        println!("(未入力でスキップ、:keep で現在と同じ場所を指定)");
        println!("(現在の場所:{})", current_path);
    } else {
        println!("(未入力でスキップ)");
    }

    loop {
        print!(">");
        io::stdout().flush()?;
        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            return Ok(None);
        }
        let input = input.trim();
        if input.is_empty() {
            return Ok(None);
        }
        if input == ":keep" {
            if let Some(path) = current_path.and_then(validate_aozoraepub3_path) {
                return Ok(Some(path));
            }
        } else if let Some(path) = validate_aozoraepub3_path(input) {
            return Ok(Some(path));
        }
        println!("入力されたフォルダにAozoraEpub3がありません。もう一度入力して下さい:");
    }
}

fn ask_line_height(
    settings: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> narou_rs::error::Result<f64> {
    let default = settings
        .get("line-height")
        .and_then(|value| value.as_f64())
        .unwrap_or(1.8);

    println!();
    println!("行間の調整を行います。小説の行の高さを設定して下さい(単位 em):");
    println!("1em = 1文字分の高さ");
    println!("行の高さ＝1文字分の高さ＋行間の高さ");
    println!("オススメは 1.8");
    println!("(未入力で {} を採用)", format_line_height(default));

    loop {
        print!(">");
        io::stdout().flush()?;
        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            return Ok(default);
        }
        let input = input.trim();
        if input.is_empty() {
            return Ok(default);
        }
        match input.parse::<f64>() {
            Ok(value) => return Ok(value),
            Err(_) => println!("数値を入力して下さい:"),
        }
    }
}

fn validate_aozoraepub3_path(path: &str) -> Option<String> {
    let normalized = normalize_path_string(path);
    if PathBuf::from(&normalized).join("AozoraEpub3.jar").exists() {
        Some(normalized)
    } else {
        None
    }
}

fn rewrite_aozoraepub3_files(aozora_path: &str, line_height: f64) -> narou_rs::error::Result<()> {
    let preset_dir = preset_dir()?;
    let aozora_dir = PathBuf::from(aozora_path);

    let custom_chuki_tag = std::fs::read_to_string(preset_dir.join("custom_chuki_tag.txt"))?;
    let chuki_tag_path = aozora_dir.join("chuki_tag.txt");
    let mut chuki_tag = std::fs::read_to_string(&chuki_tag_path)?;
    let embedded_mark = "### Narou.rb embedded custom chuki ###";
    if let (Some(start), Some(end)) = (chuki_tag.find(embedded_mark), chuki_tag.rfind(embedded_mark))
    {
        if start != end {
            let end = end + embedded_mark.len();
            chuki_tag.replace_range(start..end, &custom_chuki_tag);
        } else {
            chuki_tag.push('\n');
            chuki_tag.push_str(&custom_chuki_tag);
        }
    } else {
        chuki_tag.push('\n');
        chuki_tag.push_str(&custom_chuki_tag);
    }
    std::fs::write(&chuki_tag_path, chuki_tag)?;

    std::fs::copy(
        preset_dir.join("AozoraEpub3.ini"),
        aozora_dir.join("AozoraEpub3.ini"),
    )?;

    let vertical_font = std::fs::read_to_string(preset_dir.join("vertical_font.css"))?
        .replace("<%= line_height %>", &format_line_height(line_height));
    let vertical_font_path = aozora_dir
        .join("template")
        .join("OPS")
        .join("css_custom")
        .join("vertical_font.css");
    if let Some(parent) = vertical_font_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(vertical_font_path, vertical_font)?;

    println!("AozoraEpub3 の構成ファイルを書き換えました");
    Ok(())
}

fn preset_dir() -> narou_rs::error::Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("preset"));
        }
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    candidates.push(manifest_dir.join("preset"));
    candidates.push(manifest_dir.join("sample").join("narou").join("preset"));

    candidates.into_iter().find(|path| path.is_dir()).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "narou preset directory not found").into()
    })
}

fn format_line_height(line_height: f64) -> String {
    let mut text = line_height.to_string();
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    text
}

fn normalize_path_string(path: &str) -> String {
    let path = path.trim_matches('"');
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| PathBuf::from(path))
        .display()
        .to_string()
}

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
        })
}
