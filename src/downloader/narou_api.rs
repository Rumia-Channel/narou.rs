use std::collections::HashSet;

use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::platform::{HttpClient, HttpRequest, RateLimitScope, RateLimiter};

use super::http_policy::{ensure_success_response, host_of};
#[cfg(feature = "native-runtime")]
use super::rate_limit::RateLimiter as NativeRateLimiter;
use super::types::NarouApiEntry;

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
    http.validate_url(url).await?;
    let scope = RateLimitScope::site(host_of(url));
    let request = HttpRequest::get(url).with_header("User-Agent", user_agent);
    let response = http.send_paced(
        request, crate::platform::http::HttpPacing { limiter: rate_limiter, scope: &scope },
    ).await?;
    let response = ensure_success_response(url, response)?;
    Ok(String::from_utf8_lossy(&response.body).into_owned())
}

/// Parse a Syosetu API JSON response into entries.
///
/// The API returns either a flat array whose first element is the `allcount`
/// metadata (`[{"allcount":N}, {entry}, ...]`) or an object with a `data`
/// array. Entries without an `ncode` are skipped.
pub fn parse_narou_api_entries(body: &str) -> Vec<NarouApiEntry> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };

    match value {
        serde_json::Value::Array(values) => values
            .into_iter()
            .filter(|v| v.get("ncode").is_some())
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect(),
        serde_json::Value::Object(mut map) => {
            if let Some(data) = map.remove("data") {
                serde_json::from_value(data).unwrap_or_default()
            } else if map.get("ncode").is_some() {
                serde_json::from_value(serde_json::Value::Object(map))
                    .ok()
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// Parse a date/time string from the Syosetu API.
/// The API returns dates as `"YYYY-MM-DD HH:MM:SS"` (not RFC 3339), in JST.
pub fn parse_api_datetime(value: &str) -> Option<DateTime<Utc>> {
    super::parse_datetime_with_timezone(value, Some("Asia/Tokyo"))
}

/// Match the API's entries to the requested `(id, ncode)` chunk.
///
/// Returns the `(id, entry)` pairs the API answered for (case-insensitive
/// ncode match, first match wins) and the ids the API did not answer for, so
/// the caller can fall back to fetching those novels directly. Fallback ids
/// are sorted and deduplicated.
pub fn classify_narou_api_chunk(
    chunk: &[(i64, String)],
    entries: &[NarouApiEntry],
) -> (Vec<(i64, NarouApiEntry)>, Vec<i64>) {
    let mut updates = Vec::new();
    let mut fallback_ids = Vec::new();
    let mut seen_ncodes = HashSet::new();

    for entry in entries {
        let Some((id, _)) = chunk
            .iter()
            .find(|(_, ncode)| ncode.eq_ignore_ascii_case(&entry.ncode))
        else {
            continue;
        };

        seen_ncodes.insert(entry.ncode.to_ascii_lowercase());
        updates.push((*id, entry.clone()));
    }

    for (id, ncode) in chunk {
        if !seen_ncodes.contains(&ncode.to_ascii_lowercase()) {
            fallback_ids.push(*id);
        }
    }

    fallback_ids.sort_unstable();
    fallback_ids.dedup();
    (updates, fallback_ids)
}
