use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Utc};

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
    } else if let Some(host_and_path) = href.strip_prefix("//") {
        // スキーム相対 (`//host/path`) はページと同じスキームで解決する。
        // なろうの挿絵 (mitemin) がこの形で、`/` 始まりとして扱うと
        // `<top_url>//host/...` という存在しない URL になっていた。
        format!("{}://{}", url_scheme(&setting.top_url()), host_and_path)
    } else if href.starts_with('/') {
        format!("{}{}", setting.top_url(), href)
    } else if href.is_empty() {
        toc_url.to_string()
    } else {
        format!("{}/{}", toc_url.trim_end_matches('/'), href)
    }
}

/// URL のスキーム部分 (`https` など)。解釈できなければ `https`。
fn url_scheme(url: &str) -> &str {
    url.split_once("://")
        .map(|(scheme, _)| scheme)
        .filter(|scheme| !scheme.is_empty())
        .unwrap_or("https")
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
    if let Some(setting) = setting
        && let Some(pipeline) = setting.preprocess_pipeline()
    {
        let jobs = preprocess::PreprocessJobs::new();
        let run = preprocess::run_preprocess(pipeline, src, &jobs, "")
            .unwrap_or_else(|error| panic!("{error}"));
        if !run.requested.is_empty() {
            tracing::warn!(
                "preprocess for {} requested {} URL(s) that this path cannot fetch",
                setting.name,
                run.requested.len()
            );
        }
    }
}

/// One pretreatment run: the fetched body to rewrite plus the platform
/// handles needed when the `preprocess:` definition asks for more URLs.
pub struct PretreatmentRequest<'a> {
    pub http: &'a dyn HttpClient,
    pub rate_limiter: &'a dyn RateLimiter,
    pub policy: &'a crate::downloader::http_policy::FetchPolicy,
    pub src: &'a mut String,
    pub encoding: &'a str,
    pub setting: Option<&'a SiteSetting>,
    pub jobs: &'a mut preprocess::PreprocessJobs,
    pub url: &'a str,
}

/// Rewrite a fetched body in place, executing any URLs the definition asks for.
///
/// The definition is re-run until it stops asking for new URLs (bounded by
/// [`MAX_PREPROCESS_ROUNDS`]); each round starts from the original body, so the
/// result depends only on the body and the settled job results. Jobs that have
/// finished are visible immediately through `jobs`, which callers keep for the
/// whole novel so repeated references resolve once.
pub async fn pretreatment_source_with_jobs(request: PretreatmentRequest<'_>) -> Result<()> {
    let PretreatmentRequest {
        http,
        rate_limiter,
        policy,
        src,
        encoding,
        setting,
        jobs,
        url,
    } = request;
    src.retain(|c| c != '\r');
    decode_numeric_entities(src);
    let Some(setting) = setting else {
        return Ok(());
    };
    let Some(pipeline) = setting.preprocess_pipeline() else {
        return Ok(());
    };
    jobs.bind_policy(policy, url);

    let original = src.clone();
    jobs.clear_forms();
    let result = async {
        let mut rounds = 0;
        loop {
            let mut working = original.clone();
            let run = preprocess::run_preprocess(pipeline, &mut working, jobs, url)
                .map_err(|error| match error {
                    preprocess::PreprocessError::Definition(message) => crate::error::NarouError::SiteSetting(message),
                    preprocess::PreprocessError::RequiredJson(url) => required_json_error(
                        jobs.failure(&url).cloned().map(crate::error::NarouError::Shared)
                            .unwrap_or_else(|| crate::error::NarouError::Http(
                                format!("required JSON response is null while fetching {url}")
                            )),
                    ),
                })?;
            let pending: Vec<_> = run.requested.into_iter()
                .filter(|request| !jobs.contains(request.key())).collect();
            if pending.is_empty() || rounds >= MAX_PREPROCESS_ROUNDS {
                if pending.iter().any(|request| matches!(request, preprocess::PreprocessRequest::Form { .. })) {
                    return Err(crate::error::NarouError::SiteSetting("preprocess: unresolved form request".into()));
                }
                if pending.iter().any(|request| matches!(request, preprocess::PreprocessRequest::JsonRequired { .. })) {
                    return Err(crate::error::NarouError::SiteSetting("preprocess: unresolved required JSON request".into()));
                }
                if !pending.is_empty() {
                    tracing::warn!(
                        "preprocess for {} stopped after {} rounds with {} URL(s) unresolved",
                        setting.name, MAX_PREPROCESS_ROUNDS, pending.len()
                    );
                }
                *src = working;
                return Ok(());
            }
            for request in pending {
                match request {
                    preprocess::PreprocessRequest::Json { url } => {
                        resolve_json_job(http, rate_limiter, policy, jobs, url, None).await?;
                    }
                    preprocess::PreprocessRequest::JsonRequired { url, missing_statuses } => {
                        resolve_json_job(http, rate_limiter, policy, jobs, url, Some(missing_statuses)).await?;
                    }
                    preprocess::PreprocessRequest::Form { url, body, headers, key } => {
                        let text = crate::downloader::http_policy::fetch_form_text(
                            http, rate_limiter, &url, body, headers, policy, Some(encoding),
                        ).await?;
                        jobs.insert_form(key, serde_json::Value::String(text))
                            .map_err(crate::error::NarouError::SiteSetting)?;
                    }
                }
            }
            rounds += 1;
        }
    }.await;
    jobs.clear_forms();
    result
}

fn required_json_error(error: crate::error::NarouError) -> crate::error::NarouError {
    let mut original = &error;
    while let crate::error::NarouError::Shared(shared) = original {
        original = shared.as_ref();
    }
    match original {
        // A failed dependency is not evidence that the whole novel disappeared.
        crate::error::NarouError::NotFound(url) => crate::error::NarouError::Http(
            format!("HTTP 404 while fetching required JSON dependency {url}")
        ),
        _ => error,
    }
}

async fn resolve_json_job(
    http: &dyn crate::platform::HttpClient,
    rate_limiter: &dyn crate::platform::RateLimiter,
    policy: &crate::downloader::http_policy::FetchPolicy,
    jobs: &mut preprocess::PreprocessJobs,
    url: String,
    missing_statuses: Option<Vec<u16>>,
) -> Result<()> {
    let response = crate::downloader::http_policy::fetch_json_response(
        http, rate_limiter, &url, policy,
    ).await;
    let (status, value) = match response {
        Ok(response) => {
            let status = response.status;
            let value = crate::downloader::http_policy::ensure_success_response(&url, response)
                .and_then(|response| {
                    let source = String::from_utf8(response.body).unwrap_or_else(|error|
                        String::from_utf8_lossy(error.as_bytes()).into_owned()
                    );
                    serde_json::from_str::<serde_json::Value>(&source)
                        .map(|value| (value, source))
                        .map_err(|error| crate::error::NarouError::Http(
                            format!("invalid JSON while fetching {url}: {error}")
                        ))
                });
            (Some(status), value)
        }
        Err(error) => (None, Err(error)),
    };
    match value {
        Ok((value, source)) => {
            if missing_statuses.is_some() && value.is_null() {
                return Err(crate::error::NarouError::Http(
                    format!("required JSON response is null while fetching {url}")
                ));
            }
            jobs.insert_json(url, value, source);
        }
        Err(error) => {
            let admitted = status.is_some_and(|status| missing_statuses.as_ref()
                .is_some_and(|statuses| statuses.contains(&status)));
            if missing_statuses.is_some() && !admitted {
                return Err(required_json_error(error));
            }
            tracing::debug!("preprocess request failed for {url}: {error}");
            jobs.insert_failure(url, status, error);
        }
    }
    Ok(())
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

// ------------------------------------------------------------
// 日付の解析 (2 桁年)
// ------------------------------------------------------------

/// 2 桁年 (`24/5/29 7:36`) を読むときの書式。
///
/// Ruby の `Time.parse` と同じ規則 (00-68 → 2000-2068、69-99 → 1969-1999) に
/// 補正する。chrono の `%y` は 69 を 2069 とするため、そのまま使わない。
const TWO_DIGIT_YEAR_FORMATS: [&str; 2] = [
    "%y/%m/%d %H:%M:%S",
    "%y/%m/%d %H:%M",
];

/// 2 桁年だけを持つ日付文字列を、サイトのタイムゾーンで解釈する。
///
/// 2 桁年の無い日付 (`2024/5/29 7:36`、`2024-05-29 07:36` など) は `None` を
/// 返す — 4 桁年の書式 (`%Y`) が担当する。`%Y` は 2 桁の年も西暦 24 年として
/// 読んでしまうため、2 桁年はここで先に確定する。
///
/// 文字列を書き換えない (割り当てなし) ので、既存の書式を壊さない。
pub(crate) fn parse_two_digit_year_datetime(
    value: &str,
    timezone: super::SiteTimezone,
) -> Option<DateTime<Utc>> {
    let bytes = value.as_bytes();
    if bytes.len() < 3 || bytes[2] != b'/' || !bytes[..2].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let short_year = i32::from((bytes[0] - b'0') * 10 + bytes[1] - b'0');
    let year = if short_year >= 69 { 1900 } else { 2000 } + short_year;
    let datetime = if value.contains(':') {
        TWO_DIGIT_YEAR_FORMATS.iter()
            .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())?
            .with_year(year)?
    } else {
        NaiveDate::parse_from_str(value, "%y/%m/%d").ok()?
            .with_year(year)?
            .and_hms_opt(0, 0, 0)?
    };
    timezone.resolve_local_datetime(datetime)
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

/// narou.rb / narou-mod の `Helper#replace_filename_special_chars` と同じ
/// 文字置換。Windows でファイル名に使えない ASCII 文字を全角文字へ写像し、
/// タブと改行を除去する。`normalize-filename` が有効なら NFC もかける
/// (Worker ビルドにはローカル設定が無いため省略する)。
///
/// 既存ディレクトリの `‛` (U+201B) 形式は `db::paths` が読み取り時に
/// probe するだけなので、ここは常に正規の spelling を返す。
pub fn ruby_filename_special_chars(text: &str) -> String {
    #[allow(unused_mut)]
    let mut result = text
        .chars()
        .filter_map(|ch| match ch {
            '\t' | '\n' => None,
            '/' => Some('／'),
            ':' => Some('：'),
            '*' => Some('＊'),
            '?' => Some('？'),
            '"' => Some('”'),
            '<' => Some('〈'),
            '>' => Some('〉'),
            '[' => Some('［'),
            ']' => Some('］'),
            '{' => Some('｛'),
            '}' => Some('｝'),
            '|' => Some('｜'),
            '.' => Some('．'),
            '`' => Some('｀'),
            '\\' => Some('￥'),
            _ => Some(ch),
        })
        .collect::<String>();
    #[cfg(feature = "native-runtime")]
    {
        if crate::compat::load_local_setting_bool("normalize-filename") {
            use unicode_normalization::UnicodeNormalization;
            result = result.nfc().collect();
        }
    }
    result
}

pub fn mask_spoiler_text(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '0'..='9' | '０'..='９' | ' ' | '　' | '、' | '。' | '!' | '?' | '！' | '？' => {
                ch
            }
            _ => '●',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        build_section_url, mask_spoiler_text, parse_two_digit_year_datetime,
        ruby_filename_special_chars, sanitize_filename_with_limit,
    };
    use crate::downloader::site_setting::SiteSetting;

    fn tokyo() -> super::super::SiteTimezone {
        super::super::SiteTimezone::Named(chrono_tz::Asia::Tokyo)
    }

    fn local(value: &str) -> String {
        let parsed = parse_two_digit_year_datetime(value, tokyo())
            .unwrap_or_else(|| panic!("{value} must parse as a two-digit-year date"));
        parsed
            .with_timezone(&chrono_tz::Asia::Tokyo)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    }

    #[test]
    fn ruby_filename_special_chars_matches_narou_rb() {
        assert_eq!(
            ruby_filename_special_chars("a/b\\c:d*e?f\"g<h>i[j]k{l}m|n.o`p\tq\nr"),
            "a／b￥c：d＊e？f”g〈h〉i［j］k｛l｝m｜n．o｀pqr"
        );
        // 実体参照そのものは写像対象外。デコード後の `<` / `>` が `〈` / `〉` になる。
        assert_eq!(ruby_filename_special_chars("&lt;C201&gt;"), "&lt;C201&gt;");
        assert_eq!(ruby_filename_special_chars("<C201>"), "〈C201〉");
    }

    #[test]
    fn two_digit_year_dates_use_the_posix_and_ruby_rule() {
        // ノベルアップ＋の目次日 (`24/5/29 7:36`)。
        assert_eq!(local("24/5/29 7:36"), "2024-05-29 07:36");
        assert_eq!(local("26/11/16 8:15"), "2026-11-16 08:15");
        assert_eq!(local("24/5/29"), "2024-05-29 00:00");
        assert_eq!(local("24/5/29 7:36:05"), "2024-05-29 07:36");
    }

    #[test]
    fn two_digit_year_boundaries_follow_ruby() {
        // Ruby の Time.parse と同じ振り分け (00-68 → 2000年代, 69-99 → 1900年代)。
        assert_eq!(local("00/1/1"), "2000-01-01 00:00");
        assert_eq!(local("68/12/31"), "2068-12-31 00:00");
        assert_eq!(local("69/1/1"), "1969-01-01 00:00");
        assert_eq!(local("69/1/1 12:34"), "1969-01-01 12:34");
        assert_eq!(local("99/12/31"), "1999-12-31 00:00");
    }

    #[test]
    fn four_digit_year_dates_are_left_to_the_regular_formats() {
        // 4 桁の年は `%y` に合致しないので、ここで読んでも既存の解釈を壊さない。
        assert!(parse_two_digit_year_datetime("2024/5/29 7:36", tokyo()).is_none());
        assert!(parse_two_digit_year_datetime("2024-05-29 07:36", tokyo()).is_none());
        assert!(parse_two_digit_year_datetime("2024/5/29", tokyo()).is_none());
    }

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
    fn build_section_url_resolves_protocol_relative_hrefs_with_the_site_scheme() {
        let settings = SiteSetting::load_all().unwrap();
        let narou = settings
            .iter()
            .find(|s| s.domain == "ncode.syosetu.com")
            .unwrap();
        assert_eq!(
            build_section_url(
                narou,
                "https://ncode.syosetu.com/n9364bq/12/",
                "//9496.mitemin.net/userpageimage/viewimagebig/icode/i88657/"
            ),
            "https://9496.mitemin.net/userpageimage/viewimagebig/icode/i88657/"
        );

        // スキームはサイト定義の top_url に従う (Arcadia は http)。
        let arcadia = settings.iter().find(|s| s.name == "Arcadia").unwrap();
        assert_eq!(
            build_section_url(
                arcadia,
                "http://www.mai-net.net/bbs/sst/sst.php",
                "//example.com/x"
            ),
            "http://example.com/x"
        );
    }

    #[test]
    fn sanitize_filename_with_limit_truncates_after_sanitizing() {
        assert_eq!(sanitize_filename_with_limit("ab/cd", Some(4)), "ab_c");
    }

    #[test]
    fn sanitize_filename_with_limit_rejects_reserved_and_control_names() {
        assert_eq!(sanitize_filename_with_limit("CON.txt", None), "_CON.txt");
        assert_eq!(
            sanitize_filename_with_limit("bad\0name\x7F", None),
            "badname"
        );
        assert_eq!(sanitize_filename_with_limit("trail. ", None), "trail");
    }

    #[test]
    fn mask_spoiler_text_preserves_digits_and_punctuation() {
        assert_eq!(mask_spoiler_text("第12話!? テスト"), "●12●!? ●●●");
    }
}
