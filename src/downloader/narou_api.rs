use chrono::Utc;

use crate::error::Result;

use super::fetch::HttpFetcher;
use super::types::NarouApiResult;

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

    let mut all_ncodes = Vec::new();
    for chunk in narou_ids.chunks(50) {
        let ncodes: Vec<&str> = chunk.iter().map(|(_, nc)| nc.as_str()).collect();
        all_ncodes.push(ncodes.join("-"));
    }

    let api_url = "https://api.syosetu.com/novelapi/api/";
    let mut total_updated = 0usize;
    let mut total_failed = 0usize;

    for ncode_chunk in &all_ncodes {
        fetcher.rate_limiter.wait();
        let url = format!(
            "{}?of=t-nt-ga-gf-nu-gl-l-w-s-e-ncode-allno-novelpage&out=json&ncode={}",
            api_url, ncode_chunk
        );

        let response = match fetcher.client.get(&url).send() {
            Ok(r) => r,
            Err(_e) => {
                total_failed += 50;
                continue;
            }
        };

        if !response.status().is_success() {
            total_failed += 50;
            continue;
        }

        let body = match response.text() {
            Ok(b) => b,
            Err(_) => {
                total_failed += 50;
                continue;
            }
        };

        let api_result: NarouApiResult = match serde_json::from_str(&body) {
            Ok(r) => r,
            Err(_) => {
                total_failed += 50;
                continue;
            }
        };

        for entry in &api_result.data {
            if let Some(id) = narou_ids
                .iter()
                .find(|(_, nc)| nc == &entry.ncode)
                .map(|(id, _)| *id)
            {
                let updated = crate::db::with_database_mut(|db| {
                    if let Some(record) = db.get(id).cloned() {
                        let mut r = record;
                        r.title = entry.title.clone();
                        r.author = entry.writer.clone();
                        r.end = entry.end == 1;
                        r.general_all_no = Some(entry.general_all_no);
                        r.length = Some(entry.length);

                        if let Ok(dt) =
                            chrono::DateTime::parse_from_rfc3339(&entry.general_firstup)
                        {
                            r.general_firstup = Some(dt.with_timezone(&Utc));
                        }
                        if let Ok(dt) =
                            chrono::DateTime::parse_from_rfc3339(&entry.general_lastup)
                        {
                            r.general_lastup = Some(dt.with_timezone(&Utc));
                        }
                        if let Ok(dt) =
                            chrono::DateTime::parse_from_rfc3339(&entry.novelupdated_at)
                        {
                            r.novelupdated_at = Some(dt.with_timezone(&Utc));
                        }

                        if entry.novel_type == 2 {
                            r.novel_type = 2;
                        } else {
                            r.novel_type = 1;
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
