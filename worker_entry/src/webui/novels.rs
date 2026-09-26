//! 小説 1 件を対象にする JSON API (native: `src/web/novels.rs` /
//! `src/web/tags.rs` / `src/web/batch.rs`)。
//!
//! - `GET  /api/novels/{id}/author_comments` — native `author_comments`
//!   (TOC + 各セクションのまえがき/あとがきを集計して返す)。
//! - `POST /api/novels/{id}/freeze` / `unfreeze` — native `freeze_novel` /
//!   `unfreeze_novel`。
//! - `DELETE /api/novels/{id}` — native `remove_novel`
//!   (`with_file` は任意 JSON 本文、省略時 false)。
//! - `POST /api/novels/{id}/tag` / `DELETE /api/novels/{id}/tag` —
//!   native `add_tag` / `remove_tag`。
//! - `POST /api/novels/{id}/tags` / `PUT` / `POST /api/novels/{id}/tags/remove`
//!   — native `add_tags` / `update_tags` / `remove_tags`。
//! - `POST /api/novels/tag` / `DELETE /api/novels/tag` —
//!   native `batch_tag` / `batch_untag` (本文は `[BatchIdsBody, TagBody]` の
//!   2 要素配列)。
//!
//! エラー応答は Worker 共通の `json_error` 形 (native は `(Status, String)`
//! の文字列ボディだが、非 2xx をそのまま通知に表示するフロントには同じ
//! メッセージが伝わる)。成功応答の `{success, message}` は native の
//! `ApiResponse` と同じ。
//!
//! native の `push_server.broadcast_event("table.reload", …)` は Worker に
//! 共有 broadcast 経路が無いので送れない (row_actions.rs と同じ判断)。

use narou_rs::application::webui::{
    MAX_WEB_TAGS_PER_REQUEST, normalize_web_tag_name, sort_ids_from_records,
};
use narou_rs::application::{
    AppServices, FileDeletionStatus, RemoveRequest, TagAction, TagChangeRequest,
};
use narou_rs::platform::NovelId;
use serde::Deserialize;
use serde_json::json;
use worker::{Request, Response};

use super::{api_response, application_error_response, json_error};

// ---------------------------------------------------------------------------
// GET /api/novels/{id}/author_comments (native: novels.rs author_comments)
// ---------------------------------------------------------------------------

pub async fn author_comments(services: &AppServices, id: NovelId) -> worker::Result<Response> {
    let record = match services.library.get(id).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return json_error(404, "not_found", Some(&format!("ID: {}", id.0)));
        }
        Err(error) => return application_error_response(&error),
    };
    // native は TOC なしを 404 ("TOC not found")、壊れた YAML を 500 にする。
    let toc = match services.content.toc(id).await {
        Ok(toc) => toc,
        Err(error) => return application_error_response(&error),
    };
    let Some(bytes) = toc else {
        return json_error(404, "not_found", Some("TOC not found"));
    };
    let toc = match serde_yaml::from_slice::<narou_rs::downloader::types::TocObject>(&bytes) {
        Ok(toc) => toc,
        Err(error) => {
            return json_error(500, "internal_error", Some(&error.to_string()));
        }
    };

    let mut comments = Vec::new();
    let mut introductions_count: usize = 0;
    let mut postscripts_count: usize = 0;

    for sub in &toc.subtitles {
        let Some(bytes) = (match services
            .content
            .section(id, &sub.index, &sub.file_subtitle)
            .await
        {
            Ok(bytes) => bytes,
            Err(error) => return application_error_response(&error),
        }) else {
            continue;
        };
        let Ok(sf) = serde_yaml::from_slice::<narou_rs::downloader::types::SectionFile>(&bytes)
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
                narou_rs::downloader::html::to_aozora_strip_decoration(&sf.element.introduction),
                narou_rs::downloader::html::to_aozora_strip_decoration(&sf.element.postscript),
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

        comments.push(json!({
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

    Response::from_json(&json!({
        "title": record.title,
        "introductions_ratio": introductions_ratio,
        "postscripts_ratio": postscripts_ratio,
        "comments": comments,
    }))
}

// ---------------------------------------------------------------------------
// POST /api/novels/{id}/freeze|unfreeze (native: novels.rs freeze/unfreeze_novel)
// ---------------------------------------------------------------------------

pub async fn freeze_novel(services: &AppServices, id: NovelId) -> worker::Result<Response> {
    freeze_or_unfreeze(services, id, true).await
}

pub async fn unfreeze_novel(services: &AppServices, id: NovelId) -> worker::Result<Response> {
    freeze_or_unfreeze(services, id, false).await
}

async fn freeze_or_unfreeze(
    services: &AppServices,
    id: NovelId,
    freeze: bool,
) -> worker::Result<Response> {
    let result = match if freeze {
        services.novel_actions.freeze(&[id]).await
    } else {
        services.novel_actions.unfreeze(&[id]).await
    } {
        Ok(result) => result,
        Err(error) => return application_error_response(&error),
    };
    if !result.missing.is_empty() {
        return json_error(404, "not_found", Some(&format!("ID: {}", id.0)));
    }
    let persisted = result.store_failed.is_empty();
    let verb = if freeze { "Froze" } else { "Unfroze" };
    let message = if persisted {
        format!("{verb} {}", id.0)
    } else {
        format!("{verb} {} (freeze state persistence failed)", id.0)
    };
    Response::from_json(&api_response(persisted, message))
}

// ---------------------------------------------------------------------------
// DELETE /api/novels/{id} (native: novels.rs remove_novel)
// ---------------------------------------------------------------------------

pub async fn remove_novel(
    services: &AppServices,
    id: NovelId,
    mut req: Request,
) -> worker::Result<Response> {
    // native は `Option<Json<Value>>`: 空本文は None (= with_file:false)、
    // 壊れた JSON は 400。こちらも同じ受理範囲にする。
    let with_file = match req.text().await {
        Ok(text) if text.trim().is_empty() => false,
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(body) => body
                .get("with_file")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
        },
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let result = match services
        .novel_actions
        .remove(&RemoveRequest {
            ids: vec![id],
            delete_files: with_file,
        })
        .await
    {
        Ok(result) => result,
        Err(error) => return application_error_response(&error),
    };
    if !result.missing.is_empty() {
        return json_error(404, "not_found", Some(&format!("ID: {}", id.0)));
    }
    let files_ok = result
        .files
        .iter()
        .all(|(_, status)| matches!(status, FileDeletionStatus::Deleted));
    let message = if files_ok {
        format!("Removed {}", id.0)
    } else {
        format!("Removed {} (file deletion incomplete)", id.0)
    };
    Response::from_json(&api_response(files_ok, message))
}

// ---------------------------------------------------------------------------
// POST/DELETE /api/novels/{id}/tag, POST/PUT /api/novels/{id}/tags,
// POST /api/novels/{id}/tags/remove (native: tags.rs)


/// native `TagBody` (`src/web/state.rs`) と同じ受理形。
#[derive(Debug, Deserialize)]
struct TagBody {
    tag: String,
}

/// native `TagsBody` (`src/web/state.rs`) と同じ受理形。
#[derive(Debug, Deserialize)]
struct TagsBody {
    tags: Vec<String>,
}

async fn apply_tag_change(
    services: &AppServices,
    ids: &[i64],
    action: TagAction,
    tags: Vec<String>,
) -> Result<narou_rs::application::TagChangeResult, worker::Result<Response>> {
    services
        .novel_actions
        .change_tags(&TagChangeRequest {
            ids: ids.iter().copied().map(NovelId::from).collect(),
            action,
            tags,
        })
        .await
        .map_err(|error| application_error_response(&error))
}

/// `{id}/tag` (POST=add_tag / DELETE=remove_tag)。`tag` 1 件を対象にする。
pub async fn novel_tag(
    services: &AppServices,
    id: NovelId,
    mut req: Request,
    remove: bool,
) -> worker::Result<Response> {
    let body: TagBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let tag = match normalize_web_tag_name(&body.tag) {
        Ok(tag) => tag,
        Err(message) => return json_error(400, "bad_request", Some(&message)),
    };
    let action = if remove { TagAction::Remove } else { TagAction::Add };
    let result = match apply_tag_change(services, &[id.0], action, vec![tag]).await {
        Ok(result) => result,
        Err(response) => return response,
    };
    if !result.missing.is_empty() {
        return json_error(404, "not_found", Some(&format!("ID: {}", id.0)));
    }
    let message = if remove { "Tag removed" } else { "Tag added" };
    Response::from_json(&api_response(true, message))
}

/// `{id}/tags` (POST=add_tags / PUT=update_tags) と `{id}/tags/remove` (POST)。
pub async fn novel_tags(
    services: &AppServices,
    id: NovelId,
    mut req: Request,
    action: TagAction,
) -> worker::Result<Response> {
    let body: TagsBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let tags = match validate_tags(&body.tags) {
        Ok(tags) => tags,
        Err(message) => return json_error(400, "bad_request", Some(&message)),
    };
    let result = match apply_tag_change(services, &[id.0], action, tags).await {
        Ok(result) => result,
        Err(response) => return response,
    };
    if !result.missing.is_empty() {
        return json_error(404, "not_found", Some(&format!("ID: {}", id.0)));
    }
    let message = match action {
        TagAction::Add => "Tags added",
        TagAction::Remove => "Tags removed",
        TagAction::Replace => "Tags updated",
    };
    Response::from_json(&api_response(true, message))
}

/// native `validate_tags` (`src/web/tags.rs:12`): 件数上限と正規化。
fn validate_tags(tags: &[String]) -> Result<Vec<String>, String> {
    if tags.len() > MAX_WEB_TAGS_PER_REQUEST {
        return Err("too many tags".to_string());
    }
    tags.iter().map(|tag| normalize_web_tag_name(tag)).collect()
}

// ---------------------------------------------------------------------------
// POST/DELETE /api/novels/tag (native: batch.rs batch_tag / batch_untag)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct BatchIdsBody {
    ids: Vec<i64>,
    #[serde(default)]
    #[allow(dead_code)]
    sort_state: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    timestamp: Option<u64>,
}

/// `POST /api/novels/tag` (追加) / `DELETE /api/novels/tag` (削除)。
/// 本文は native と同じ `[{"ids":[…], …}, {"tag":"…"}]` の 2 要素配列。
pub async fn batch_tag(
    services: &AppServices,
    mut req: Request,
    remove: bool,
) -> worker::Result<Response> {
    // axum の `Json<(BatchIdsBody, TagBody)>` と同じ受理形 (2 要素の配列)。
    let body: (BatchIdsBody, TagBody) = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let (ids_body, tag_body) = body;
    if ids_body.ids.len() > super::max_web_targets_for(services).await {
        return json_error(400, "bad_request", Some("too many ids"));
    }
    let ids = sort_ids_for_request(services, &ids_body.ids).await;
    let tag = match normalize_web_tag_name(&tag_body.tag) {
        Ok(tag) => tag,
        Err(message) => return json_error(400, "bad_request", Some(&message)),
    };
    let action = if remove { TagAction::Remove } else { TagAction::Add };
    let result = match apply_tag_change(services, &ids, action, vec![tag]).await {
        Ok(result) => result,
        Err(response) => return response,
    };
    if !result.missing.is_empty() {
        return json_error(
            404,
            "not_found",
            Some(&format!("ID: {}", result.missing[0].0)),
        );
    }
    let verb = if remove { "Untagged" } else { "Tagged" };
    Response::from_json(&api_response(
        true,
        format!("{verb} {} novels", ids.len()),
    ))
}

/// native `sort_ids_for_request`: リクエストの sort_state/timestamp は参照
/// せず、サーバー保存の現在ソートで並べ直す (row_actions.rs と同じ規則)。
async fn sort_ids_for_request(services: &AppServices, ids: &[i64]) -> Vec<i64> {
    let records = services.library.records().await.unwrap_or_default();
    let sort_state = super::load_current_sort_state_for(services).await;
    sort_ids_from_records(ids, &records, &sort_state)
}
