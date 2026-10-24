use axum::{
    extract::{Form, Path, Query, State},
    http::StatusCode,
    response::{Json, Response},
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

fn map_application_error(error: ApplicationError) -> (StatusCode, String) {
    match error {
        ApplicationError::InvalidRequest(message) => (StatusCode::BAD_REQUEST, message),
        ApplicationError::Conflict(message) => (StatusCode::CONFLICT, message),
        ApplicationError::NotFound(message) => (StatusCode::NOT_FOUND, message),
        ApplicationError::Platform(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
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
    let page = state
        .services
        .library
        .list(&LibraryListRequest {
            search,
            start,
            length,
            sort_column,
            sort_order,
            hidden: Some(crate::native::hidden::load().unwrap_or_default()),
            include_hidden: params.show_hidden.unwrap_or(false),
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
    }))
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
        crate::application::webui::mutations::MutationNotice::TableReload,
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
        crate::application::webui::mutations::MutationNotice::TableReload,
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

    let mut sections: Vec<Option<crate::downloader::types::SectionFile>> =
        Vec::with_capacity(toc.subtitles.len());
    for sub in &toc.subtitles {
        let Some(bytes) = state
            .services
            .content
            .section(id.into(), &sub.index, &sub.file_subtitle)
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
    ))
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

    let existing = find_existing(&paths).or_else(|| {
        if ext != ".epub" {
            crate::mail::get_ebook_file_paths(&record, &novel_dir, ".epub")
                .ok()
                .and_then(|eps| find_existing(&eps))
        } else {
            None
        }
    });

    #[cfg(feature = "lite")]
    let file_path = match existing {
        Some(path) => path,
        None => {
            let context = crate::application::messages::MessageContext::new(
                state.push_server.clone(), crate::compat::load_local_setting_bool("webui.debug-mode"),
            );
            let (bytes, filename) = context.scope(generate_epub_on_demand(id, &record, &novel_dir)).await?;
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

/// Build an EPUB from the converted 青空文庫 text at request time
/// (`lite` feature). Reads the same output txt the CLI converter writes and
/// resolves illustrations from the novel's `挿絵/` directory.
#[cfg(feature = "lite")]
async fn generate_epub_on_demand(
    id: i64,
    record: &crate::db::novel_record::NovelRecord,
    novel_dir: &std::path::Path,
) -> Result<(Vec<u8>, String), (StatusCode, String)> {
    let inventory = crate::db::inventory::Inventory::with_default_root()
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    // Restore inputs before loading native settings (including setting.ini and
    // the per-novel/global replace.txt), and images before localizing HTML.
    let _files = crate::native::object_store::NativeStore::for_narou_root(inventory.root_dir())
        .and_then(|store| store.materialize_novel_files(novel_dir))
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let _materialized = crate::native::illustrations::materialize(novel_dir).await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    // 保存済み EPUB と同じ命名規則で Content-Disposition 名を決める
    // (変換時の txt basename に `.epub` を付けたもの)。
    // `convert.filename` / `convert.filename-to-ncode` は
    // `NovelSettings::load_for_novel` と `OutputNamingEnv` が反映する。
    let settings = crate::converter::settings::NovelSettings::load_for_novel(
        id,
        &record.title,
        &record.author,
        novel_dir,
    );
    let filename = {
        // 命名には TocObject の title/author/toc_url だけが使われるため、
        // toc.yaml を読まずに record から組み立てる。
        let naming_toc = crate::downloader::TocObject {
            title: record.title.clone(),
            author: record.author.clone(),
            toc_url: record.toc_url.clone(),
            story: None,
            subtitles: Vec::new(),
            novel_type: Some(record.novel_type),
        };
        crate::converter::output::epub_output_filename(
            &settings,
            &naming_toc,
            Some(record),
            &crate::converter::output::local_output_naming_env(),
        )
    };

    // P4a: prefer the SQLite mirror of the converted text when present.
    // `convert.keep-txt=false` では変換済みテキストを保存しない構成なので、
    // 残っていても読まずに本文から組み立て直す (Worker の download.epub と
    // 同じ扱い)。
    let keep_text = crate::converter::keep_converted_text_file();
    let cached_text = if keep_text
        && !crate::native::sqlite::state::legacy_yaml_active()
        && let Some(narou_dir) = crate::db::inventory::Inventory::with_default_root()
            .ok()
            .map(|inventory| inventory.root_dir().join(".narou"))
        && let Some(state) = crate::native::sqlite::state::active_for(&narou_dir)
        && let Some(payload) = {
            let conn = state.conn_ref();
            crate::native::sqlite::content::load_output(&conn.lock(), id, "converted_text")
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        }
        && !payload.is_empty()
    {
        Some(payload)
    } else {
        None
    };
    // 変換済みテキストを保存しない構成では本文から組み立てた novel.txt を
    // 使い、命名は record 由来の filename (fast path と同じ) を保つ。
    // 保存する構成は従来どおり toc.yaml から命名・txt パスを解決する。
    let mut filename = filename;
    let toc_object = if keep_text && cached_text.is_none() {
        let toc_content = std::fs::read_to_string(novel_dir.join("toc.yaml"))
            .map_err(|e| (StatusCode::CONFLICT, format!("toc.yaml: {e}")))?;
        let toc: crate::downloader::TocFile = serde_yaml::from_str(&toc_content)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("toc.yaml: {e}")))?;
        let toc_object = crate::downloader::TocObject {
            title: toc.title,
            author: toc.author,
            toc_url: toc.toc_url,
            story: toc.story,
            subtitles: toc.subtitles,
            novel_type: toc.novel_type,
        };
        // 保存済み EPUB と同じ名: 実際に生成される txt の basename に `.epub`
        // を付けたもの (native `device.rs` の `{file_stem(txt)}.epub` 経路と同じ規則)。
        filename = crate::converter::output::epub_output_filename(
            &settings,
            &toc_object,
            Some(record),
            &crate::converter::output::local_output_naming_env(),
        );
        Some(toc_object)
    } else {
        None
    };
    // 変換済みテキストを保存しない構成では保存済みの本文からその都度
    // 変換する。build_book はファイルを要求するため、材料を novel_dir へ
    // 取り出し、組み立てた txt は消えるガードとして書き出す。
    let (txt_path, _text_guard) = if let Some(toc_object) = &toc_object {
        (
            crate::converter::output::create_output_text_path(
                &settings,
                id,
                novel_dir,
                toc_object,
                Some(record),
            ),
            None,
        )
    } else {
        let text = if let Some(payload) = cached_text {
            // `converted_text` contains UTF-8 text, never EPUB bytes.
            String::from_utf8(payload)
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        } else {
            let record = record.clone();
            let novel_dir = novel_dir.to_path_buf();
            let settings = settings.clone();
            let message_context = crate::application::messages::MessageContext::current();
            tokio::task::spawn_blocking(move || {
                let convert = || {
                let toc: crate::downloader::TocFile =
                    serde_yaml::from_str(&std::fs::read_to_string(novel_dir.join("toc.yaml"))?)?;
                let sections = crate::converter::load_sections_from_dir(&novel_dir, &toc.subtitles)?;
                let user_converter = crate::converter::user_converter::UserConverter::load_with_title(
                    &novel_dir, &settings.title_for_output(&record.title),
                );
                let mut converter = match user_converter {
                    Some(user) => crate::converter::NovelConverter::with_user_converter(settings, user),
                    None => crate::converter::NovelConverter::new(settings),
                };
                converter.attach_store_illustration_storage(&novel_dir);
                converter.apply_record_fetch_policy(Some(&record));
                let toc = crate::downloader::TocObject {
                    title: toc.title, author: toc.author, toc_url: toc.toc_url,
                    story: toc.story, subtitles: toc.subtitles, novel_type: toc.novel_type,
                };
                converter.convert_novel(&toc, &sections)
                };
                match message_context {
                    Some(context) => context.run(convert),
                    None => convert(),
                }
            })
            .await
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
            .map_err(|error| (StatusCode::CONFLICT, format!("convert: {error}")))?
        };
        std::fs::create_dir_all(novel_dir)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
        // The request owns only this temporary file; never overwrite or delete
        // an existing novel.txt, even on an EPUB build failure.
        let mut temporary = tempfile::Builder::new().prefix(".narou-epub-").suffix(".txt")
            .tempfile_in(novel_dir)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
        std::io::Write::write_all(&mut temporary, text.as_bytes())
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
        (temporary.path().to_path_buf(), Some(temporary))
    };
    if !txt_path.is_file() {
        let message = if keep_text {
            "Converted text not found: run convert first"
        } else {
            "Converted text could not be produced"
        };
        return Err((StatusCode::CONFLICT, message.to_string()));
    }
    // Conversion can fetch images that were absent from the pre-build snapshot.
    let _new_images = crate::native::illustrations::materialize(novel_dir).await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;

    // 本文の濁点注記の有無で濁点フォント (DMincho) を埋め込む — native CLI
    // (`file_contains_dakuten_chuki`) と Worker と同じ auto 判定。保存済み EPUB を
    // 使わないこの経路は、濁点フォントが無いと本文の濁点注記を再現できない。
    let use_dakuten_font = std::fs::read_to_string(&txt_path)
        .is_ok_and(|text| crate::epub_lite::text_has_dakuten_marker(&text));
    // 保存済み設定から組立オプションを組む (縦書き・表紙の規則は native Web /
    // Worker 共通。表紙候補は cover.{jpg,png,jpeg} の実在で判定する)。
    let options = crate::epub_lite::EpubBuildOptions::from_settings(
        crate::epub_lite::EpubSettingsInput {
            title: &record.title,
            author: &record.author,
            settings: &settings,
            has_images: txt_path
                .parent()
                .and_then(crate::converter::render::cover_image_path)
                .is_some(),
            // Java 版と同じ資産 (注記表・外字フォント・AozoraEpub3.ini) を読ませる。
            extra_assets: crate::converter::dakuten_font::lite_font_assets(use_dakuten_font)
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?,
            assets_dir: crate::compat::aozora_assets_dir(),
            kindle: false,
            rotate_image: crate::compat::load_local_setting_string("convert.rotate-image")
                .and_then(|value| crate::converter::settings::ImageRotation::from_setting(&value)),
        },
    );
    let build_path = txt_path.clone();
    let message_context = crate::application::messages::MessageContext::current();
    let build = tokio::task::spawn_blocking(move || {
        let build = || crate::epub_lite::build_book(&build_path, &options);
        match message_context {
            Some(context) => context.run(build),
            None => build(),
        }
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut bytes = Vec::new();
    crate::epub_lite::stream_epub(&build.book, &mut bytes, |epub_path| build.resolve(epub_path))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;


    Ok((bytes, filename))
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
        std::fs::create_dir(temp.path().join(".narou")).unwrap();
        std::fs::write(temp.path().join(".narou/local_setting.yaml"),
            "convert.keep-txt: false\n").unwrap();
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
        let record = serde_yaml::from_str(
            "id: 990001\ntitle: 監査確認\nauthor: author\nfile_title: fixture\nsitename: audit\ntoc_url: https://example.com/story\nnovel_type: 2\nlast_update: 2026-01-01T00:00:00Z\n").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (bytes, _) = runtime.block_on(super::generate_epub_on_demand(990001, &record, &dir)).unwrap();
        let mut epub = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut images: Vec<(String, Vec<u8>)> = Vec::new();
        let mut body_xhtml = String::new();
        let mut xhtml = String::new();
        for index in 0..epub.len() {
            let mut entry = epub.by_index(index).unwrap();
            let name = entry.name().to_string();
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
}
