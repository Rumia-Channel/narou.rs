use std::path::PathBuf;

use narou_rs::compat::open_browser;
use narou_rs::db;
use narou_rs::db::paths::novel_dir_for_record;
use narou_rs::downloader::persistence::load_toc_file;

use super::download;
use super::help;
use super::log;

pub fn cmd_browser(targets: &[String], vote: bool) -> i32 {
    match cmd_browser_inner(targets, vote) {
        Ok(()) => 0,
        Err(err) => {
            log::report_error(&err);
            1
        }
    }
}

fn cmd_browser_inner(targets: &[String], vote: bool) -> Result<(), String> {
    db::init_database().map_err(|e| e.to_string())?;

    if targets.is_empty() {
        help::display_command_help("browser");
        return Ok(());
    }

    let expanded = download::tagname_to_ids(targets);
    for target in expanded {
        let Some((toc_url, novel_dir)) = resolve_target_urls(&target) else {
            log::report_error(&format!("{} は存在しません", target));
            continue;
        };

        let url = if vote {
            build_vote_target_url(&toc_url, &novel_dir).unwrap_or(toc_url)
        } else {
            toc_url
        };

        open_browser(&url);
    }

    Ok(())
}

fn resolve_target_urls(target: &str) -> Option<(String, PathBuf)> {
    let id = super::resolve_target_to_id(target)?;
    db::with_database(|db| {
        let archive_root = db.archive_root().to_path_buf();
        Ok(db.get(id).map(|record| {
            (
                record.toc_url.clone(),
                novel_dir_for_record(&archive_root, record),
            )
        }))
    })
    .ok()
    .flatten()
}

fn build_vote_target_url(toc_url: &str, novel_dir: &PathBuf) -> Option<String> {
    let toc = load_toc_file(novel_dir)?;
    let latest_index = toc.subtitles.last()?.index.trim();
    if latest_index.is_empty() {
        return None;
    }
    Some(build_vote_url(toc_url, latest_index))
}

fn build_vote_url(toc_url: &str, latest_index: &str) -> String {
    let mut base = toc_url.trim_end_matches('/').to_string();
    base.push('/');
    format!("{}{}/#my_novelpoint", base, latest_index)
}

#[cfg(test)]
mod tests {
    use super::build_vote_url;

    #[test]
    fn vote_url_appends_latest_index() {
        assert_eq!(
            build_vote_url("https://example.com/novel/1/", "123"),
            "https://example.com/novel/1/123/#my_novelpoint"
        );
        assert_eq!(
            build_vote_url("https://example.com/novel/1", "123"),
            "https://example.com/novel/1/123/#my_novelpoint"
        );
    }
}
