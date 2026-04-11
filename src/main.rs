mod cli;
mod commands;

use clap::Parser;

use cli::{Cli, Commands};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let user_agent = cli.user_agent.clone();

    match cli.command {
        Commands::Init {
            aozora_path,
            line_height,
        } => {
            if let Err(e) = commands::init::cmd_init(aozora_path.as_deref(), line_height) {
                eprintln!("Error initializing: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Web { port, no_browser } => {
            commands::web::run_web_server(port, no_browser).await;
        }
        Commands::Download { targets } => {
            if targets.is_empty() {
                eprintln!("Usage: narou download <url|ncode|id>...");
                std::process::exit(1);
            }
            commands::download::cmd_download(&targets, user_agent);
        }
        Commands::Update {
            ids,
            all,
            force,
            no_convert,
            sort_by,
        } => {
            commands::update::cmd_update(commands::update::UpdateOptions {
                ids,
                all,
                force,
                no_convert,
                sort_by,
                user_agent,
            });
        }
        Commands::Convert { targets } => {
            if targets.is_empty() {
                eprintln!("Usage: narou convert <url|ncode|id>...");
                std::process::exit(1);
            }
            commands::convert::cmd_convert(&targets);
        }
        Commands::List { tag, frozen } => {
            commands::manage::cmd_list(tag.as_deref(), frozen);
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
            commands::manage::cmd_tag(add.as_deref(), remove.as_deref(), &targets);
        }
        Commands::Freeze { targets, off } => {
            if targets.is_empty() {
                eprintln!("Usage: narou freeze <targets>...");
                std::process::exit(1);
            }
            commands::manage::cmd_freeze(&targets, off);
        }
        Commands::Remove { targets } => {
            if targets.is_empty() {
                eprintln!("Usage: narou remove <targets>...");
                std::process::exit(1);
            }
            commands::manage::cmd_remove(&targets);
        }
        Commands::Setting {
            args,
            list,
            all,
            burn,
        } => {
            commands::setting::cmd_setting(&args, list, all, burn);
        }
    }
}
