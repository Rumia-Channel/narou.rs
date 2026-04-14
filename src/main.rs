#[macro_use]
mod output_macros;
mod cli;
mod commands;
mod logger;

use std::io::IsTerminal;
use std::time::Instant;

use clap::Parser;

use cli::{Cli, Commands};

#[tokio::main]
async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    let global_flags = cli::preprocess_args(&mut args);
    let show_time = global_flags.show_time;
    let backtrace = global_flags.backtrace;
    let no_color = global_flags.no_color;
    let user_agent = global_flags.user_agent.clone();

    if no_color {
        unsafe {
            std::env::set_var("NO_COLOR", "1");
        }
    }

    logger::init();
    logger::init_tracing(no_color);

    if !args.is_empty() {
        cli::inject_default_args(&mut args);
        cli::inject_command_defaults(&mut args);
    }

    let start = if show_time {
        Some(Instant::now())
    } else {
        None
    };

    let exit_code = if !args.is_empty() && args[0] == "help" {
        commands::help::cmd_help();
        0
    } else {
        run_command(args, user_agent, backtrace).await
    };

    if let Some(start) = start {
        let elapsed = start.elapsed();
        eprintln!("実行時間 {:.1}秒", elapsed.as_secs_f64());
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

async fn run_command(args: Vec<String>, user_agent: Option<String>, backtrace: bool) -> i32 {
    let cli = match Cli::try_parse_from(std::iter::once("narou".to_string()).chain(args)) {
        Ok(cli) => cli,
        Err(e) => {
            if backtrace {
                eprintln!("{}", e);
            } else {
                let msg = format!("{}", e);
                for line in msg.lines().take(5) {
                    eprintln!("{}", line);
                }
            }
            return 1;
        }
    };

    let ua = user_agent.or(cli.user_agent);

    match cli.command {
        Commands::Web { port, no_browser } => {
            commands::web::run_web_server(port, no_browser).await;
            0
        }
        other => run_sync_command(other, ua, backtrace),
    }
}

fn run_sync_command(command: Commands, user_agent: Option<String>, backtrace: bool) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match command {
        Commands::Init {
            aozora_path,
            line_height,
        } => {
            if let Err(e) = commands::init::cmd_init(aozora_path.as_deref(), line_height) {
                eprintln!("Error initializing: {}", e);
                1
            } else {
                0
            }
        }
        Commands::Download {
            targets,
            force,
            no_convert,
            freeze,
            remove,
            mail,
        } => {
            if targets.is_empty() && !std::io::stdin().is_terminal() {
                eprintln!("Usage: narou download <url|ncode|id>...");
                return 1;
            }
            commands::download::cmd_download(commands::download::DownloadOptions {
                targets,
                force,
                no_convert,
                freeze,
                remove,
                mail,
                user_agent,
            });
            0
        }
        Commands::Mail { targets, force } => {
            commands::mail::cmd_mail(commands::mail::MailOptions { targets, force });
            0
        }
        Commands::Update {
            ids,
            force,
            no_convert,
            convert_only_new_arrival,
            gl,
            sort_by,
            ignore_all,
        } => {
            commands::update::cmd_update(commands::update::UpdateOptions {
                ids,
                force,
                no_convert,
                convert_only_new_arrival,
                gl,
                sort_by,
                ignore_all,
                user_agent,
            });
            0
        }
        Commands::Convert { targets } => {
            if targets.is_empty() {
                eprintln!("Usage: narou convert <url|ncode|id>...");
                1
            } else {
                commands::convert::cmd_convert(&targets);
                0
            }
        }
        Commands::List { tag, frozen } => {
            commands::manage::cmd_list(tag.as_deref(), frozen);
            0
        }
        Commands::Tag {
            add,
            remove,
            targets,
        } => {
            if targets.is_empty() {
                eprintln!("Usage: narou tag --add <tag> <targets>...");
                1
            } else {
                commands::manage::cmd_tag(add.as_deref(), remove.as_deref(), &targets);
                0
            }
        }
        Commands::Freeze {
            targets,
            list,
            on,
            off,
        } => {
            commands::manage::cmd_freeze(&targets, list, on, off);
            0
        }
        Commands::Remove { targets } => {
            if targets.is_empty() {
                eprintln!("Usage: narou remove <targets>...");
                1
            } else {
                commands::manage::cmd_remove(&targets);
                0
            }
        }
        Commands::Setting {
            args,
            list,
            all,
            burn,
        } => {
            commands::setting::cmd_setting(&args, list, all, burn);
            0
        }
        Commands::Log {
            path,
            num,
            tail,
            source_convert,
        } => match commands::log::cmd_log(path.as_deref(), num, tail, source_convert) {
            Ok(_) => 0,
            Err(e) => {
                commands::log::report_error(&e);
                127
            }
        },
        Commands::Version { more } => {
            commands::version::cmd_version(more);
            0
        }
        Commands::Web { .. } => unreachable!(),
    }));

    match result {
        Ok(code) => code,
        Err(panic_info) => {
            if backtrace {
                if let Some(s) = panic_info.downcast_ref::<&str>() {
                    eprintln!("panic: {}", s);
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    eprintln!("panic: {}", s);
                } else {
                    eprintln!("panic: unknown error");
                }
            } else {
                eprintln!("エラーが発生したため終了しました。");
                eprintln!("詳細なエラーログは --backtrace オプションを付けて再度実行して下さい。");
            }
            127
        }
    }
}
