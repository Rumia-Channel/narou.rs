use axum::{
    extract::{Form, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};

use crate::application::web_payloads::{
    DEFAULT_WEB_PAGE_LENGTH, MAX_WEB_PAGE_LENGTH, MAX_WEB_SEARCH_BYTES, combine_search_terms,
    sort_column_from_index,
};
use crate::application::webui::sanitize_content_disposition;
use crate::application::{ApplicationError, LibraryListRequest, LibrarySortOrder};
use crate::db::with_database;
use crate::error::NarouError;

use super::AppState;
use super::state::{ApiResponse, IdPath, ListParams, NovelListItem, NovelListResponse};

#[derive(Clone)]
struct ReadResponseOwner(std::sync::Arc<futures::lock::Mutex<Option<crate::application::novel_access::NovelReadScope>>>);

impl ReadResponseOwner {
    async fn refresh(&self) -> crate::error::Result<()> {
        if let Some(scope) = self.0.lock().await.as_mut() { scope.refresh_all().await?; }
        Ok(())
    }

    async fn release(&self) -> crate::error::Result<()> {
        let scope = self.0.lock().await.take();
        match scope { Some(scope) => scope.release_all().await, None => Ok(()) }
    }
}

/// The body owns admissions across backpressure, EOF/error and cancellation.
pub(crate) fn leased_response(
    response: Response,
    scope: crate::application::novel_access::NovelReadScope,
) -> Response {
    use futures::StreamExt;
    let (mut parts, body) = response.into_parts();
    let owner = ReadResponseOwner(std::sync::Arc::new(futures::lock::Mutex::new(Some(scope))));
    parts.extensions.insert(owner.clone());
    let stream = futures::stream::try_unfold(
        (body.into_data_stream(), owner),
        |(mut body, owner)| async move {
            let result = async {
                owner.refresh().await?;
                let next = body.next().await.transpose()
                    .map_err(|error| NarouError::Platform(error.to_string()))?;
                owner.refresh().await?;
                Ok::<_, NarouError>(next)
            }.await;
            match result {
                Ok(Some(bytes)) => Ok(Some((bytes, (body, owner)))),
                Ok(None) => { drop(body); owner.release().await?; Ok(None) }
                Err(error) => { drop(body); let _ = owner.release().await; Err(error) }
            }
        },
    );
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

/// Axum suppresses HEAD bodies without polling them. Retire ownership before
/// suppression, and release error responses without discarding their message.
pub(crate) async fn finalize_read_response(mut response: Response, method: &axum::http::Method) -> Response {
    if let Some(owner) = response.extensions_mut().remove::<ReadResponseOwner>() {
        if method == axum::http::Method::HEAD {
            drop(std::mem::replace(response.body_mut(), axum::body::Body::empty()));
            if let Err(error) = owner.release().await {
                *response.status_mut() = map_application_error(ApplicationError::platform(error)).0;
            }
        } else if !response.status().is_success() {
            let _ = owner.release().await;
        }
    }
    response
}

async fn finish_read_response(
    result: Result<Response, (StatusCode, String)>,
    scope: crate::application::novel_access::NovelReadScope,
) -> Result<Response, (StatusCode, String)> {
    match result {
        Ok(response) => Ok(leased_response(response, scope)),
        Err(error) => { let _ = scope.release_all().await; Err(error) }
    }
}

pub(super) fn map_application_error(error: ApplicationError) -> (StatusCode, String) {
    let (status, _) = crate::application::external_api::http_error(&error);
    (StatusCode::from_u16(status).expect("application status"), error.message().to_owned())
}

pub(super) fn epub_error_code(status: StatusCode, message: &str) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "bad_request",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT | StatusCode::LOCKED | StatusCode::SERVICE_UNAVAILABLE => {
            let (mapped, code) = crate::application::external_api::http_error(
                &ApplicationError::Platform(message.to_owned()),
            );
            if mapped == status.as_u16() {
                code
            } else if status == StatusCode::CONFLICT {
                "conflict"
            } else if status == StatusCode::LOCKED {
                "novel_busy"
            } else {
                "service_unavailable"
            }
        }
        _ => "internal_error",
    }
}

pub async fn index() -> &'static str {
    "narou.rs API server"
}

pub async fn novels_count(State(state): State<AppState>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let count = state.services.library.count().await.map_err(map_application_error)?;
    Ok(Json(serde_json::json!({ "count": count })))
}

pub async fn api_list(
    Query(params): Query<ListParams>,
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, String)> {
    api_list_inner(&state, params).await
}

pub async fn api_list_post(
    State(state): State<AppState>,
    Form(params): Form<ListParams>,
) -> Result<Response, (StatusCode, String)> {
    api_list_inner(&state, params).await
}

async fn api_list_inner(
    state: &AppState,
    params: ListParams,
) -> Result<Response, (StatusCode, String)> {
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
                .unwrap_or(DEFAULT_WEB_PAGE_LENGTH)
                .min(MAX_WEB_PAGE_LENGTH) as usize,
        )
    };
    let total_query_bytes =
        params.filter.as_ref().map_or(0, String::len)
            + params.search_value.as_ref().map_or(0, String::len);
    if total_query_bytes > MAX_WEB_SEARCH_BYTES {
        return Err((StatusCode::BAD_REQUEST, "search query is too long".to_string()));
    }

    let search = combine_search_terms(params.filter, params.search_value);
    let sort_column = sort_column_from_index(params.order_column);
    let sort_order = if params.order_dir.as_deref() == Some("desc") {
        LibrarySortOrder::Descending
    } else {
        LibrarySortOrder::Ascending
    };
    let (hidden, exclude_ids) = tokio::task::spawn_blocking(|| {
        let hidden = crate::native::hidden::load().unwrap_or_default();
        let exclude_ids: Vec<i64> = crate::native::virtual_novel::load()
            .map(|members| members.all_member_ids().into_iter().collect())
            .unwrap_or_default();
        (hidden, exclude_ids)
    })
    .await
    .map_err(|error| map_application_error(ApplicationError::Platform(error.to_string())))?;
    let mut reads = state.services.content.read_scope("native-library-list");
    let result = async {
    let page = state
        .services
        .library
        .list(&mut reads, &LibraryListRequest {
            search,
            start,
            length,
            sort_column,
            sort_order,
            hidden: Some(hidden),
            include_hidden: params.show_hidden.unwrap_or(false),
            exclude_ids,
            attribute_tags_enabled: crate::application::attribute_tags::enabled(&state.services.settings).await,
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
            display_url: record.display_url,
            toc_url: record.toc_url,
            ncode: record.ncode,
            general_all_no: record.general_all_no,
        })
        .collect();

    Ok(Json(NovelListResponse {
        draw,
        records_total: page.records_total,
        records_filtered: page.records_filtered,
        data,
    }).into_response())
    }.await;
    finish_read_response(result, reads).await
}


pub async fn get_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Response, (StatusCode, String)> {
    let mut reads = state.services.content.read_scope("web-novel-metadata");
    let result = async {
        let record = state.services.content.read_record(&mut reads, id.into()).await
            .map_err(map_application_error)?
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("ID: {id}")))?;
        Ok(Json(serde_json::to_value(&record).unwrap_or_default()).into_response())
    }.await;
    finish_read_response(result, reads).await
}

pub async fn get_story(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Response, (StatusCode, String)> {
    let id_str = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "id is required".to_string()))?;
    let id: i64 = id_str
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid id".to_string()))?;

    let mut read_scope = state.services.content.read_scope("web-novel-story");
    let result = async {
    let record = state.services.content.read_record(&mut read_scope, id.into()).await
        .map_err(map_application_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("ID: {id}")))?;
    let toc = state
        .services
        .content
        .toc(&mut read_scope, id.into())
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

    Ok(Json(serde_json::json!({ "title": title, "story": story })).into_response())
    }.await;
    finish_read_response(result, read_scope).await
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
    for event in crate::application::webui::mutations::mutation_events(
        crate::application::webui::mutations::MutationNotice::TableAndTags,
    ) {
        state.push_server.broadcast_raw(event);
    }
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
    for event in crate::application::webui::mutations::mutation_events(
        crate::application::webui::mutations::MutationNotice::TableAndTags,
    ) {
        state.push_server.broadcast_raw(event);
    }
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
    for event in crate::application::webui::mutations::mutation_events(
        crate::application::webui::mutations::MutationNotice::TableAndTags,
    ) {
        state.push_server.broadcast_raw(event);
    }
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
) -> Result<Response, (StatusCode, String)> {
    let mut read_scope = state.services.content.read_scope("web-author-comments");
    let result = async {
    let record = state.services.content.read_record(&mut read_scope, id.into()).await
        .map_err(map_application_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("ID: {id}")))?;
    let toc = state
        .services
        .content
        .toc(&mut read_scope, id.into())
        .await
        .map_err(map_application_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "TOC not found".to_string()))
        .and_then(|bytes| {
            serde_yaml::from_slice::<crate::downloader::types::TocObject>(&bytes)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        })?;

    let mut sections: Vec<Option<crate::downloader::types::SectionFile>> =
        Vec::with_capacity(toc.subtitles.len());
    for sub in &toc.subtitles {
        let Some(bytes) = state
            .services
            .content
            .section(&mut read_scope, id.into(), &sub.index, &sub.file_subtitle)
            .await
            .map_err(map_application_error)?
        else {
            sections.push(None);
            continue;
        };
        sections.push(serde_yaml::from_slice::<crate::downloader::types::SectionFile>(&bytes).ok());
    }

    Ok(Json(
        crate::application::webui::author_comments::author_comments_payload(
            &record.title,
            &toc,
            &sections,
        ),
    ).into_response())
    }.await;
    finish_read_response(result, read_scope).await
}

pub async fn download_ebook(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Response, (StatusCode, String)> {
    let mut reads = state.services.content.read_scope("web-ebook");
    let result = async {
    let record = state.services.content.read_record(&mut reads, id.into()).await
        .map_err(map_application_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("ID: {id}")))?;
    let keys = crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)
        .map_err(|error| map_application_error(ApplicationError::platform(error)))?;

    // 仮想小説は toc を持たないため、DL 前に結合 EPUB を生成する。
    if record
        .ncode
        .as_deref()
        .is_some_and(crate::virtual_novel::is_virtual_ncode)
    {
        let generated = generate_virtual_epub(&mut reads, record).await?;
        return serve_epub_file(&mut reads, &keys, &generated).await;
    }

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

    let candidates = crate::mail::ebook_file_candidates(&mut reads, &record, ext)
        .await
        .map_err(|error| map_application_error(ApplicationError::platform(error)))?;

    let find_existing = |paths: &[(String, bool)]| -> Option<std::path::PathBuf> {
        paths
            .iter()
            .find(|(_, present)| *present)
            .map(|(name, _)| novel_dir.join(name))
    };

    let existing = match find_existing(&candidates) {
        Some(path) => Some(path),
        None if ext != ".epub" => {
            let epubs = crate::mail::ebook_file_candidates(&mut reads, &record, ".epub")
                .await
                .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
            find_existing(&epubs)
        }
        None => None,
    };

    #[cfg(feature = "lite")]
    let file_path = match existing {
        Some(path) => path,
        None => {
            let context = crate::application::messages::MessageContext::new(
                state.push_server.clone(), crate::compat::load_local_setting_bool("webui.debug-mode"),
            );
            let (bytes, filename) = context.scope(generate_epub_on_demand(&mut reads, id, &record, &novel_dir)).await?;
            return serve_epub_bytes(bytes, &filename);
        }
    };

    #[cfg(not(feature = "lite"))]
    let file_path = existing.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("Ebook not found for ID={}", id),
        )
    })?;

    serve_epub_file(&mut reads, &keys, &file_path).await
    }.await;
    finish_read_response(result, reads).await
}

pub(crate) async fn generate_virtual_epub(
    reads: &mut crate::application::novel_access::NovelReadScope,
    record: crate::db::NovelRecord,
) -> Result<std::path::PathBuf, (StatusCode, String)> {
    let write = crate::native::materialization::capture_write_for_record(&record)
        .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
    let mut lease = reads.lease_for(write.prefix()).await.map_err(|error| map_application_error(ApplicationError::platform(error)))?;
    lease.validate().await.map_err(|error| map_application_error(ApplicationError::platform(error)))?;
    if lease.epoch() != write.epoch() { return Err((StatusCode::LOCKED, "virtual parent generation changed".into())); }
    tokio::task::spawn_blocking(move || {
        let all = crate::native::virtual_novel::load()
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
        let members = all.get(record.id)
            .ok_or_else(|| (StatusCode::NOT_FOUND, "virtual members not found".to_string()))?;
        crate::native::virtual_novel::convert_virtual(
            &write,
            record.id, &record, members, Some(crate::converter::device::Device::Epub), false, false,
        ).map_err(|error| map_application_error(ApplicationError::platform(error)))
    })
    .await
    .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
}

/// 保存済みの EPUB ファイルを返す。
async fn serve_epub_file(
    scope: &mut crate::application::novel_access::NovelReadScope,
    keys: &crate::platform::NovelObjectKeys,
    file_path: &std::path::Path,
) -> Result<Response, (StatusCode, String)> {
    use axum::body::Body;
    use axum::http::HeaderValue;
    use axum::http::header;
    use tokio::io::AsyncReadExt;

    let inventory = crate::db::inventory::Inventory::with_default_root()
        .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
    let root = inventory.root_dir().join(crate::downloader::ARCHIVE_ROOT_DIR);
    let content_length;
    let filename = file_path.file_name().and_then(|name| name.to_str()).unwrap_or("ebook.epub");
    let disposition = sanitize_content_disposition(filename);
    let body = if file_path.starts_with(&root) || !crate::application::novel_access::is_private_path(file_path)
        .map_err(|error| map_application_error(ApplicationError::platform(error)))? {
        let prefix = crate::platform::ObjectPrefix::from(keys.prefix().clone());
        let mut lease = scope.lease_for(&prefix).await
            .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
        lease.validate().await.map_err(|error| map_application_error(ApplicationError::platform(error)))?;
        let store = crate::native::object_store::NativeStore::for_narou_root(inventory.root_dir())
            .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
        let path = crate::db::paths::ensure_within_archive_root(file_path, &root)
            .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
        let directory = crate::db::paths::ensure_within_archive_root(&store.novel_directory(&prefix)
            .map_err(|error| map_application_error(ApplicationError::platform(error)))?, &root)
            .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
        let relative = path.strip_prefix(&directory).map_err(|_| (StatusCode::CONFLICT, "ebook moved outside its admitted novel".to_owned()))?;
        let relative = relative.to_str().ok_or_else(|| (StatusCode::BAD_REQUEST, "ebook path is not UTF-8".to_owned()))?.replace('\\', "/");
        let key = keys.prefix().join(&relative).map_err(|error| map_application_error(ApplicationError::platform(error)))?;
        let (metadata, stream) = store.open_leased_asset(lease.prefix(), lease.lease(), &key, 0..u64::MAX).await
            .map_err(|error| map_application_error(ApplicationError::platform(error)))?
            .ok_or_else(|| (StatusCode::NOT_FOUND, "ebook not found".into()))?;
        content_length = Some(metadata.size);
        Body::from_stream(stream)
    } else {
        // Only unpublished/generated private files may use a direct native open.
        crate::application::novel_access::require_private_path(file_path)
            .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
        let file = tokio::fs::File::open(file_path).await
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
        content_length = Some(file.metadata().await.map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?.len());
        let stream = futures::stream::try_unfold(file, |mut file| async move {
            let mut bytes = vec![0; 64 * 1024];
            let count = file.read(&mut bytes).await?;
            if count == 0 { Ok::<_, std::io::Error>(None) }
            else { bytes.truncate(count); Ok(Some((bytes, file))) }
        });
        Body::from_stream(stream)
    };
    let mut response = Response::new(body);
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

/// 応答の Content-Type を EPUB として明示する。
///
/// WEB UI のダウンロード (`serve_epub_file`) は `application/octet-stream`
/// のままにする (ブラウザが拡張子で扱うため)。外部公開 API は
/// `application/epub+zip` を返すので、受け側が Content-Type で EPUB と
/// 判定できる (Worker 側と同じ)。
fn as_epub_response(mut response: Response) -> Response {
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/epub+zip"),
    );
    response
}

/// EPUB を返す (外部公開 API 用)。
///
/// 保存済みの EPUB があればそれを、無ければ `lite` ビルドではその場で生成
/// する。端末設定の影響を受けないよう、拡張子は常に `.epub`。
pub(crate) async fn serve_epub(
    state: &AppState,
    scope: &mut crate::application::novel_access::NovelReadScope,
    id: i64,
    record: &crate::db::novel_record::NovelRecord,
    novel_dir: &std::path::Path,
) -> Result<Response, (StatusCode, String)> {
    let keys = crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)
        .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
    // A saved virtual EPUB cannot validate changes to its members or settings.
    // Rebuild from current member inputs, as the UI download does.
    if record
        .ncode
        .as_deref()
        .is_some_and(crate::virtual_novel::is_virtual_ncode)
    {
        let generated = generate_virtual_epub(scope, record.clone()).await?;
        return Ok(as_epub_response(serve_epub_file(scope, &keys, &generated).await?));
    }
    let candidates = crate::mail::ebook_file_candidates(scope, record, ".epub")
        .await
        .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
    if let Some((name, _)) = candidates.iter().find(|(_, present)| *present) {
        return Ok(as_epub_response(
            serve_epub_file(scope, &keys, &novel_dir.join(name)).await?,
        ));
    }
    #[cfg(feature = "lite")]
    {
        let context = crate::application::messages::MessageContext::new(
            state.push_server.clone(),
            crate::compat::load_local_setting_bool("webui.debug-mode"),
        );
        let (bytes, filename) = context.scope(generate_epub_on_demand(scope, id, record, novel_dir)).await?;
        Ok(as_epub_response(serve_epub_bytes(bytes, &filename)?))
    }
    #[cfg(not(feature = "lite"))]
    {
        let _ = state;
        Err((StatusCode::NOT_FOUND, format!("Ebook not found for ID={id}")))
    }
}

/// Build an EPUB from the converted 青空文庫 text at request time
/// (`lite` feature). Reads the same output txt the CLI converter writes and
/// resolves illustrations from the novel's `挿絵/` directory.
#[cfg(feature = "lite")]
async fn generate_epub_on_demand(
    scope: &mut crate::application::novel_access::NovelReadScope,
    id: i64,
    record: &crate::db::NovelRecord,
    _novel_dir: &std::path::Path,
) -> Result<(Vec<u8>, String), (StatusCode, String)> {
    let inventory = crate::db::inventory::Inventory::with_default_root()
        .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
    let stores = crate::native::object_store::NativeStores::for_narou_root(inventory.root_dir())
        .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
    let keys = crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)
        .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
    let prefix = crate::platform::ObjectPrefix::from(keys.prefix().clone());
    let mut lease = scope.lease_for(&prefix).await
        .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
    let objects = crate::native::materialization::read_store(inventory.root_dir())
        .map_err(|error| map_application_error(ApplicationError::platform(error)))?;
    let root = inventory.root_dir().to_path_buf();
    let record = record.clone();
    let message_context = crate::application::messages::MessageContext::current();
    let result = tokio::task::spawn_blocking(move || {
        let mut build = || -> crate::error::Result<(Vec<u8>, String)> {
            let snapshot = crate::native::materialization::block_on(
                crate::native::materialization::ConversionInputs::begin(&objects, &mut lease, &root),
            )?;
            let directory = snapshot.path();
            let settings = crate::converter::settings::NovelSettings::load_for_novel(
                id, &record.title, &record.author, directory,
            )?;
            let toc: crate::downloader::TocFile = serde_yaml::from_slice(&std::fs::read(directory.join("toc.yaml"))?)?;
            let toc = crate::downloader::TocObject {
                title: toc.title, author: toc.author, toc_url: toc.toc_url, story: toc.story,
                subtitles: toc.subtitles, novel_type: toc.novel_type,
            };
            let filename = crate::converter::output::epub_output_filename(
                &settings, &toc, Some(&record), &crate::converter::output::local_output_naming_env(),
            );
            let text_path = crate::converter::output::create_output_text_path(&settings, id, directory, &toc, Some(&record));
            let relative = text_path.strip_prefix(directory).map_err(|_| NarouError::Conversion("ebook text escaped its private workspace".into()))?;
            let mut cached = None;
            if crate::converter::keep_converted_text_file() {
                let relative = relative.to_str().ok_or_else(|| NarouError::Conversion("ebook filename is not UTF-8".into()))?.replace('\\', "/");
                crate::native::materialization::block_on(crate::native::materialization::copy_asset(&objects, &mut lease, &keys.prefix().join(&relative)?, &text_path, None))?;
                if !text_path.is_file() && matches!(stores.primary.as_ref(), crate::native::object_store::NativeStore::Sqlite(_)) {
                    cached = stores.primary.with_reader_transaction(&prefix, lease.lease(), |connection| crate::native::sqlite::content::load_output(connection, id, "converted_text"))?;
                }
            }
            let render = !text_path.is_file() && cached.as_ref().is_none_or(|bytes| bytes.is_empty());
            crate::native::materialization::block_on(snapshot.populate(&objects, &mut lease, render, None, crate::native::materialization::ConversionInputSelection {
                illustrations: settings.enable_illust, cover: settings.enable_illust,
            }))?;
            if !text_path.is_file() {
                let bytes = match cached.filter(|bytes| !bytes.is_empty()) {
                    Some(bytes) => bytes,
                    None => {
                        let sections = crate::converter::load_sections_from_dir(directory, &toc.subtitles)?;
                        let capabilities = crate::native::converter::private_capabilities(directory)?;
                        let mut converter = match crate::converter::user_converter::UserConverter::load_with_title(directory, &settings.title_for_output(&record.title)) {
                            Some(user) => crate::converter::NovelConverter::with_user_converter_and_capabilities(settings.clone(), user, capabilities),
                            None => crate::converter::NovelConverter::with_capabilities(settings.clone(), capabilities),
                        };
                        converter.apply_record_fetch_policy(Some(&record));
                        converter.convert_novel(&toc, &sections, Some(&record))?.into_bytes()
                    }
                };
                std::fs::write(&text_path, bytes)?;
            }
            crate::native::materialization::block_on(lease.validate())?;
            let use_dakuten = crate::epub_lite::text_has_dakuten_marker(&std::fs::read_to_string(&text_path)?);
            let options = crate::epub_lite::EpubBuildOptions::from_settings(crate::epub_lite::EpubSettingsInput {
                title: &record.title, author: &record.author, settings: &settings,
                has_cover_image: crate::converter::render::cover_image_path(directory).is_some(),
                extra_assets: crate::converter::dakuten_font::lite_font_assets(use_dakuten)?,
                assets_dir: crate::compat::aozora_assets_dir(), kindle: false,
                rotate_image: crate::compat::load_local_setting_string("convert.rotate-image")
                    .and_then(|value| crate::converter::settings::ImageRotation::from_setting(&value)),
            });
            let book = crate::epub_lite::build_book(&text_path, &options)?;
            let mut bytes = Vec::new();
            crate::epub_lite::stream_epub(&book.book, &mut bytes, |path| book.resolve(path))?;
            crate::native::materialization::block_on(lease.validate())?;
            Ok((bytes, filename))
        };
        let result = match message_context { Some(context) => context.run(build), None => build() };
        let released = crate::native::materialization::block_on(lease.release());
        match result { Ok(value) => { released?; Ok(value) }, Err(error) => Err(error) }
    }).await.map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    result.map_err(|error| map_application_error(ApplicationError::platform(error)))
}

/// Wrap generated EPUB bytes into a download response.
#[cfg(feature = "lite")]
fn serve_epub_bytes(bytes: Vec<u8>, filename: &str) -> Result<Response, (StatusCode, String)> {
    use axum::body::Body;
    use axum::http::{HeaderValue, header};

    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/epub+zip"),
    );
    let disposition_value = HeaderValue::from_bytes(sanitize_content_disposition(filename).as_bytes())
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"ebook.epub\""));
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, disposition_value);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::NovelListItem;
    use serde_json::json;

    #[cfg(feature = "lite")]
    struct DatabaseRestore(Option<crate::db::Database>);

    #[cfg(feature = "lite")]
    impl Drop for DatabaseRestore {
        fn drop(&mut self) { *crate::db::DATABASE.lock() = self.0.take(); }
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
            ncode: Some("n1234ab".to_string()),
            display_url: "https://example.com".to_string(),
            general_all_no: Some(99),
        };

        let value = serde_json::to_value(item).unwrap();
        assert_eq!(value["last_update"], json!(1_776_384_000));
        assert_eq!(value["general_lastup"], json!(1_776_470_400));
        assert_eq!(value["last_check_date"], json!(1_776_556_800));
        assert_eq!(value["new_arrivals_date"], json!(1_776_384_000));
    }

    #[cfg(feature = "lite")]
    #[test]
    fn on_demand_epub_preserves_stored_images_cover_and_global_replacements() {
        use std::io::{Cursor, Read};
        let temp = tempfile::tempdir().unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(temp.path());
        let _legacy = crate::test_support::legacy_yaml_guard();
        let _database = DatabaseRestore(crate::db::DATABASE.lock().take());
        std::fs::create_dir(temp.path().join(".narou")).unwrap();
        std::fs::write(temp.path().join(".narou/local_setting.yaml"),
            "convert.keep-txt: false\n").unwrap();
        let record = serde_yaml::from_str(
            "id: 990001\ntitle: 監査確認\nauthor: author\nfile_title: fixture\nsitename: audit\ntoc_url: https://example.com/story\nnovel_type: 2\nlast_update: 2026-01-01T00:00:00Z\n").unwrap();
        crate::db::init_database().unwrap();
        let (record, _write) = crate::native::materialization::create_test_record(record).unwrap();
        let dir = temp.path().join("小説データ/audit/fixture");
        std::fs::create_dir_all(dir.join("本文")).unwrap();
        std::fs::create_dir(dir.join("挿絵")).unwrap();
        std::fs::write(temp.path().join("replace.txt"), "置換前\t置換後\n").unwrap();
        std::fs::write(dir.join("toc.yaml"),
            "title: 監査確認\nauthor: author\ntoc_url: https://example.com/story\nnovel_type: 2\nsubtitles:\n- index: '1'\n  href: '1'\n  subtitle: 第一話\n  file_subtitle: 第一話\n  subdate: ''\n").unwrap();
        std::fs::write(dir.join("本文/1 第一話.yaml"),
            "index: '1'\nhref: '1'\nsubtitle: 第一話\nfile_subtitle: 第一話\nsubdate: ''\nelement:\n  data_type: html\n  body: '<p>置換前</p><img src=\"https://example.com/picture.png\">'\n").unwrap();
        std::fs::write(dir.join(".illustration_cache.yaml"),
            "version: 2\nsources:\n  https://example.com/picture.png: picture.png\n").unwrap();
        let mut image = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]))
            .write_to(&mut image, image::ImageFormat::Png).unwrap();
        let illustration = image.get_ref().clone();
        std::fs::write(dir.join("挿絵/picture.png"), &illustration).unwrap();
        let mut cover_cursor = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(3, 3, image::Rgba([0, 0, 255, 255]))
            .write_to(&mut cover_cursor, image::ImageFormat::Png).unwrap();
        let cover = cover_cursor.get_ref().clone();
        std::fs::write(dir.join("cover.png"), &cover).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let objects = crate::native::materialization::read_store(temp.path()).unwrap();
        let mut reads = crate::application::novel_access::NovelReadScope::new(&objects, "epub-test");
        let (bytes, _) = runtime.block_on(super::generate_epub_on_demand(&mut reads, record.id, &record, &dir)).unwrap();
        runtime.block_on(reads.release_all()).unwrap();
        let mut epub = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut images: Vec<(String, Vec<u8>)> = Vec::new();
        let mut body_xhtml = String::new();
        let mut xhtml = String::new();
        for index in 0..epub.len() {
            let mut entry = epub.by_index(index).unwrap();
            let name = entry.name().unwrap().to_string();
            if name.starts_with("item/image/") {
                let mut data = Vec::new();
                entry.read_to_end(&mut data).unwrap();
                images.push((name, data));
            } else if name.ends_with(".xhtml") {
                let mut content = String::new();
                entry.read_to_string(&mut content).unwrap();
                if content.contains("置換後") {
                    body_xhtml = content.clone();
                }
                xhtml.push_str(&content);
            }
        }
        // Lite は画像を連番 (`item/image/0001.png` …) で格納するため、名前では
        // なく実体で照合する。保存済み挿絵と表紙の両方が EPUB に入ること。
        let names: Vec<&str> = images.iter().map(|(name, _)| name.as_str()).collect();
        assert!(
            images.iter().any(|(_, data)| data == &illustration),
            "保存済み挿絵が EPUB に無い: {names:?}"
        );
        assert!(
            images.iter().any(|(_, data)| data == &cover),
            "表紙が EPUB に無い: {names:?}"
        );
        assert!(xhtml.contains("置換後"), "{xhtml}");
        assert!(!xhtml.contains("置換前"), "{xhtml}");
        // 本文の `<img>` は格納済みの挿絵 (表紙ではない) を指す。
        let reference = body_xhtml
            .split("src=\"../image/")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_default()
            .to_string();
        assert!(!reference.is_empty(), "{body_xhtml}");
        let stored = images
            .iter()
            .find(|(name, _)| name == &format!("item/image/{reference}"))
            .map(|(_, data)| data)
            .unwrap_or_else(|| panic!("本文が参照する画像が EPUB に無い: {reference}"));
        assert_eq!(stored, &illustration, "{body_xhtml}");
        assert!(!dir.join("novel.txt").exists());
        assert!(!std::fs::read_dir(&dir).unwrap().any(|entry|
            entry.unwrap().path().extension().is_some_and(|extension| extension == "txt")));
    }

    #[cfg(feature = "lite")]
    #[test]
    fn on_demand_epub_reads_sqlite_only_named_text_and_cover() {
        use std::io::{Cursor, Read};
        let temp = tempfile::tempdir().unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(temp.path());
        let _database = DatabaseRestore(crate::db::DATABASE.lock().take());
        std::fs::create_dir(temp.path().join(".narou")).unwrap();
        std::fs::write(temp.path().join(".narou/storage-backend"), "sqlite").unwrap();
        std::fs::write(temp.path().join(".narou/local_setting.yaml"),
            "sqlite.mirror-files: false\nconvert.keep-txt: true\n").unwrap();
        let dir = temp.path().join("小説データ/audit/stored-text");
        let record: crate::db::NovelRecord = serde_yaml::from_str(
            "id: 990002\ntitle: 保管済みテキスト\nauthor: author\nfile_title: stored-text\nsitename: audit\ntoc_url: https://example.com/stored-text\nnovel_type: 2\nlast_update: 2026-01-01T00:00:00Z\n").unwrap();
        crate::db::init_database().unwrap();
        let (record, write) = crate::native::materialization::create_test_record(record).unwrap();
        let toc = crate::downloader::TocObject {
            title: record.title.clone(), author: record.author.clone(), toc_url: record.toc_url.clone(),
            story: None, subtitles: Vec::new(), novel_type: Some(2),
        };
        let mut settings = crate::converter::settings::NovelSettings::from_sources(
            Some(record.id), &record.title, &record.author, &crate::converter::ini::IniData::new(), &std::collections::HashMap::new(), false, false,
        );
        settings.archive_path = dir.clone();
        let text_path = crate::converter::output::create_output_text_path(
            &settings, record.id, &dir, &toc, Some(&record),
        );
        let root = temp.path().join("小説データ");
        let text_key = crate::native::object_store::object_key_for_file(&root, &text_path).unwrap();
        let keys = crate::platform::NovelObjectKeys::new("audit", "stored-text", false).unwrap();
        let mut cover = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(3, 3, image::Rgba([0, 0, 255, 255]))
            .write_to(&mut cover, image::ImageFormat::Png).unwrap();
        let cover = cover.into_inner();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            write.write_small(&keys.toc(), serde_yaml::to_string(&toc).unwrap().into_bytes()).await.unwrap();
            write.write_small(&keys.prefix().join("cover.png").unwrap(), cover.clone()).await.unwrap();
            write.write_small(&text_key,
                "保管済みテキスト\nauthor\n\n保存済み本文フォールバック。\n［＃挿絵（cover.png）入る］\n".as_bytes().to_vec()).await.unwrap();
        });
        assert!(!text_path.exists());
        let objects = crate::native::materialization::read_store(temp.path()).unwrap();
        let mut reads = crate::application::novel_access::NovelReadScope::new(&objects, "epub-stored-test");
        let (bytes, _) = runtime.block_on(super::generate_epub_on_demand(&mut reads, record.id, &record, &dir)).unwrap();
        runtime.block_on(reads.release_all()).unwrap();
        let mut epub = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut found_text = false;
        let mut found_cover = false;
        for index in 0..epub.len() {
            let mut entry = epub.by_index(index).unwrap();
            let name = entry.name().unwrap().to_string();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            found_cover |= name.starts_with("item/image/") && bytes == cover;
            found_text |= name.ends_with(".xhtml") &&
                String::from_utf8_lossy(&bytes).contains("保存済み本文フォールバック。");
        }
        assert!(found_text, "the stored text, not a fresh conversion, must supply the body");
        assert!(found_cover, "the SQLite-only cover must be included");
        for path in [text_path, dir.join("toc.yaml"), dir.join("cover.png")] {
            assert!(!path.exists(), "request-owned files must be removed: {}", path.display());
        }
    }
}

#[cfg(test)]
mod response_lease_tests {
    use super::*;
    use crate::platform::{NovelAccessFence, ObjectPrefix, ObjectStore};
    use futures::StreamExt;

    #[test]
    fn response_backpressure_holds_admission_until_actual_eof() {
        futures::executor::block_on(async {
            let memory = std::sync::Arc::new(crate::platform::mocks::MemoryObjectStore::new());
            let objects: std::sync::Arc<dyn ObjectStore> = memory.clone();
            let prefix = ObjectPrefix::new("novels/site/body").unwrap();
            let mut reads = crate::application::novel_access::NovelReadScope::new(&objects, "response-test");
            drop(reads.lease_for(&prefix).await.unwrap());
            let inner = futures::stream::iter([Ok::<_, NarouError>(axum::body::Bytes::from_static(b"first")), Ok(axum::body::Bytes::from_static(b"second"))]);
            let response = leased_response(Response::new(axum::body::Body::from_stream(inner)), reads);
            let response = finalize_read_response(response, &axum::http::Method::GET).await;
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 1);
            let mut body = response.into_body().into_data_stream();
            assert_eq!(body.next().await.unwrap().unwrap(), b"first"[..]);
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 1);
            assert_eq!(body.next().await.unwrap().unwrap(), b"second"[..]);
            assert!(body.next().await.is_none());
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 0);
        });
    }

    #[test]
    fn response_error_and_head_release_without_detached_cleanup() {
        futures::executor::block_on(async {
            let memory = std::sync::Arc::new(crate::platform::mocks::MemoryObjectStore::new());
            let objects: std::sync::Arc<dyn ObjectStore> = memory.clone();
            let prefix = ObjectPrefix::new("novels/site/body").unwrap();
            for head in [false, true] {
                let mut reads = crate::application::novel_access::NovelReadScope::new(&objects, "response-error-test");
                drop(reads.lease_for(&prefix).await.unwrap());
                let inner = futures::stream::once(async { Err::<axum::body::Bytes, _>(NarouError::Platform("body failed".into())) });
                let response = leased_response(Response::new(axum::body::Body::from_stream(inner)), reads);
                if head {
                    let response = finalize_read_response(response, &axum::http::Method::HEAD).await;
                    assert!(response.into_body().into_data_stream().next().await.is_none());
                } else {
                    let mut body = response.into_body().into_data_stream();
                    assert!(body.next().await.unwrap().is_err());
                }
                assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 0);
            }
        });
    }

    #[test]
    fn shared_error_mappers_preserve_access_fence_statuses_and_epub_codes() {
        for (message, expected_status, expected_code) in [
            ("novel_recovering: closed", StatusCode::SERVICE_UNAVAILABLE, "novel_recovering"),
            ("novel_lease_expired: reader", StatusCode::SERVICE_UNAVAILABLE, "novel_lease_expired"),
            ("recovery_required: journal", StatusCode::SERVICE_UNAVAILABLE, "recovery_required"),
            ("novel_busy: writer", StatusCode::LOCKED, "novel_busy"),
            ("sqlite: novel_epoch_conflict", StatusCode::CONFLICT, "novel_epoch_conflict"),
            ("gate_owner_conflict: novels/site/x is owned by remove-v1:1", StatusCode::CONFLICT, "novel_epoch_conflict"),
            ("storage unavailable", StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        ] {
            let (status, _) = map_application_error(ApplicationError::Platform(message.into()));
            assert_eq!(status, expected_status, "{message}");
            assert_eq!(epub_error_code(status, message), expected_code, "{message}");
        }
        assert_eq!(epub_error_code(StatusCode::NOT_FOUND, "ebook not found"), "not_found");
        assert_eq!(epub_error_code(StatusCode::BAD_REQUEST, "bad"), "bad_request");
        assert_eq!(epub_error_code(StatusCode::CONFLICT, "ebook moved"), "conflict");
    }
}
