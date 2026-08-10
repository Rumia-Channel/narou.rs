use axum::{
    extract::{Form, Path, Query, State},
    http::StatusCode,
    response::{Json, Response},
};

use crate::application::{
    ApplicationError, LibraryListRequest, LibrarySortColumn, LibrarySortOrder,
};
use crate::db::with_database;
use crate::error::NarouError;

use super::AppState;
use super::state::{ApiResponse, IdPath, ListParams, NovelListItem, NovelListResponse};

fn map_application_error(error: ApplicationError) -> (StatusCode, String) {
    match error {
        ApplicationError::InvalidRequest(message) => (StatusCode::BAD_REQUEST, message),
        ApplicationError::NotFound(message) => (StatusCode::NOT_FOUND, message),
        ApplicationError::Platform(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}

fn combine_search_terms(filter: Option<String>, search: Option<String>) -> Option<String> {
    match (filter, search) {
        (Some(filter), Some(search)) if !filter.is_empty() && !search.is_empty() => {
            Some(format!("{filter} {search}"))
        }
        (Some(filter), _) if !filter.is_empty() => Some(filter),
        (_, Some(search)) if !search.is_empty() => Some(search),
        _ => None,
    }
}
pub async fn index() -> &'static str {
    "narou.rs API server"
}

pub async fn novels_count(State(state): State<AppState>) -> Json<serde_json::Value> {
    let count = state.services.library.count().await.unwrap_or(0);
    Json(serde_json::json!({ "count": count }))
}

pub async fn api_list(
    Query(params): Query<ListParams>,
    State(state): State<AppState>,
) -> Result<Json<NovelListResponse>, (StatusCode, String)> {
    api_list_inner(&state, params).await
}

pub async fn api_list_post(
    State(state): State<AppState>,
    Form(params): Form<ListParams>,
) -> Result<Json<NovelListResponse>, (StatusCode, String)> {
    api_list_inner(&state, params).await
}

async fn api_list_inner(
    state: &AppState,
    params: ListParams,
) -> Result<Json<NovelListResponse>, (StatusCode, String)> {
    let draw = params.draw.unwrap_or(1);
    let return_all = params.all.unwrap_or(false);
    let start = if return_all {
        0
    } else {
        params.start.unwrap_or(0) as usize
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
        params.filter.as_ref().map_or(0, String::len)
            + params.search_value.as_ref().map_or(0, String::len);
    if total_query_bytes > super::MAX_WEB_SEARCH_BYTES {
        return Err((StatusCode::BAD_REQUEST, "search query is too long".to_string()));
    }

    let search = combine_search_terms(params.filter, params.search_value);
    let sort_column = match params.order_column.unwrap_or(0) {
        1 => LibrarySortColumn::LastUpdate,
        2 => LibrarySortColumn::GeneralLastup,
        3 => LibrarySortColumn::LastCheckDate,
        4 => LibrarySortColumn::Title,
        5 => LibrarySortColumn::Author,
        6 => LibrarySortColumn::SiteName,
        7 => LibrarySortColumn::NovelType,
        9 => LibrarySortColumn::GeneralAllNo,
        10 => LibrarySortColumn::Length,
        _ => LibrarySortColumn::Id,
    };
    let sort_order = if params.order_dir.as_deref() == Some("desc") {
        LibrarySortOrder::Descending
    } else {
        LibrarySortOrder::Ascending
    };
    let page = state
        .services
        .library
        .list(&LibraryListRequest {
            search,
            start,
            length,
            sort_column,
            sort_order,
        })
        .await
        .map_err(map_application_error)?;

    let data = page
        .data
        .into_iter()
        .map(|record| NovelListItem {
            id: record.id,
            title: record.title,
            author: record.author,
            sitename: record.sitename,
            novel_type: record.novel_type,
            end: record.end,
            last_update: record.last_update.timestamp(),
            general_lastup: record.general_lastup.map(|dt| dt.timestamp()),
            last_check_date: record.last_check_date.map(|dt| dt.timestamp()),
            new_arrivals_date: record.new_arrivals_date.map(|dt| dt.timestamp()),
            tags: record.tags,
            new_arrivals: record.new_arrivals,
            frozen: record.frozen,
            suspend: record.suspend,
            length: record.length,
            toc_url: record.toc_url,
            general_all_no: record.general_all_no,
        })
        .collect();

    Ok(Json(NovelListResponse {
        draw,
        records_total: page.records_total,
        records_filtered: page.records_filtered,
        data,
    }))
}

#[cfg(test)]
mod tests {
    use super::NovelListItem;
    use serde_json::json;

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
}

pub async fn get_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let record = state
        .services
        .library
        .get(id.into())
        .await
        .map_err(map_application_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("ID: {}", id)))?;

    let value = serde_json::to_value(&record).unwrap_or_default();
    Ok(Json(value))
}

pub async fn get_story(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let id_str = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "id is required".to_string()))?;
    let id: i64 = id_str
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid id".to_string()))?;

    let record = state
        .services
        .library
        .get(id.into())
        .await
        .map_err(map_application_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("ID: {}", id)))?;

    let toc = state
        .services
        .content
        .toc(id.into())
        .await
        .map_err(map_application_error)?
        .and_then(|bytes| serde_yaml::from_slice::<crate::downloader::types::TocObject>(&bytes).ok());
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
    let result = state
        .services
        .novel_actions
        .remove(&crate::application::RemoveRequest {
            ids: vec![id.into()],
            delete_files: with_file,
        })
        .await
        .map_err(map_application_error)?;
    if !result.missing.is_empty() {
        return Err((StatusCode::NOT_FOUND, format!("ID: {}", id)));
    }

    let files_ok = result
        .files
        .iter()
        .all(|(_, status)| matches!(status, crate::application::FileDeletionStatus::Deleted));
    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");
    Ok(Json(ApiResponse {
        success: files_ok,
        message: if files_ok {
            format!("Removed {}", id)
        } else {
            format!("Removed {} (file deletion incomplete)", id)
        },
    }))
}

pub async fn freeze_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let result = state
        .services
        .novel_actions
        .freeze(&[id.into()])
        .await
        .map_err(map_application_error)?;
    if !result.missing.is_empty() {
        return Err((StatusCode::NOT_FOUND, format!("ID: {}", id)));
    }
    let persisted = result.store_failed.is_empty();
    state.push_server.broadcast_event("table.reload", "");
    Ok(Json(ApiResponse {
        success: persisted,
        message: if persisted {
            format!("Froze {}", id)
        } else {
            format!("Froze {} (freeze state persistence failed)", id)
        },
    }))
}

pub async fn unfreeze_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let result = state
        .services
        .novel_actions
        .unfreeze(&[id.into()])
        .await
        .map_err(map_application_error)?;
    if !result.missing.is_empty() {
        return Err((StatusCode::NOT_FOUND, format!("ID: {}", id)));
    }
    let persisted = result.store_failed.is_empty();
    state.push_server.broadcast_event("table.reload", "");
    Ok(Json(ApiResponse {
        success: persisted,
        message: if persisted {
            format!("Unfroze {}", id)
        } else {
            format!("Unfroze {} (freeze state persistence failed)", id)
        },
    }))
}

pub async fn author_comments(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let record = state
        .services
        .library
        .get(id.into())
        .await
        .map_err(map_application_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("ID: {}", id)))?;

    let toc = state
        .services
        .content
        .toc(id.into())
        .await
        .map_err(map_application_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "TOC not found".to_string()))
        .and_then(|bytes| {
            serde_yaml::from_slice::<crate::downloader::types::TocObject>(&bytes)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        })?;

    let mut comments = Vec::new();
    let mut introductions_count: usize = 0;
    let mut postscripts_count: usize = 0;

    for sub in &toc.subtitles {
        let Some(bytes) = state
            .services
            .content
            .section(id.into(), &sub.index, &sub.file_subtitle)
            .await
            .map_err(map_application_error)?
        else {
            continue;
        };
        let Ok(sf) = serde_yaml::from_slice::<crate::downloader::types::SectionFile>(&bytes)
        else {
            continue;
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
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Response, (StatusCode, String)> {
    use axum::body::Body;
    use axum::http::{HeaderValue, header};
    use tokio::io::AsyncReadExt;

    let record = state
        .services
        .library
        .get(id.into())
        .await
        .map_err(map_application_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("ID: {}", id)))?;

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

    let file = tokio::fs::File::open(&file_path)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let content_length = file
        .metadata()
        .await
        .map(|metadata| metadata.len())
        .ok();
    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ebook.epub");
    let disposition = sanitize_content_disposition(filename);

    let stream = futures::stream::try_unfold(file, |mut file| async move {
        let mut buf = vec![0; 64 * 1024];
        let read = file.read(&mut buf).await?;
        if read == 0 {
            Ok::<Option<(Vec<u8>, tokio::fs::File)>, std::io::Error>(None)
        } else {
            buf.truncate(read);
            Ok(Some((buf, file)))
        }
    });

    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    let disposition_value = HeaderValue::from_bytes(disposition.as_bytes())
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"ebook.epub\""));
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, disposition_value);
    if let Some(len) = content_length
        && let Ok(value) = HeaderValue::from_str(&len.to_string())
    {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }

    Ok(response)
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
