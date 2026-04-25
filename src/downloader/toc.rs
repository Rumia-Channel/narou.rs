use std::collections::HashMap;

use crate::error::{NarouError, Result};
use crate::progress::ProgressReporter;

use super::fetch::HttpFetcher;
use super::novel_info::NovelInfo;
use super::site_setting::SiteSetting;
use super::types::SubtitleInfo;
use super::util::{
    compile_html_pattern, load_length_limit, pretreatment_source, sanitize_filename,
    sanitize_filename_with_limit,
};

pub fn fetch_toc(
    fetcher: &mut HttpFetcher,
    setting: &SiteSetting,
    toc_url: &str,
) -> Result<String> {
    fetcher.rate_limiter.wait_for_url(toc_url);

    let body = fetcher.fetch_text(toc_url, setting.cookie(), Some(setting.encoding()))?;
    let mut body = body;
    pretreatment_source(&mut body, setting.encoding(), Some(setting));

    if let Some(error_pattern) = setting.error_message() {
        if let Ok(re) = compile_html_pattern(error_pattern) {
            if re.is_match(&body) {
                return Err(NarouError::NotFound("Novel deleted or private".into()));
            }
        }
    }

    Ok(body)
}

pub fn parse_subtitles(
    setting: &SiteSetting,
    toc_source: &str,
    url_captures: &HashMap<String, String>,
) -> Result<Vec<SubtitleInfo>> {
    let subtitles_pattern = setting
        .subtitles_pattern()
        .ok_or_else(|| NarouError::SiteSetting("No subtitles pattern defined".into()))?;
    let filename_length_limit = load_length_limit("filename-length-limit", Some(50));

    let mut subtitles = Vec::new();
    let mut remaining = toc_source;

    while let Some(caps) = subtitles_pattern.captures(remaining) {
        let full_match = caps.get(0).unwrap();
        remaining = &remaining[full_match.end()..];

        let index = caps
            .name("index")
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let href = caps
            .name("href")
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| {
                if let Some(href_tpl) = &setting.href {
                    setting.interpolate_subtitles_href(href_tpl, &index, url_captures)
                } else {
                    String::new()
                }
            });
        let chapter = caps
            .name("chapter")
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let subchapter = caps
            .name("subchapter")
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let subtitle_raw = caps
            .name("subtitle")
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let subdate = caps
            .name("subdate")
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let subupdate = caps.name("subupdate").map(|m| m.as_str().to_string());
        let subdate = if subdate.is_empty() {
            subupdate.clone().unwrap_or_default()
        } else {
            subdate
        };

        let file_subtitle = match filename_length_limit {
            Some(limit) => {
                let reserved = index.chars().count() + 1;
                let subtitle_limit = limit.saturating_sub(reserved);
                sanitize_filename_with_limit(&subtitle_raw, Some(subtitle_limit))
            }
            None => sanitize_filename_with_limit(&subtitle_raw, None),
        };

        subtitles.push(SubtitleInfo {
            index,
            href,
            chapter,
            subchapter,
            subtitle: subtitle_raw,
            file_subtitle,
            subdate,
            subupdate,
            download_time: None,
        });

        if full_match.end() == 0 {
            break;
        }
    }

    Ok(subtitles)
}

pub fn parse_subtitles_multipage(
    fetcher: &mut HttpFetcher,
    setting: &SiteSetting,
    toc_source: &str,
    url_captures: &HashMap<String, String>,
    title: &str,
    progress: Option<&dyn ProgressReporter>,
) -> Result<Vec<SubtitleInfo>> {
    parse_subtitles_multipage_with(
        fetcher,
        setting,
        toc_source,
        url_captures,
        title,
        progress,
        fetch_toc,
    )
}

fn parse_subtitles_multipage_with<F>(
    fetcher: &mut HttpFetcher,
    setting: &SiteSetting,
    toc_source: &str,
    url_captures: &HashMap<String, String>,
    title: &str,
    progress: Option<&dyn ProgressReporter>,
    mut fetch_next_toc: F,
) -> Result<Vec<SubtitleInfo>>
where
    F: FnMut(&mut HttpFetcher, &SiteSetting, &str) -> Result<String>,
{
    let mut all_subtitles = Vec::new();
    let mut current_toc_source = toc_source.to_string();
    let mut page = 0;
    let max_pages = if let Some(pattern) = setting.toc_page_max_pattern() {
        pattern
            .captures(&current_toc_source)
            .and_then(|caps| caps.get(1))
            .and_then(|m| m.as_str().parse::<usize>().ok())
            .unwrap_or(1)
            .max(1)
    } else {
        50
    };
    let show_progress = max_pages >= 5 && !title.is_empty();
    if show_progress {
        if let Some(progress) = progress {
            progress.set_position(0);
            progress.set_length(max_pages as u64);
            progress.set_message(&format!("目次 {}", title));
        }
    }

    loop {
        let page_subs = parse_subtitles(setting, &current_toc_source, url_captures)?;
        all_subtitles.extend(page_subs);

        page += 1;
        if show_progress {
            if let Some(progress) = progress {
                progress.inc(1);
            }
        }
        if page >= max_pages {
            break;
        }

        let next_toc_pattern = match setting.next_toc_pattern() {
            Some(p) => p,
            None => break,
        };

        let caps = match next_toc_pattern.captures(&current_toc_source) {
            Some(c) => c,
            None => break,
        };

        let mut next_captures: HashMap<String, String> = HashMap::new();
        for name in next_toc_pattern.capture_names().flatten() {
            if let Some(m) = caps.name(name) {
                next_captures.insert(name.to_string(), m.as_str().to_string());
            }
        }

        let next_url_val = match &setting.next_url {
            Some(u) => u.clone(),
            None => break,
        };
        let next_url = setting.get_next_url_with_captures(&next_url_val, &next_captures);

        current_toc_source = fetch_next_toc(fetcher, setting, &next_url)?;
    }

    if show_progress {
        if let Some(progress) = progress {
            progress.set_position(0);
        }
    }

    Ok(all_subtitles)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct MockProgress {
        lengths: Mutex<Vec<u64>>,
        positions: Mutex<Vec<u64>>,
        increments: Mutex<Vec<u64>>,
        messages: Mutex<Vec<String>>,
    }

    impl ProgressReporter for MockProgress {
        fn set_length(&self, len: u64) {
            self.lengths.lock().unwrap().push(len);
        }

        fn set_position(&self, pos: u64) {
            self.positions.lock().unwrap().push(pos);
        }

        fn inc(&self, delta: u64) {
            self.increments.lock().unwrap().push(delta);
        }

        fn set_message(&self, msg: &str) {
            self.messages.lock().unwrap().push(msg.to_string());
        }

        fn finish_with_message(&self, _msg: &str) {}

        fn println(&self, _msg: &str) {}
    }

    #[test]
    fn multipage_toc_uses_existing_progress_for_long_series() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings
            .iter()
            .find(|s| s.domain == "ncode.syosetu.com")
            .unwrap();
        let toc_source = r#"
<a href="/n1234aa/?p=5" class="c-pager__item c-pager__item--last">5</a>
<div class="p-eplist__sublist">
<a href="/n1234aa/1/" class="p-eplist__subtitle">
第1話
</a>
<div class="p-eplist__update">
2024年01月01日 00時00分
</div>
</div>
"#;
        let mut fetcher = HttpFetcher::new("test-agent").unwrap();
        let progress = MockProgress::default();

        let subtitles = parse_subtitles_multipage(
            &mut fetcher,
            setting,
            toc_source,
            &HashMap::new(),
            "テスト作品",
            Some(&progress),
        )
        .unwrap();

        assert!(subtitles.is_empty() || subtitles.len() == 1);
        assert_eq!(*progress.lengths.lock().unwrap(), vec![5]);
        assert_eq!(*progress.increments.lock().unwrap(), vec![1]);
        assert_eq!(
            *progress.messages.lock().unwrap(),
            vec!["目次 テスト作品".to_string()]
        );
        assert_eq!(*progress.positions.lock().unwrap(), vec![0, 0]);
    }

    #[test]
    fn r18_multipage_toc_fetches_following_pages_via_next_url() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings
            .iter()
            .find(|s| s.domain == "novel18.syosetu.com")
            .unwrap();
        let first_page = r#"
<a href="/n1234aa/?p=2" class="c-pager__item c-pager__item--last">2</a>
<div class="p-eplist__sublist">
<a href="/n1234aa/1/" class="p-eplist__subtitle">
第1話
</a>

<div class="p-eplist__update">
2024年01月01日 00時00分
</div>
</div>
<a href="/n1234aa/?p=2" class="c-pager__item c-pager__item--next">
"#;
        let second_page = r#"
<div class="p-eplist__sublist">
<a href="/n1234aa/2/" class="p-eplist__subtitle">
第2話
</a>

<div class="p-eplist__update">
2024年01月02日 00時00分
</div>
</div>
"#;
        let mut fetcher = HttpFetcher::new("test-agent").unwrap();
        let mut fetched_urls = Vec::new();

        let subtitles = parse_subtitles_multipage_with(
            &mut fetcher,
            setting,
            first_page,
            &HashMap::new(),
            "",
            None,
            |_, _, next_url| {
                fetched_urls.push(next_url.to_string());
                Ok(second_page.to_string())
            },
        )
        .unwrap();

        assert_eq!(
            fetched_urls,
            vec!["https://novel18.syosetu.com/n1234aa/?p=2".to_string()]
        );
        assert_eq!(subtitles.len(), 2);
        assert_eq!(subtitles[0].index, "1");
        assert_eq!(subtitles[1].index, "2");
        assert_eq!(subtitles[1].href, "/n1234aa/2/");
    }

    #[test]
    fn akatsuki_toc_pattern_extracts_sections_with_nbsp_indent() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "暁").unwrap();
        let toc_source = r#"
<table class="list" cellpadding="0" cellspacing="0"><thead><tr><th>タイトル</th><th width="250">更新日時</th></tr></thead><tbody><tr><td style="border: 0; padding: 0;word-break:break-all;" colspan=\"2\"><b>ゼンヒ巡査部長編</b></td></tr><tr><td>&nbsp;&nbsp;<a href="/stories/view/313728/novel_id~31149">プロローグ:新任巡査部長、扇皇 ゼンヒ</a>&nbsp;</td><td class="font-s">2025年 10月 24日 07時 00分&nbsp;</td></tr><tr><td><a href="/stories/view/313729/novel_id~31149">Case1:三毛猫の捜索</a>&nbsp;</td><td class="font-s">2025年 10月 24日 12時 00分&nbsp;</td></tr></tbody></table>
"#;

        let subtitles = parse_subtitles(setting, toc_source, &HashMap::new()).unwrap();

        assert_eq!(subtitles.len(), 2);
        assert_eq!(subtitles[0].chapter, "ゼンヒ巡査部長編");
        assert_eq!(subtitles[0].index, "313728");
        assert_eq!(subtitles[0].subtitle, "プロローグ:新任巡査部長、扇皇 ゼンヒ");
        assert_eq!(
            subtitles[0].href,
            "/stories/view/313728/novel_id~31149".to_string()
        );
        assert_eq!(
            subtitles[0].subupdate.as_deref(),
            Some("2025年 10月 24日 07時 00分")
        );
        assert_eq!(subtitles[1].chapter, "");
        assert_eq!(subtitles[1].index, "313729");
    }
}

pub fn create_short_story_subtitles(
    setting: &SiteSetting,
    toc_source: &str,
    info: &NovelInfo,
) -> Result<Vec<SubtitleInfo>> {
    let title = info
        .title
        .clone()
        .or_else(|| setting.resolve_info_pattern("t", toc_source))
        .unwrap_or_else(|| "短編".to_string());
    let subdate = info.raw_captures.get("gf").cloned().unwrap_or_default();
    let subupdate = info
        .raw_captures
        .get("nu")
        .cloned()
        .or_else(|| info.raw_captures.get("gl").cloned())
        .or_else(|| info.raw_captures.get("gf").cloned());

    Ok(vec![SubtitleInfo {
        index: "1".to_string(),
        href: String::new(),
        chapter: String::new(),
        subchapter: String::new(),
        subtitle: title,
        file_subtitle: match load_length_limit("filename-length-limit", Some(50)) {
            Some(limit) => {
                let reserved = "1".chars().count() + 1;
                sanitize_filename_with_limit("短編", Some(limit.saturating_sub(reserved)))
            }
            None => sanitize_filename("短編"),
        },
        subdate,
        subupdate,
        download_time: None,
    }])
}
