#[cfg(feature = "native-runtime")]
use chrono::{DateTime, Utc};

use crate::error::{NarouError, Result};
use crate::platform::{HttpClient, HttpRequest, RateLimitScope, RateLimiter};
#[cfg(feature = "native-runtime")]
use crate::platform::NovelRepository;

use super::http_policy::{ensure_success_response, host_of};
#[cfg(feature = "native-runtime")]
use super::rate_limit::RateLimiter as NativeRateLimiter;
use super::security::validate_public_url;

const NAROU_API_DEFAULT_USER_AGENT: &str = "Narou RS";
const NAROU_API_DEFAULT_INTERVAL_SECS: f64 = 1.0;

/// User-Agent used for なろうAPI requests.
/// Configured via the `download.narou-api.user-agent` local setting.
/// Worker builds use the default (no local settings on the platform).
pub fn narou_api_user_agent() -> String {
    #[cfg(feature = "native-runtime")]
    {
        crate::compat::load_local_setting_string("download.narou-api.user-agent")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| NAROU_API_DEFAULT_USER_AGENT.to_string())
    }
    #[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
    {
        NAROU_API_DEFAULT_USER_AGENT.to_string()
    }
}

/// Minimum wait time (seconds) between なろうAPI requests.
/// Configured via the `download.narou-api.interval` local setting.
/// Worker builds use the default (no local settings on the platform).
pub fn narou_api_interval_secs() -> f64 {
    #[cfg(feature = "native-runtime")]
    {
        crate::compat::load_local_setting_value("download.narou-api.interval")
            .and_then(|value| match value {
                serde_yaml::Value::Number(number) => number.as_f64(),
                serde_yaml::Value::String(raw) => raw.parse::<f64>().ok(),
                _ => None,
            })
            .unwrap_or(NAROU_API_DEFAULT_INTERVAL_SECS)
            .max(0.0)
    }
    #[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
    {
        NAROU_API_DEFAULT_INTERVAL_SECS
    }
}

/// Rate limiter dedicated to なろうAPI calls. Independent of the
/// `download.interval` / `download.wait-steps` settings used by per-episode
/// downloads. Native-only: the concrete limiter is the native rate-limit
/// implementation; Worker builds schedule batch updates with their own
/// injected platform `RateLimiter`.
#[cfg(feature = "native-runtime")]
pub fn narou_api_rate_limiter() -> NativeRateLimiter {
    NativeRateLimiter::with_settings(narou_api_interval_secs(), 0)
}

/// Fetch one なろうAPI JSON response: rate-limit with the dedicated API
/// limiter, send with the API user-agent, and map non-success statuses to
/// errors. The body is decoded as UTF-8 (the API always returns JSON).
pub async fn fetch_narou_api_json(
    http: &dyn HttpClient,
    rate_limiter: &dyn RateLimiter,
    url: &str,
    user_agent: &str,
) -> Result<String> {
    validate_public_url(url).map_err(|e| NarouError::Http(e.to_string()))?;
    rate_limiter
        .acquire(&RateLimitScope::site(host_of(url)))
        .await?;
    let request = HttpRequest::get(url).with_header("User-Agent", user_agent);
    let response = http.send(request).await?;
    let response = ensure_success_response(url, response)?;
    Ok(String::from_utf8_lossy(&response.body).into_owned())
}

/// Parse a date/time string from the Syosetu API.
/// The API returns dates as `"YYYY-MM-DD HH:MM:SS"` (not RFC 3339).
#[cfg(feature = "native-runtime")]
fn parse_api_datetime(value: &str) -> Option<DateTime<Utc>> {
    super::parse_datetime_with_timezone(value, Some("Asia/Tokyo"))
}

/// Parse Syosetu API JSON response.
/// The API returns a flat array: `[{"allcount":N}, {entry1}, {entry2}, ...]`.
#[cfg(feature = "native-runtime")]
fn parse_api_entries(body: &str) -> Vec<serde_json::Value> {
    let arr: Vec<serde_json::Value> = match serde_json::from_str(body) {
        Ok(a) => a,
        Err(_) => return Vec::new(),
    };
    // Skip first element (allcount metadata), return data entries that have ncode
    arr.into_iter()
        .skip(1)
        .filter(|v| v.get("ncode").is_some())
        .collect()
}

/// Batch-refresh なろうAPI metadata for all なろう records.
///
/// Native-only orchestration: it uses the native dedicated API rate limiter
/// (`download.narou-api.*` settings). Worker builds schedule their own batch
/// jobs with an injected platform `RateLimiter`; the portable request/parse
/// helpers (`fetch_narou_api_json` and friends) remain available to both.
#[cfg(feature = "native-runtime")]
pub async fn narou_api_batch_update(
    http: &dyn HttpClient,
    novels: &dyn NovelRepository,
) -> Result<(usize, usize)> {
    // なろう対象の一覧を keyset scan で取得する (D1: `WHERE is_narou = 1
    // AND ncode IS NOT NULL ORDER BY id LIMIT ?`)。`ncode` のないレコードは
    // スキップしてからバッチ処理する。
    let mut narou_ids: Vec<(i64, String)> = Vec::new();
    let filter = crate::platform::NovelFilter {
        is_narou: Some(true),
        ..Default::default()
    };
    const SCAN_PAGE: usize = 500;
    let mut after_id = None;
    loop {
        let page = novels.scan_ids(&filter, after_id, SCAN_PAGE).await?;
        if page.is_empty() {
            break;
        }
        for id in &page {
            if let Some(record) = novels.get(*id).await? {
                if let Some(ncode) = record.ncode {
                    narou_ids.push((record.id, ncode));
                }
            }
        }
        after_id = page.last().copied();
        if page.len() < SCAN_PAGE {
            break;
        }
    }

    if narou_ids.is_empty() {
        return Ok((0, 0));
    }

    let api_url = "https://api.syosetu.com/novelapi/api/";
    let api_user_agent = narou_api_user_agent();
    let api_rate_limiter = narou_api_rate_limiter();
    let mut total_updated = 0usize;
    let mut total_failed = 0usize;

    // Ruby prepends `n-` to the of parameter (api.rb:38).
    // API field abbreviations: n=ncode, t=title, w=writer, s=story,
    // nt=novel_type, e=end, ga=general_all_no, gf=general_firstup,
    // gl=general_lastup, nu=novelupdated_at, l=length
    for chunk in narou_ids.chunks(50) {
        let ncodes: Vec<&str> = chunk.iter().map(|(_, nc)| nc.as_str()).collect();
        let ncode_param = ncodes.join("-");

        let url = format!(
            "{}?of=n-t-nt-ga-gf-nu-gl-l-w-s-e&out=json&ncode={}",
            api_url, ncode_param
        );

        let body = match fetch_narou_api_json(http, &api_rate_limiter, &url, &api_user_agent).await
        {
            Ok(b) => b,
            Err(_) => {
                total_failed += chunk.len();
                continue;
            }
        };

        let entries = parse_api_entries(&body);

        for entry in &entries {
            let entry_ncode = entry
                .get("ncode")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if let Some(id) = chunk
                .iter()
                .find(|(_, nc)| nc.eq_ignore_ascii_case(entry_ncode))
                .map(|(id, _)| *id)
            {
                let updated = match novels.get(id.into()).await? {
                    Some(record) => {
                        let mut r = record;

                        if let Some(s) = entry.get("title").and_then(|v| v.as_str()) {
                            r.title = s.to_string();
                        }
                        if let Some(s) = entry.get("writer").and_then(|v| v.as_str()) {
                            r.author = s.to_string();
                        }
                        if let Some(n) = entry.get("end").and_then(|v| v.as_i64()) {
                            r.end = n == 1;
                        }
                        if let Some(n) = entry.get("general_all_no").and_then(|v| v.as_i64()) {
                            r.general_all_no = Some(n);
                        }
                        if let Some(n) = entry.get("length").and_then(|v| v.as_i64()) {
                            r.length = Some(n);
                        }

                        if let Some(s) = entry.get("general_firstup").and_then(|v| v.as_str()) {
                            r.general_firstup = parse_api_datetime(s);
                        }
                        if let Some(s) = entry.get("general_lastup").and_then(|v| v.as_str()) {
                            r.general_lastup = parse_api_datetime(s);
                        }
                        if let Some(s) = entry.get("novelupdated_at").and_then(|v| v.as_str()) {
                            r.novelupdated_at = parse_api_datetime(s);
                        }

                        if let Some(nt) = entry.get("novel_type").and_then(|v| v.as_i64()) {
                            r.novel_type = if nt == 2 { 2 } else { 1 };
                        }

                        novels
                            .apply_batch(vec![crate::platform::NovelMutation::Upsert(r)])
                            .await?;
                        true
                    }
                    None => false,
                };

                if updated {
                    total_updated += 1;
                }
            }
        }
    }

    Ok((total_updated, total_failed))
}
