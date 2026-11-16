use narou_rs::compat::open_browser;
use narou_rs::db;

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
        let Some((record, write)) = resolve_target_urls(&target)? else {
            log::report_error(&format!("{} は存在しません", target));
            continue;
        };

        let url = if vote {
            build_vote_target_url(&record.toc_url, &write)?.unwrap_or(record.toc_url)
        } else {
            record.toc_url
        };

        open_browser(&url);
        // Ruby は開いた URL を表示する (browser.rb: `puts open_url`)。
        println!("{}", url);
    }

    Ok(())
}

fn resolve_target_urls(target: &str) -> Result<Option<(narou_rs::db::NovelRecord, narou_rs::application::novel_access::NovelWriteGuard)>, String> {
    let Some(id) = super::resolve_target_to_id(target) else { return Ok(None); };
    let record = narou_rs::native::novel_repository::NativeNovelRepository::new().get_current_sync(id.into()).map_err(|error| error.to_string())?;
    let Some(record) = record else { return Ok(None); };
    let write = narou_rs::native::materialization::capture_write_for_record(&record).map_err(|error| error.to_string())?;
    Ok(Some((record, write)))
}

fn build_vote_target_url(toc_url: &str, write: &narou_rs::application::novel_access::NovelWriteGuard) -> Result<Option<String>, String> {
    narou_rs::native::materialization::block_on(async {
        let inventory = narou_rs::db::inventory::Inventory::with_default_root()?;
        let objects = narou_rs::native::materialization::read_store(inventory.root_dir())?;
        let mut lease = narou_rs::application::novel_access::NovelReadLease::admit(&objects, write.prefix(), "browser-vote").await?;
        let result = async {
            if lease.epoch() != write.epoch() { return Err(narou_rs::error::NarouError::Platform("novel_busy: browser source changed epoch".into())); }
            let key = narou_rs::platform::ObjectKey::try_new(write.prefix().as_ref())?.join("toc.yaml")?;
            let Some(bytes) = lease.read_small(&key).await? else { return Ok(None); };
            let toc: narou_rs::downloader::TocFile = serde_yaml::from_slice(&bytes)?;
            lease.validate().await?;
            Ok(toc.subtitles.last().map(|subtitle| build_vote_url(toc_url, &subtitle.index)))
        }.await;
        let released = lease.release().await;
        match result { Ok(value) => { released?; Ok(value) }, Err(error) => Err(error) }
    }).map_err(|error| error.to_string())
}

/// Ruby は単純に `toc_url + latest_index + "/#my_novelpoint"` と連結する
/// (末尾スラッシュの正規化はしない)。
fn build_vote_url(toc_url: &str, latest_index: &str) -> String {
    format!("{}{}/#my_novelpoint", toc_url, latest_index)
}

#[cfg(test)]
mod tests {
    use super::build_vote_url;

    #[test]
    fn vote_url_appends_latest_index() {
        // Ruby の単純連結をそのまま再現する。toc_url が `/` 終わりなら
        // そのまま index が続き、無ければ二重スラッシュにはならない代わりに
        // パスがそのまま連結される (upstream と同じ挙動)。
        assert_eq!(
            build_vote_url("https://example.com/novel/1/", "123"),
            "https://example.com/novel/1/123/#my_novelpoint"
        );
        assert_eq!(
            build_vote_url("https://example.com/novel/1", "123"),
            "https://example.com/novel/1123/#my_novelpoint"
        );
    }
}
