use axum::{
    extract::{Form, Path, Query, State},
    http::StatusCode,
    response::{Json, Response},
};
use chrono::{DateTime, Utc};

use crate::compat::{load_frozen_ids_from_inventory, record_is_frozen};
use crate::db::{with_database, with_database_mut};
use crate::error::NarouError;

use super::AppState;
use super::state::{ApiResponse, IdPath, ListParams, NovelListItem, NovelListResponse};

const ANNOTATION_COLOR_TIME_LIMIT_SECS: i64 = 6 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchToken {
    negated: bool,
    field: Option<String>,
    values: Vec<String>,
}

fn is_new_arrivals_marker(
    new_arrivals_date: Option<DateTime<Utc>>,
    last_update: DateTime<Utc>,
    now: DateTime<Utc>,
) -> bool {
    new_arrivals_date.is_some_and(|nad| {
        let limit = chrono::Duration::seconds(ANNOTATION_COLOR_TIME_LIMIT_SECS);
        nad >= last_update && (nad + limit) >= now
    })
}

fn split_search_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;

    for ch in query.chars() {
        if ch == '"' {
            quoted = !quoted;
            current.push(ch);
            continue;
        }
        if ch.is_whitespace() && !quoted {
            if !current.trim().is_empty() {
                terms.push(current.trim().to_string());
            }
            current.clear();
            continue;
        }
        current.push(ch);
    }

    if !current.trim().is_empty() {
        terms.push(current.trim().to_string());
    }

    terms
}

fn strip_search_quotes(value: &str) -> &str {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn split_search_values(value: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut quoted = false;

    for ch in value.chars() {
        if ch == '"' {
            quoted = !quoted;
            current.push(ch);
            continue;
        }
        if ch == '|' && !quoted {
            let trimmed = strip_search_quotes(current.trim()).trim().to_lowercase();
            if !trimmed.is_empty() {
                values.push(trimmed);
            }
            current.clear();
            continue;
        }
        current.push(ch);
    }

    let trimmed = strip_search_quotes(current.trim()).trim().to_lowercase();
    if !trimmed.is_empty() {
        values.push(trimmed);
    }

    values
}

fn parse_search_token(raw_term: &str) -> SearchToken {
    let term = raw_term.trim();
    let negated = matches!(term.chars().next(), Some('-' | '^' | '!'));
    let body = if negated { &term[1..] } else { term };
    let (field, value) = if let Some(colon) = body.find(':') {
        let field = body[..colon].trim();
        if field.is_empty() {
            (None, body)
        } else {
            (Some(field.to_lowercase()), body[colon + 1..].trim())
        }
    } else {
        (None, body)
    };

    SearchToken {
        negated,
        field,
        values: split_search_values(value),
    }
}

fn collect_search_tokens(filter: Option<&str>, search_value: Option<&str>) -> Vec<SearchToken> {
    [filter, search_value]
        .into_iter()
        .flatten()
        .flat_map(split_search_terms)
        .map(|term| parse_search_token(&term))
        .filter(|token| !token.values.is_empty())
        .collect()
}

fn record_status_text(record: &crate::db::novel_record::NovelRecord, frozen: bool) -> String {
    let mut status = Vec::new();
    if frozen {
        status.push("凍結");
    }
    if record.tags.iter().any(|tag| tag == "end") || record.end {
        status.push("完結");
    }
    if record.tags.iter().any(|tag| tag == "404") {
        status.push("削除");
    }
    if record.suspend {
        status.push("中断");
    }
    status.join(", ").to_lowercase()
}

fn record_matches_token(
    record: &crate::db::novel_record::NovelRecord,
    token: &SearchToken,
    frozen: bool,
) -> bool {
    let title = record.title.to_lowercase();
    let author = record.author.to_lowercase();
    let sitename = record.sitename.to_lowercase();
    let status = record_status_text(record, frozen);
    let tags: Vec<String> = record.tags.iter().map(|tag| tag.to_lowercase()).collect();

    let matched = match token.field.as_deref() {
        Some("tag") => token
            .values
            .iter()
            .any(|value| tags.iter().any(|tag| tag.contains(value))),
        Some("author") => token.values.iter().any(|value| author.contains(value)),
        Some("site") | Some("sitename") => token.values.iter().any(|value| sitename.contains(value)),
        Some("title") => token.values.iter().any(|value| title.contains(value)),
        Some("status") => token.values.iter().any(|value| status.contains(value)),
        _ => token.values.iter().any(|value| {
            title.contains(value)
                || author.contains(value)
                || sitename.contains(value)
                || status.contains(value)
                || tags.iter().any(|tag| tag.contains(value))
        }),
    };

    if token.negated { !matched } else { matched }
}

fn record_matches_search(
    record: &crate::db::novel_record::NovelRecord,
    tokens: &[SearchToken],
    frozen: bool,
) -> bool {
    tokens
        .iter()
        .all(|token| record_matches_token(record, token, frozen))
}

pub async fn index() -> &'static str {
    "narou.rs API server"
}

pub async fn novels_count(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let count = with_database(|db| Ok(db.all_records().len() as u64)).unwrap_or(0);
    Json(serde_json::json!({ "count": count }))
}

pub async fn api_list(
    Query(params): Query<ListParams>,
    State(_state): State<AppState>,
) -> Result<Json<NovelListResponse>, (StatusCode, String)> {
    api_list_inner(params)
}

pub async fn api_list_post(
    State(_state): State<AppState>,
    Form(params): Form<ListParams>,
) -> Result<Json<NovelListResponse>, (StatusCode, String)> {
    api_list_inner(params)
}

fn api_list_inner(params: ListParams) -> Result<Json<NovelListResponse>, (StatusCode, String)> {
    let draw = params.draw.unwrap_or(1);
    let return_all = params.all.unwrap_or(false);
    let start = if return_all {
        0
    } else {
        params.start.unwrap_or(0)
    };
    let length = if return_all {
        None
    } else {
        Some(
            params
                .length
                .unwrap_or(50)
                .min(super::MAX_WEB_PAGE_LENGTH) as usize,
        )
    };
    let total_query_bytes =
        params.filter.as_ref().map_or(0, |filter| filter.len())
            + params.search_value.as_ref().map_or(0, |search| search.len());
    if total_query_bytes > super::MAX_WEB_SEARCH_BYTES {
        return Err((StatusCode::BAD_REQUEST, "search query is too long".to_string()));
    }
    let search_tokens = collect_search_tokens(params.filter.as_deref(), params.search_value.as_deref());
    let order_col = params.order_column.unwrap_or(0);
    let order_dir = params.order_dir.unwrap_or_else(|| "asc".to_string());
    let frozen_ids = with_database(|db| load_frozen_ids_from_inventory(db.inventory())).unwrap_or_default();

    let response = with_database(|db| {
        let all_records: Vec<_> = db.all_records().values().collect();

        let mut filtered: Vec<_> = if search_tokens.is_empty() {
            all_records
        } else {
            all_records
                .into_iter()
                .filter(|record| {
                    record_matches_search(record, &search_tokens, record_is_frozen(record, &frozen_ids))
                })
                .collect()
        };

        let sort_key = match order_col {
            0 => "id",
            1 => "last_update",
            2 => "general_lastup",
            3 => "last_check_date",
            4 => "title",
            5 => "author",
            6 => "sitename",
            7 => "novel_type",
            9 => "general_all_no",
            10 => "length",
            _ => "id",
        };

        let reverse = order_dir == "desc";
        filtered.sort_by(|a, b| {
            let va = match sort_key {
                "id" => a.id.cmp(&b.id),
                "title" => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
                "author" => a.author.to_lowercase().cmp(&b.author.to_lowercase()),
                "last_update" => a.last_update.cmp(&b.last_update),
                "general_lastup" => a
                    .general_lastup
                    .unwrap_or_default()
                    .cmp(&b.general_lastup.unwrap_or_default()),
                "last_check_date" => a
                    .last_check_date
                    .unwrap_or_default()
                    .cmp(&b.last_check_date.unwrap_or_default()),
                "sitename" => a.sitename.cmp(&b.sitename),
                "novel_type" => a.novel_type.cmp(&b.novel_type),
                "general_all_no" => a
                    .general_all_no
                    .unwrap_or(0)
                    .cmp(&b.general_all_no.unwrap_or(0)),
                "length" => a.length.unwrap_or(0).cmp(&b.length.unwrap_or(0)),
                _ => std::cmp::Ordering::Equal,
            };
            if reverse { va.reverse() } else { va }
        });

        let records_total = db.all_records().len() as u64;
        let records_filtered = filtered.len() as u64;

        let data: Vec<NovelListItem> = filtered
            .into_iter()
            .skip(start as usize)
            .take(length.unwrap_or(usize::MAX))
            .map(|r| {
                let now = chrono::Utc::now();
                let is_new = is_new_arrivals_marker(r.new_arrivals_date, r.last_update, now);
                NovelListItem {
                    id: r.id,
                    title: r.title.clone(),
                    author: r.author.clone(),
                    sitename: r.sitename.clone(),
                    novel_type: r.novel_type,
                    end: r.end,
                    last_update: r.last_update.timestamp(),
                    general_lastup: r.general_lastup.map(|dt| dt.timestamp()),
                    last_check_date: r.last_check_date.map(|dt| dt.timestamp()),
                    new_arrivals_date: r.new_arrivals_date.map(|dt| dt.timestamp()),
                    tags: r.tags.clone(),
                    new_arrivals: is_new,
                    frozen: record_is_frozen(r, &frozen_ids),
                    suspend: r.suspend,
                    length: r.length,
                    toc_url: r.toc_url.clone(),
                    general_all_no: r.general_all_no,
                }
            })
            .collect();

        Ok(NovelListResponse {
            draw,
            records_total,
            records_filtered,
            data,
        })
    })
    .map_err(|e: NarouError| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::{NovelListItem, is_new_arrivals_marker};
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::json;

    fn sample_record() -> crate::db::novel_record::NovelRecord {
        crate::db::novel_record::NovelRecord {
            id: 1,
            author: "author".to_string(),
            title: "title".to_string(),
            file_title: "file title".to_string(),
            toc_url: "https://example.com".to_string(),
            sitename: "カクヨム".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.with_ymd_and_hms(2026, 4, 17, 0, 0, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: vec!["sf".to_string()],
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn new_arrivals_marker_uses_ruby_six_hour_window() {
        let last_update = Utc.with_ymd_and_hms(2026, 4, 17, 0, 0, 0).unwrap();
        let now = last_update + Duration::hours(5);
        assert!(is_new_arrivals_marker(Some(last_update), last_update, now));

        let boundary = last_update + Duration::hours(6);
        assert!(is_new_arrivals_marker(Some(last_update), last_update, boundary));

        let expired = last_update + Duration::hours(7);
        assert!(!is_new_arrivals_marker(Some(last_update), last_update, expired));
    }

    #[test]
    fn new_arrivals_marker_requires_new_arrivals_date_not_older_than_last_update() {
        let last_update = Utc.with_ymd_and_hms(2026, 4, 17, 12, 0, 0).unwrap();
        let earlier = last_update - Duration::minutes(1);
        let now = last_update + Duration::minutes(30);
        assert!(!is_new_arrivals_marker(Some(earlier), last_update, now));
    }

    #[test]
    fn novel_list_item_serializes_dates_as_epoch_integers() {
        let item = NovelListItem {
            id: 5,
            title: "title".to_string(),
            author: "author".to_string(),
            sitename: "site".to_string(),
            novel_type: 1,
            end: false,
            last_update: 1_776_384_000,
            general_lastup: Some(1_776_470_400),
            last_check_date: Some(1_776_556_800),
            new_arrivals_date: Some(1_776_384_000),
            tags: vec!["tag".to_string()],
            new_arrivals: true,
            frozen: false,
            suspend: false,
            length: Some(1234),
            toc_url: "https://example.com".to_string(),
            general_all_no: Some(99),
        };

        let value = serde_json::to_value(item).unwrap();
        assert_eq!(value["last_update"], json!(1_776_384_000));
        assert_eq!(value["general_lastup"], json!(1_776_470_400));
        assert_eq!(value["last_check_date"], json!(1_776_556_800));
        assert_eq!(value["new_arrivals_date"], json!(1_776_384_000));
    }

    #[test]
    fn record_matches_search_finds_site_name_for_non_narou_records() {
        let record = sample_record();
        let tokens = super::collect_search_tokens(Some("カクヨム"), None);
        assert!(super::record_matches_search(&record, &tokens, false));
        let tokens = super::collect_search_tokens(Some("narou"), None);
        assert!(!super::record_matches_search(&record, &tokens, false));
    }

    #[test]
    fn collect_search_tokens_combines_filter_and_search_value_as_and_terms() {
        let record = sample_record();
        let tokens = super::collect_search_tokens(Some("title"), Some("author"));
        assert!(super::record_matches_search(&record, &tokens, false));

        let tokens = super::collect_search_tokens(Some("title"), Some("missing"));
        assert!(!super::record_matches_search(&record, &tokens, false));
    }

    #[test]
    fn record_matches_search_unqualified_terms_use_ruby_field_subset() {
        let mut record = sample_record();
        record.length = Some(1234);
        record.general_all_no = Some(99);
        let tokens = super::collect_search_tokens(Some("1234"), None);
        assert!(!super::record_matches_search(&record, &tokens, false));

        let tokens = super::collect_search_tokens(Some("sf"), None);
        assert!(super::record_matches_search(&record, &tokens, false));
    }

    #[test]
    fn record_matches_search_supports_tag_or_and_negation() {
        let mut record = sample_record();
        record.tags = vec!["sf".to_string(), "coffee".to_string()];

        let tokens = super::collect_search_tokens(Some("tag:coffee|mystery"), None);
        assert!(super::record_matches_search(&record, &tokens, false));

        let tokens = super::collect_search_tokens(Some("-tag:coffee"), None);
        assert!(!super::record_matches_search(&record, &tokens, false));

        let tokens = super::collect_search_tokens(Some("^tag:mystery"), None);
        assert!(super::record_matches_search(&record, &tokens, false));
    }

    #[test]
    fn record_matches_search_includes_status_text() {
        let mut record = sample_record();
        record.tags.push("404".to_string());

        let tokens = super::collect_search_tokens(Some("削除"), None);
        assert!(super::record_matches_search(&record, &tokens, false));

        let tokens = super::collect_search_tokens(Some("凍結"), None);
        assert!(super::record_matches_search(&record, &tokens, true));
    }
}

pub async fn get_novel(
    State(_state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let record = with_database(|db| {
        db.get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    let value = serde_json::to_value(&record).unwrap_or_default();
    Ok(Json(value))
}

pub async fn get_story(
    State(_state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let id_str = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "id is required".to_string()))?;
    let id: i64 = id_str
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid id".to_string()))?;

    let record = with_database(|db| {
        db.get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    let novel_dir = with_database(|db| {
        super::safe_existing_novel_dir(db.archive_root(), &record)
            .map_err(NarouError::Database)
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let toc = crate::downloader::persistence::load_toc_file(&novel_dir);
    let (title, story) = match toc {
        Some(t) => {
            let story = t.story.unwrap_or_default().trim().to_string();
            (t.title, story)
        }
        None => (record.title, String::new()),
    };

    Ok(Json(serde_json::json!({ "title": title, "story": story })))
}

pub async fn remove_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    body: Option<Json<serde_json::Value>>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let with_file = body
        .and_then(|b| b.get("with_file").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    let mut args = vec!["remove".to_string(), "--yes".to_string()];
    if with_file {
        args.push("--with-file".to_string());
    }
    args.push(id.to_string());
    let output =
        super::jobs::run_cli_and_broadcast(&state, args, super::non_external_console_target())
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if !output.status.success() {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "remove の実行に失敗しました".to_string()));
    }
    with_database_mut(|db| db.refresh())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Removed {}", id),
    }))
}

pub async fn freeze_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let output = super::jobs::run_cli_and_broadcast(
        &state,
        vec!["freeze".to_string(), "--on".to_string(), id.to_string()],
        super::non_external_console_target(),
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if !output.status.success() {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "freeze の実行に失敗しました".to_string()));
    }
    with_database_mut(|db| db.refresh())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.push_server.broadcast_event("table.reload", "");
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Froze {}", id),
    }))
}

pub async fn unfreeze_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let output = super::jobs::run_cli_and_broadcast(
        &state,
        vec!["freeze".to_string(), "--off".to_string(), id.to_string()],
        super::non_external_console_target(),
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if !output.status.success() {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "freeze の実行に失敗しました".to_string()));
    }
    with_database_mut(|db| db.refresh())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.push_server.broadcast_event("table.reload", "");
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Unfroze {}", id),
    }))
}

pub async fn author_comments(
    State(_state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let record = with_database(|db| {
        db.get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    let novel_dir = with_database(|db| {
        super::safe_existing_novel_dir(db.archive_root(), &record)
            .map_err(NarouError::Database)
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let toc = crate::downloader::persistence::load_toc_file(&novel_dir)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "TOC not found".to_string()))?;

    let section_dir = novel_dir.join(crate::downloader::SECTION_SAVE_DIR);
    let mut comments = Vec::new();
    let mut introductions_count: usize = 0;
    let mut postscripts_count: usize = 0;

    for sub in &toc.subtitles {
        let Some(path) =
            crate::downloader::persistence::resolve_section_file_path(&section_dir, sub)
        else {
            continue;
        };
        let sf = match crate::downloader::persistence::load_section_file(&path) {
            Some(sf) => sf,
            None => continue,
        };

        let data_type = if sf.element.data_type.is_empty() {
            "text"
        } else {
            &sf.element.data_type
        };
        let (introduction, postscript) = if data_type == "html" {
            (
                crate::downloader::html::to_aozora_strip_decoration(&sf.element.introduction),
                crate::downloader::html::to_aozora_strip_decoration(&sf.element.postscript),
            )
        } else {
            (
                sf.element.introduction.clone(),
                sf.element.postscript.clone(),
            )
        };

        if !introduction.is_empty() {
            introductions_count += 1;
        }
        if !postscript.is_empty() {
            postscripts_count += 1;
        }

        comments.push(serde_json::json!({
            "subtitle": sub.subtitle,
            "introduction": introduction,
            "postscript": postscript,
        }));
    }

    let total = toc.subtitles.len() as f64;
    let introductions_ratio = if total > 0.0 {
        (introductions_count as f64 / total * 100.0 * 100.0).round() / 100.0
    } else {
        0.0
    };
    let postscripts_ratio = if total > 0.0 {
        (postscripts_count as f64 / total * 100.0 * 100.0).round() / 100.0
    } else {
        0.0
    };

    Ok(Json(serde_json::json!({
        "title": record.title,
        "introductions_ratio": introductions_ratio,
        "postscripts_ratio": postscripts_ratio,
        "comments": comments,
    })))
}

pub async fn download_ebook(
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Response, (StatusCode, String)> {
    use axum::http::header;
    use axum::response::IntoResponse;

    let record = with_database(|db| {
        db.get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    let novel_dir = with_database(|db| {
        super::safe_existing_novel_dir(db.archive_root(), &record)
            .map_err(NarouError::Database)
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let device = crate::compat::current_device();
    let ext = device
        .as_ref()
        .map(|d| d.ebook_file_ext())
        .unwrap_or(".epub");

    let paths = crate::mail::get_ebook_file_paths(&record, &novel_dir, ext)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let find_existing = |paths: &[std::path::PathBuf]| -> Option<std::path::PathBuf> {
        paths.iter().find(|p| p.exists()).cloned()
    };

    let file_path = find_existing(&paths)
        .or_else(|| {
            if ext != ".epub" {
                crate::mail::get_ebook_file_paths(&record, &novel_dir, ".epub")
                    .ok()
                    .and_then(|eps| find_existing(&eps))
            } else {
                None
            }
        })
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("Ebook not found for ID={}", id),
            )
        })?;

    let data = std::fs::read(&file_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ebook.epub");
    let disposition = sanitize_content_disposition(filename);

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        data,
    )
        .into_response())
}

/// Sanitizes a filename for use in Content-Disposition header.
/// Replaces quotes and control characters, then wraps in `filename="..."`.
fn sanitize_content_disposition(filename: &str) -> String {
    let sanitized: String = filename
        .chars()
        .map(|c| {
            if c == '"' || c.is_ascii_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    format!("attachment; filename=\"{}\"", sanitized)
}
