use crate::error::Result;
use crate::platform::{HttpClient, RateLimiter};

use super::preprocess;
use super::security::MAX_YAML_REGEX_PATTERN_LEN;
use super::site_setting::SiteSetting;

pub const DEFAULT_REGEX_SIZE_LIMIT: usize = 1_000_000;

/// How many times one body may be re-processed while the definition keeps
/// asking for new URLs. Each round resolves one dependency level.
pub const MAX_PREPROCESS_ROUNDS: usize = 4;

pub fn build_section_url(setting: &SiteSetting, toc_url: &str, href: &str) -> String {
    let href = decode_html_href(href);
    if href.starts_with("http://") || href.starts_with("https://") {
        href
    } else if href.starts_with('/') {
        format!("{}{}", setting.top_url(), href)
    } else if href.is_empty() {
        toc_url.to_string()
    } else {
        format!("{}/{}", toc_url.trim_end_matches('/'), href)
    }
}

pub fn decode_html_text(text: &str) -> String {
    let mut decoded = text
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&copy;", "(c)");
    decode_numeric_entities(&mut decoded);
    decoded.replace('\u{00A0}', " ")
}

fn decode_html_href(href: &str) -> String {
    decode_html_text(href.split('#').next().unwrap_or(""))
}

pub fn compile_html_pattern(pattern: &str) -> std::result::Result<regex::Regex, regex::Error> {
    if pattern.len() > MAX_YAML_REGEX_PATTERN_LEN {
        return Err(regex::Error::Syntax(format!(
            "YAML regex pattern exceeds {} bytes",
            MAX_YAML_REGEX_PATTERN_LEN
        )));
    }

    regex::RegexBuilder::new(pattern)
        .dot_matches_new_line(true)
        .size_limit(DEFAULT_REGEX_SIZE_LIMIT)
        .build()
}

/// Rewrite a fetched body in place with the site's `preprocess:` definition.
///
/// Definitions that never call `request()` finish in one pass. Definitions
/// that do need [`pretreatment_source_with_jobs`], which can execute the
/// requested URLs; this entry point logs the requests it had to drop.
pub fn pretreatment_source(src: &mut String, _encoding: &str, setting: Option<&SiteSetting>) {
    src.retain(|c| c != '\r');
    decode_numeric_entities(src);
    if let Some(setting) = setting {
        if let Some(pipeline) = setting.preprocess_pipeline() {
            let jobs = preprocess::PreprocessJobs::new();
            let run = preprocess::run_preprocess(pipeline, src, &jobs, "");
            if !run.requested.is_empty() {
                tracing::warn!(
                    "preprocess for {} requested {} URL(s) that this path cannot fetch",
                    setting.name,
                    run.requested.len()
                );
            }
        }
    }
}

/// Rewrite a fetched body in place, executing any URLs the definition asks for.
///
/// The definition is re-run until it stops asking for new URLs (bounded by
/// [`MAX_PREPROCESS_ROUNDS`]); each round starts from the original body, so the
/// result depends only on the body and the settled job results. Jobs that have
/// finished are visible immediately through `jobs`, which callers keep for the
/// whole novel so repeated references resolve once.
pub async fn pretreatment_source_with_jobs(
    http: &dyn HttpClient,
    rate_limiter: &dyn RateLimiter,
    policy: &crate::downloader::http_policy::FetchPolicy,
    src: &mut String,
    _encoding: &str,
    setting: Option<&SiteSetting>,
    jobs: &mut preprocess::PreprocessJobs,
    url: &str,
) -> Result<()> {
    src.retain(|c| c != '\r');
    decode_numeric_entities(src);
    let Some(setting) = setting else {
        return Ok(());
    };
    let Some(pipeline) = setting.preprocess_pipeline() else {
        return Ok(());
    };

    let original = src.clone();
    let mut rounds = 0;
    loop {
        let mut working = original.clone();
        let run = preprocess::run_preprocess(pipeline, &mut working, jobs, url);
        let pending: Vec<String> = run
            .requested
            .into_iter()
            .filter(|url| !jobs.contains(url))
            .collect();
        if pending.is_empty() || rounds >= MAX_PREPROCESS_ROUNDS {
            if !pending.is_empty() {
                tracing::warn!(
                    "preprocess for {} stopped after {} rounds with {} URL(s) unresolved",
                    setting.name,
                    MAX_PREPROCESS_ROUNDS,
                    pending.len()
                );
            }
            *src = working;
            return Ok(());
        }

        for url in pending {
            // A failed request settles as `null`: illustrations are optional and
            // the definition branches on the result instead of failing the
            // whole download.
            let value = match crate::downloader::http_policy::fetch_text(
                http,
                rate_limiter,
                &url,
                policy,
                Some("UTF-8"),
            )
            .await
            {
                Ok(body) => serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
                Err(err) => {
                    tracing::debug!("preprocess request failed for {url}: {err}");
                    serde_json::Value::Null
                }
            };
            jobs.insert(url, value);
        }
        rounds += 1;
    }
}

pub fn decode_numeric_entities(src: &mut String) {
    if !src.contains("&#") {
        return;
    }
    static HEX_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"&#x([0-9a-fA-F]+);").unwrap());
    static DEC_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"&#(\d+);").unwrap());

    if HEX_RE.is_match(src) {
        *src = HEX_RE
            .replace_all(src, |caps: &regex::Captures| {
                let code = u32::from_str_radix(&caps[1], 16).unwrap_or(0xFFFD);
                char::from_u32(code).unwrap_or('\u{FFFD}').to_string()
            })
            .to_string();
    }

    if DEC_RE.is_match(src) {
        *src = DEC_RE
            .replace_all(src, |caps: &regex::Captures| {
                let code: u32 = caps[1].parse().unwrap_or(0xFFFD);
                char::from_u32(code).unwrap_or('\u{FFFD}').to_string()
            })
            .to_string();
    }
}

/// Length limit for generated filenames (`folder-length-limit` /
/// `filename-length-limit` local settings). Worker builds have no local
/// settings and fall back to the provided default.
pub fn load_length_limit(key: &str, default: Option<usize>) -> Option<usize> {
    #[cfg(feature = "native-runtime")]
    {
        crate::compat::load_local_setting_value(key)
            .and_then(|value| match value {
                serde_yaml::Value::Number(number) => number.as_i64(),
                serde_yaml::Value::String(raw) => raw.parse::<i64>().ok(),
                _ => None,
            })
            .map(|limit| limit.max(0) as usize)
            .or(default)
    }
    #[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
    {
        let _ = key;
        default
    }
}

pub fn sanitize_filename_with_limit(name: &str, limit: Option<usize>) -> String {
    crate::db::paths::sanitize_windows_filename_component_with_limit(name, limit, Some('_'), "_")
}

pub fn sanitize_filename(name: &str) -> String {
    sanitize_filename_with_limit(name, Some(80))
}

pub fn mask_spoiler_text(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '0'..='9' | '０'..='９' | ' ' | '　' | '、' | '。' | '!' | '?' | '！' | '？' => ch,
            _ => '●',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{build_section_url, mask_spoiler_text, sanitize_filename_with_limit};
    use crate::downloader::site_setting::SiteSetting;

    #[test]
    fn build_section_url_decodes_html_entities_in_href() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "Arcadia").unwrap();

        let url = build_section_url(
            setting,
            "http://www.mai-net.net/bbs/sst/sst.php?act=dump&cate=all&all=6858&n=0&count=1",
            "/bbs/sst/sst.php?act=dump&amp;cate=all&amp;all=6858&amp;n=0#kiji",
        );

        assert_eq!(
            url,
            "http://www.mai-net.net/bbs/sst/sst.php?act=dump&cate=all&all=6858&n=0"
        );
    }

    #[test]
    fn sanitize_filename_with_limit_truncates_after_sanitizing() {
        assert_eq!(sanitize_filename_with_limit("ab/cd", Some(4)), "ab_c");
    }

    #[test]
    fn sanitize_filename_with_limit_rejects_reserved_and_control_names() {
        assert_eq!(sanitize_filename_with_limit("CON.txt", None), "_CON.txt");
        assert_eq!(sanitize_filename_with_limit("bad\0name\x7F", None), "badname");
        assert_eq!(sanitize_filename_with_limit("trail. ", None), "trail");
    }

    #[test]
    fn mask_spoiler_text_preserves_digits_and_punctuation() {
        assert_eq!(mask_spoiler_text("第12話!? テスト"), "●12●!? ●●●");
    }
}
