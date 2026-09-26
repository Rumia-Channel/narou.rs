use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use narou_rs::application::messages::{self, MessageSink, Stream};
use narou_rs::mail::{
    MAIL_INTERRUPTED_MESSAGE, MailSettingLoadError, ensure_mail_setting_file, load_mail_setting,
    send_target_with_setting_interruptible,
};

pub struct MailOptions {
    pub targets: Vec<String>,
    pub force: bool,
}

static MAIL_INTERRUPT_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

pub fn cmd_mail(opts: MailOptions, sink: &Arc<dyn MessageSink>) {
    if let Err(code) = cmd_mail_inner(opts, sink.as_ref()) {
        std::process::exit(code);
    }
}

fn cmd_mail_inner(opts: MailOptions, sink: &dyn MessageSink) -> Result<(), i32> {
    if let Err(e) = narou_rs::db::init_database() {
        sink.emit(Stream::Stderr, &messages::init_db_error(e));
        return Err(1);
    }

    let interrupted = mail_interrupt_flag(sink)?;
    interrupted.store(false, Ordering::SeqCst);

    let setting = match load_mail_setting() {
        Ok(setting) => setting,
        Err(MailSettingLoadError::NotFound(_)) => {
            let path = ensure_mail_setting_file().map_err(|e| {
                sink.emit(Stream::Stderr, &messages::error_line(e));
                1
            })?;
            sink.emit(Stream::Stdout, &messages::mail::mail_setting_created(&path));
            sink.emit(Stream::Stdout, messages::mail::mail_setting_file_notice());
            sink.emit(
                Stream::Stdout,
                messages::mail::mail_setting_next_update_notice(),
            );
            return Ok(());
        }
        Err(e @ MailSettingLoadError::Incomplete(_)) => {
            sink.emit(Stream::Stderr, &e.to_string());
            return Err(127);
        }
        Err(e) => {
            sink.emit(Stream::Stderr, &messages::error_line(e));
            return Err(127);
        }
    };

    let send_all = opts.targets.is_empty();
    let targets = if send_all {
        collect_all_targets()
    } else {
        expand_targets(&opts.targets)
    };

    for target in targets {
        if let Err(e) = send_target_with_setting_interruptible(
            &setting,
            &target,
            send_all,
            opts.force,
            Some(interrupted.as_ref()),
        ) {
            if e == MAIL_INTERRUPTED_MESSAGE {
                sink.emit(Stream::Stdout, &e);
                return Err(126);
            }
            sink.emit(Stream::Stderr, &e);
            return Err(127);
        }
    }

    Ok(())
}

fn mail_interrupt_flag(sink: &dyn MessageSink) -> Result<Arc<AtomicBool>, i32> {
    if let Some(flag) = MAIL_INTERRUPT_FLAG.get() {
        return Ok(flag.clone());
    }

    let flag = Arc::new(AtomicBool::new(false));
    let handler_flag = flag.clone();
    ctrlc::set_handler(move || {
        handler_flag.store(true, Ordering::SeqCst);
    })
    .map_err(|e| {
        sink.emit(Stream::Stderr, &messages::error_line(e));
        1
    })?;

    let _ = MAIL_INTERRUPT_FLAG.set(flag.clone());
    Ok(flag)
}

fn collect_all_targets() -> Vec<String> {
    let frozen_ids = narou_rs::compat::load_frozen_ids().unwrap_or_default();
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let mut ids: Vec<i64> = novels
        .scan_ids_sync(&narou_rs::platform::NovelFilter::all(), None, usize::MAX)
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect();
    ids.sort_unstable();
    ids.into_iter()
        .filter(|id| {
            novels
                .get_sync((*id).into())
                .ok()
                .flatten()
                .map(|record| !narou_rs::compat::record_is_frozen(&record, &frozen_ids))
                .unwrap_or(false)
        })
        .map(|id| id.to_string())
        .collect()
}

fn expand_targets(targets: &[String]) -> Vec<String> {
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let all_ids: Vec<i64> = novels
        .scan_ids_sync(&narou_rs::platform::NovelFilter::all(), None, usize::MAX)
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect();
    let mut all_sorted = all_ids;
    all_sorted.sort_unstable();
    let tag_ids = |tag_name: &str| -> Vec<i64> {
        let filter = narou_rs::platform::NovelFilter {
            tag: Some(tag_name.to_string()),
            ..Default::default()
        };
        novels
            .scan_ids_sync(&filter, None, usize::MAX)
            .unwrap_or_default()
            .into_iter()
            .map(|id| id.0)
            .collect()
    };

    let mut expanded = Vec::new();
    for target in targets {
        if let Ok(id) = target.parse::<i64>() {
            let exists = novels.get_sync(id.into()).ok().flatten().is_some();
            if exists {
                expanded.push(id.to_string());
                continue;
            }
        }

        if let Some(tag_name) = target.strip_prefix("^tag:") {
            let exclude: std::collections::HashSet<i64> = tag_ids(tag_name).into_iter().collect();
            if !exclude.is_empty() {
                expanded.extend(
                    all_sorted
                        .iter()
                        .filter(|id| !exclude.contains(id))
                        .map(|id| id.to_string()),
                );
            } else {
                expanded.push(tag_name.to_string());
            }
            continue;
        }

        if let Some(tag_name) = target.strip_prefix("tag:") {
            let ids = tag_ids(tag_name);
            if !ids.is_empty() {
                expanded.extend(ids.iter().map(|id| id.to_string()));
            } else {
                expanded.push(tag_name.to_string());
            }
            continue;
        }

        let ids = tag_ids(target);
        if !ids.is_empty() {
            expanded.extend(ids.iter().map(|id| id.to_string()));
        } else {
            expanded.push(target.clone());
        }
    }

    let mut seen = std::collections::HashSet::new();
    expanded
        .into_iter()
        .filter(|target| seen.insert(target.clone()))
        .collect()
}
