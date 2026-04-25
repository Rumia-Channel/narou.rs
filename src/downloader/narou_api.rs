use chrono::{DateTime, Utc};

use crate::error::Result;

use super::fetch::HttpFetcher;

/// Parse a date/time string from the Syosetu API.
/// The API returns dates as `"YYYY-MM-DD HH:MM:SS"` (not RFC 3339).
fn parse_api_datetime(value: &str) -> Option<DateTime<Utc>> {
    super::parse_datetime_with_timezone(value, Some("Asia/Tokyo"))
}

/// Parse Syosetu API JSON response.
/// The API returns a flat array: `[{"allcount":N}, {entry1}, {entry2}, ...]`.
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

pub fn narou_api_batch_update(fetcher: &mut HttpFetcher) -> Result<(usize, usize)> {
    let narou_ids: Vec<(i64, String)> = crate::db::with_database(|db| {
        Ok(db
            .all_records()
            .values()
            .filter(|r| r.is_narou && r.ncode.is_some())
            .filter_map(|r| r.ncode.as_ref().map(|nc| (r.id, nc.clone())))
            .collect())
    })
    .unwrap_or_default();

    if narou_ids.is_empty() {
        return Ok((0, 0));
    }

    let api_url = "https://api.syosetu.com/novelapi/api/";
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

        fetcher.rate_limiter.wait_for_url(&url);
        let response = match fetcher.client.get(&url).send() {
            Ok(r) => r,
            Err(_e) => {
                total_failed += chunk.len();
                continue;
            }
        };

        if !response.status().is_success() {
            total_failed += chunk.len();
            continue;
        }

        let body = match response.text() {
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
                let updated = crate::db::with_database_mut(|db| {
                    if let Some(record) = db.get(id).cloned() {
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

                        db.insert(r);
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                })
                .unwrap_or(false);

                if updated {
                    total_updated += 1;
                }
            }
        }
    }

    let _ = crate::db::with_database_mut(|db| db.save());
    Ok((total_updated, total_failed))
}
