#![cfg_attr(windows, windows_subsystem = "windows")]

#[macro_use]
mod output_macros;
mod backtracer;
mod cli;
mod commands;
mod logger;
#[cfg(test)]
mod test_support;

use std::any::Any;
use std::io::IsTerminal;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use clap::Parser;
use futures::FutureExt;

use cli::{Cli, Commands};

#[cfg(windows)]
fn prepare_windows_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole};

    if raw_hide_console_requested() {
        return;
    }

    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            let _ = AllocConsole();
        }
    }
}

#[cfg(not(windows))]
fn prepare_windows_console() {}

fn last_panic_detail() -> &'static Mutex<Option<String>> {
    static LAST_PANIC_DETAIL: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    LAST_PANIC_DETAIL.get_or_init(|| Mutex::new(None))
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown error".to_string()
    }
}

fn panic_detail_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = panic_payload_message(info.payload());
    if let Some(location) = info.location() {
        format!(
            "panic: {} (at {}:{}:{})",
            payload,
            location.file(),
            location.line(),
            location.column()
        )
    } else {
        format!("panic: {}", payload)
    }
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let detail = panic_detail_message(info);
        if let Ok(mut slot) = last_panic_detail().lock() {
            *slot = Some(detail.clone());
        }
        eprintln!("{}", detail);
    }));
}

#[cfg(windows)]
fn raw_hide_console_requested() -> bool {
    if narou_rs::compat::inherited_hide_console_requested() {
        return true;
    }

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--no-color" | "--multiple" | "--time" | "--backtrace" => {}
            "--user-agent" => {
                let _ = args.next();
            }
            value if value.starts_with("--user-agent=") => {}
            "-h" | "--help" | "-v" | "--version" => return false,
            value if value.starts_with('-') => {}
            command => {
                let is_web = matches!(command.to_ascii_lowercase().as_str(), "web" | "w" | "we");
                return is_web && args.any(|value| value == "--hide-console");
            }
        }
    }
    false
}

#[tokio::main]
async fn main() {
    prepare_windows_console();

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

    install_panic_hook();

    logger::init();
    logger::init_tracing(no_color);

    let trace_args = args.clone();

    if !args.is_empty() {
        cli::inject_default_args(&mut args);
        cli::inject_command_defaults(&mut args);
    }

    let start = if show_time {
        Some(Instant::now())
    } else {
        None
    };

    let trace_args_for_run = trace_args.clone();
    let exit_code = match std::panic::AssertUnwindSafe(async move {
        if !args.is_empty() && args[0] == "help" {
            commands::help::cmd_help();
            0
        } else {
            run_command(args, trace_args_for_run, user_agent, backtrace).await
        }
    })
    .catch_unwind()
    .await
    {
        Ok(code) => code,
        Err(panic_info) => {
            backtracer::save_log(&trace_args, panic_info.as_ref());
            if backtrace {
                let _ = last_panic_detail().lock().map(|mut slot| slot.take());
            } else {
                eprintln!("エラーが発生したため終了しました。");
                eprintln!("詳細なエラーログは --backtrace オプションを付けて再度実行して下さい。");
            }
            127
        }
    };

    if let Some(start) = start {
        let elapsed = start.elapsed();
        eprintln!("実行時間 {:.1}秒", elapsed.as_secs_f64());
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

async fn run_command(
    args: Vec<String>,
    trace_args: Vec<String>,
    user_agent: Option<String>,
    backtrace: bool,
) -> i32 {
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
            return 127;
        }
    };

    let ua = user_agent.or(cli.user_agent);
    logger::use_convert_log_postfix(matches!(&cli.command, Commands::Convert { .. }));

    match cli.command {
        Commands::Web {
            port,
            no_browser,
            hide_console,
        } => {
            commands::web::run_web_server(port, no_browser, hide_console).await;
            0
        }
        other => run_sync_command(other, trace_args, ua, backtrace),
    }
}

fn run_sync_command(
    command: Commands,
    trace_args: Vec<String>,
    user_agent: Option<String>,
    backtrace: bool,
) -> i32 {
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
                return 127;
            }
            commands::download::cmd_download(commands::download::DownloadOptions {
                targets,
                force,
                no_convert,
                freeze,
                remove,
                mail,
                user_agent,
            })
        }
        Commands::Mail { targets, force } => {
            commands::mail::cmd_mail(commands::mail::MailOptions { targets, force });
            0
        }
        Commands::Send {
            args,
            without_freeze,
            force,
            backup_bookmark,
            restore_bookmark,
        } => commands::send::cmd_send(commands::send::SendOptions {
            args,
            without_freeze,
            force,
            backup_bookmark,
            restore_bookmark,
        }),
        Commands::Backup { targets } => match commands::backup::cmd_backup(&targets) {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("{}", e);
                127
            }
        },
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
        Commands::Convert {
            targets,
            output,
            encoding,
            no_epub,
            no_mobi,
            no_strip,
            no_zip,
            make_zip,
            ignore_default,
            ignore_force,
            inspect,
            verbose,
            no_open,
        } => {
            if targets.is_empty() {
                eprintln!("Usage: narou convert <url|ncode|id>...");
                1
            } else {
                commands::convert::cmd_convert(
                    &targets,
                    output.as_deref(),
                    encoding.as_deref(),
                    no_epub,
                    no_mobi,
                    no_strip,
                    no_zip,
                    make_zip,
                    inspect,
                    no_open,
                    verbose,
                    ignore_default,
                    ignore_force,
                );
                0
            }
        }
        Commands::Diff {
            target,
            view_diff_version,
            number,
            list,
            clean,
            all_clean,
            no_tool,
        } => commands::diff::cmd_diff(commands::diff::DiffOptions {
            target,
            view_diff_version,
            number: number.unwrap_or(1).max(1),
            list,
            clean,
            all_clean,
            no_tool,
        }),
        Commands::List {
            limit,
            latest,
            general_lastup,
            reverse,
            url,
            kind,
            site,
            author,
            filter,
            grep,
            tag,
            echo,
            frozen,
        } => commands::manage::cmd_list(commands::manage::ListOptions {
            limit,
            latest,
            general_lastup,
            reverse,
            url,
            kind,
            site,
            author,
            filter,
            grep,
            tag,
            echo,
            frozen,
        }),
        Commands::Tag {
            add,
            delete,
            color,
            clear,
            no_overwrite_color,
            targets,
        } => commands::manage::cmd_tag(commands::manage::TagOptions {
            add,
            delete,
            color,
            clear,
            no_overwrite_color,
            targets,
        }),
        Commands::Freeze {
            targets,
            list,
            on,
            off,
        } => {
            commands::manage::cmd_freeze(&targets, list, on, off);
            0
        }
        Commands::Remove {
            targets,
            yes,
            with_file,
            all_ss,
        } => commands::manage::cmd_remove(&targets, yes, with_file, all_ss),
        Commands::Setting {
            args,
            list,
            all,
            burn,
        } => {
            commands::setting::cmd_setting(&args, list, all, burn);
            0
        }
        Commands::Alias { args, list } => commands::alias::cmd_alias(&args, list),
        Commands::Folder { targets, no_open } => commands::folder::cmd_folder(&targets, no_open),
        Commands::Browser { targets, vote } => commands::browser::cmd_browser(&targets, vote),
        Commands::Clean {
            targets,
            force,
            dry_run,
            all,
        } => commands::clean::cmd_clean(&targets, force, dry_run, all),
        Commands::Inspect { targets } => commands::inspect::cmd_inspect(&targets),
        Commands::Csv { output, import } => {
            commands::csv::cmd_csv(output.as_deref(), import.as_deref())
        }
        Commands::Trace => match commands::trace::cmd_trace() {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("{}", e);
                127
            }
        },
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
            backtracer::save_log(&trace_args, panic_info.as_ref());
            if backtrace {
                let _ = last_panic_detail().lock().map(|mut slot| slot.take());
            } else {
                eprintln!("エラーが発生したため終了しました。");
                eprintln!("詳細なエラーログは --backtrace オプションを付けて再度実行して下さい。");
            }
            127
        }
    }
}
