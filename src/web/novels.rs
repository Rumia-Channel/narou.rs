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
            let (bytes, filename) = generate_epub_on_demand(id, &record, &novel_dir).await?;
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

/// Build an EPUB from the converted 青空文庫 text at request time
/// (`lite` feature). Reads the same output txt the CLI converter writes and
/// resolves illustrations from the novel's `挿絵/` directory.
#[cfg(feature = "lite")]
async fn generate_epub_on_demand(
    id: i64,
    record: &crate::db::novel_record::NovelRecord,
    novel_dir: &std::path::Path,
) -> Result<(Vec<u8>, String), (StatusCode, String)> {
    // `webui.debug-mode` なら挿絵の解決状況などを Web コンソールへ流す。
    crate::application::debug::set_enabled(crate::compat::load_local_setting_bool(
        "webui.debug-mode",
    ));
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
    if crate::converter::keep_converted_text_file()
        && !crate::native::sqlite::state::legacy_yaml_active()
        && let Some(narou_dir) = crate::db::inventory::Inventory::with_default_root()
            .ok()
            .map(|inventory| inventory.root_dir().join(".narou"))
        && let Some(state) = crate::native::sqlite::state::active_for(&narou_dir)
        && let Some(payload) = {
            let conn = state.conn_ref();
            conn.lock()
                .expect("sqlite mutex poisoned")
                .query_row(
                    "SELECT payload FROM novel_outputs WHERE novel_id = ? AND kind = 'converted_text'",
                    rusqlite::params![id],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .ok()
        }
        && !payload.is_empty()
    {
        return Ok((payload, filename));
    }

    let keep_text = crate::converter::keep_converted_text_file();
    // 変換済みテキストを保存しない構成では本文から組み立てた novel.txt を
    // 使い、命名は record 由来の filename (fast path と同じ) を保つ。
    // 保存する構成は従来どおり toc.yaml から命名・txt パスを解決する。
    let mut filename = filename;
    let toc_object = if keep_text {
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
    let (txt_path, _conversion_guard) = if let Some(toc_object) = &toc_object {
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
        let stores = crate::native::object_store::NativeStores::for_current_root()
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
        let narou_root = novel_dir
            .ancestors()
            .find(|candidate| candidate.join(".narou").is_dir())
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let files =
            crate::native::object_store::NativeStore::for_narou_root(&narou_root)
                .and_then(|store| store.materialize_novel_files(novel_dir))
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
        let inventory = crate::db::inventory::Inventory::with_default_root()
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
        let service = std::sync::Arc::new(crate::application::convert::ConvertService::new(
            stores.objects.clone(),
            stores.assets.clone(),
            std::sync::Arc::new(
                crate::native::application::NativeSettingsStore::new(
                    std::sync::Arc::new(inventory),
                ),
            ),
        ));
        // NovelConverter が内部に Rc を持つため非 Send。スレッド内で block_on して
        // そのスレッド上で完結させる (結果の String は Send)。
        let record = record.clone();
        let text = tokio::task::spawn_blocking(move || {
            futures::executor::block_on(service.convert_only(&record))
                .map(|converted| converted.text)
        })
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .map_err(|error| (StatusCode::CONFLICT, format!("convert: {error}")))?;
        let on_demand_txt = novel_dir.join("novel.txt");
        let _ = std::fs::create_dir_all(novel_dir);
        std::fs::write(&on_demand_txt, text.as_bytes())
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
        (
            on_demand_txt.clone(),
            Some((
                files,
                crate::native::object_store::MaterializedNovelFiles::from_paths(vec![
                    on_demand_txt,
                ]),
            )),
        )
    };
    if !txt_path.is_file() {
        let message = if keep_text {
            "Converted text not found: run convert first"
        } else {
            "Converted text could not be produced"
        };
        return Err((StatusCode::CONFLICT, message.to_string()));
    }

    // 挿絵を S3 に置く構成ではローカルに実体が無い。EPUB 生成はファイルを
    // 要求するので、ここで取り出し、スコープを抜けた時点で片付ける。
    let _materialized = crate::native::illustrations::Materialized::new(
        crate::native::illustrations::materialize(novel_dir)
            .await
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?,
    );
    let options = crate::epub_lite::EpubBuildOptions {
        title: record.title.clone(),
        author: record.author.clone(),
        vertical: !settings.enable_yokogaki,
        cover_from_first_image: settings.enable_illust
            && txt_path.parent().is_some_and(|dir| {
                [".jpg", ".png", ".jpeg"]
                    .iter()
                    .any(|ext| dir.join(format!("cover{ext}")).is_file())
            }),
        // Java 版と同じ資産 (注記表・外字フォント・AozoraEpub3.ini) を読ませる。
        assets_dir: crate::compat::aozora_assets_dir(),
        kindle: false,
        extra_assets: crate::converter::dakuten_font::lite_font_assets(false)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?,
        rotate_image: crate::compat::load_local_setting_string("convert.rotate-image")
            .and_then(|value| crate::converter::settings::ImageRotation::from_setting(&value)),
    };
    let build_path = txt_path.clone();
    let build = tokio::task::spawn_blocking(move || {
        crate::epub_lite::build_book(&build_path, &options)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut bytes = Vec::new();
    crate::epub_lite::stream_epub(&build.book, &mut bytes, |epub_path| build.resolve(epub_path))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // keep=false + 実ファイル無しの構成で作った空の novel_dir を片付ける
    // (中身があれば失敗してそのまま残る)。
    if !keep_text {
        let _ = std::fs::remove_dir(novel_dir);
    }

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
}
