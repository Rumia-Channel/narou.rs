use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Json, Response},
};

use crate::compat::{load_frozen_ids_from_inventory, record_is_frozen, set_frozen_state};
use crate::db::{with_database, with_database_mut};
use crate::error::NarouError;

use super::AppState;
use super::state::{ApiResponse, IdPath, ListParams, NovelListItem, NovelListResponse};

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
    let draw = params.draw.unwrap_or(1);
    let start = params.start.unwrap_or(0);
    let length = params.length.unwrap_or(50);
    let search = params.search_value.unwrap_or_default();
    let order_col = params.order_column.unwrap_or(0);
    let order_dir = params.order_dir.unwrap_or_else(|| "asc".to_string());
    let frozen_ids = with_database(|db| load_frozen_ids_from_inventory(db.inventory())).unwrap_or_default();

    let response = with_database(|db| {
        let all_records: Vec<_> = db.all_records().values().collect();

        let mut filtered: Vec<_> = if search.is_empty() {
            all_records
        } else {
            let search_lower = search.to_lowercase();
            all_records
                .into_iter()
                .filter(|r| {
                    r.title.to_lowercase().contains(&search_lower)
                        || r.author.to_lowercase().contains(&search_lower)
                        || r.tags
                            .iter()
                            .any(|t| t.to_lowercase().contains(&search_lower))
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
            .take(length as usize)
            .map(|r| {
                let now = chrono::Utc::now();
                let is_new = r.new_arrivals_date.is_some_and(|nad| {
                    let limit = chrono::Duration::seconds(259200); // 3 days
                    nad >= r.last_update && (nad + limit) > now
                });
                NovelListItem {
                    id: r.id,
                    title: r.title.clone(),
                    author: r.author.clone(),
                    sitename: r.sitename.clone(),
                    novel_type: r.novel_type,
                    end: r.end,
                    last_update: r.last_update.format("%Y-%m-%d %H:%M").to_string(),
                    general_lastup: r.general_lastup
                        .map(|d| d.format("%Y-%m-%d %H:%M").to_string()),
                    last_check_date: r.last_check_date
                        .map(|d| d.format("%Y-%m-%d %H:%M").to_string()),
                    new_arrivals_date: r.new_arrivals_date
                        .map(|d| d.format("%Y-%m-%d %H:%M").to_string()),
                    tags: r.tags.clone(),
                    new_arrivals: is_new,
                    frozen: record_is_frozen(r, &frozen_ids),
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
        Ok(crate::db::existing_novel_dir_for_record(
            db.archive_root(),
            &record,
        ))
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let toc = crate::downloader::persistence::load_toc_file(&novel_dir);
    let (title, story) = match toc {
        Some(t) => {
            let s = t.story.unwrap_or_default().trim().to_string();
            let html_story = s.replace('\n', "<br>");
            (t.title, html_story)
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
    let result = with_database_mut(|db| {
        if let Some(record) = db.remove(id) {
            if with_file {
                let dir = crate::db::existing_novel_dir_for_record(db.archive_root(), &record);
                let _ = std::fs::remove_dir_all(&dir);
            }
            db.save()?;
            Ok::<String, NarouError>(record.title)
        } else {
            Err(NarouError::NotFound(format!("ID: {}", id)))
        }
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast("remove", &result);
    Ok(Json(ApiResponse {
        success: true,
        message: result,
    }))
}

pub async fn freeze_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    set_frozen_state(id, true).map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast("freeze", &id.to_string());
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Froze {}", id),
    }))
}

pub async fn unfreeze_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    set_frozen_state(id, false).map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast("unfreeze", &id.to_string());
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
        Ok(crate::db::existing_novel_dir_for_record(
            db.archive_root(),
            &record,
        ))
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let toc = crate::downloader::persistence::load_toc_file(&novel_dir)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "TOC not found".to_string()))?;

    let section_dir = novel_dir.join(crate::downloader::SECTION_SAVE_DIR);
    let mut comments = Vec::new();
    let mut introductions_count: usize = 0;
    let mut postscripts_count: usize = 0;

    for sub in &toc.subtitles {
        let filename = format!("{} {}.yaml", sub.index, sub.file_subtitle);
        let path = section_dir.join(&filename);
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
            "introduction": introduction.replace('\n', "<br>"),
            "postscript": postscript.replace('\n', "<br>"),
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
        Ok(crate::db::existing_novel_dir_for_record(
            db.archive_root(),
            &record,
        ))
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
    let disposition = format!("attachment; filename=\"{}\"", filename);

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
