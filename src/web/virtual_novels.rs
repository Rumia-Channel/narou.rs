//! バーチャル小説の Web API。
//!
//! 仮想小説は `novels` の通常行なので一覧 (`/api/list`) にはそのまま出る。
//! ここでは登録・メンバー順の変更・削除を扱う。

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;

use crate::native::novel_repository::NativeNovelRepository;
use crate::native::virtual_novel::{self, NewVirtualNovel};
use crate::platform::NovelId;
use crate::virtual_novel as core;

use super::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateVirtualRequest {
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub sitename: String,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub members: Vec<serde_json::Value>,
    /// 利用者が選んだ属性タグ (小説 / 漫画)。省略可。
    #[serde(default)]
    pub attribute_tag: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReorderVirtualRequest {
    pub members: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct AddMembersRequest {
    pub members: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct ClassifyRequest {
    pub members: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct AttributeRequest {
    pub attribute_tag: String,
}

fn error_response(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({"success": false, "message": message.into()})),
    )
}

pub async fn api_virtual_list() -> Json<serde_json::Value> {
    let repository = NativeNovelRepository::new();
    match virtual_novel::load() {
        Ok(groups) => {
            // 分類は表示機能の ON / OFF とは独立に返す。
            let states = virtual_novel::attribute_states_in(&groups);
            let novels = groups
                .groups
                .iter()
                .filter_map(|(id, member_ids)| {
                    let record = repository.get_sync(NovelId(*id)).ok().flatten()?;
                    let member_titles = member_ids
                        .iter()
                        .map(|member| {
                            repository
                                .get_sync(NovelId(*member))
                                .ok()
                                .flatten()
                                .map(|record| record.title)
                                .unwrap_or_default()
                        })
                        .collect::<Vec<_>>();
                    // 分類を確定できない作品 (欠落したメンバーなど) は保存済みの
                    // 値をそのまま返し、属性は編集できないものとする。
                    let (mixed, attribute_tag) = match states.get(id) {
                        Some(state) => (state.mixed, state.attribute_tag.to_string()),
                        None => (
                            false,
                            record
                                .attribute_tag()
                                .unwrap_or(crate::application::attribute_tags::NOVEL_TAG)
                                .to_string(),
                        ),
                    };
                    Some(serde_json::json!({
                        "id": record.id,
                        "ncode": core::ncode(*id),
                        "title": record.title,
                        "author": record.author,
                        "sitename": record.sitename,
                        "domain": record.domain,
                        "tags": record.tags,
                        "members": member_ids,
                        "member_titles": member_titles,
                        "mixed": mixed,
                        "attribute_tag": attribute_tag,
                        "attribute_editable": states.contains_key(id),
                    }))
                })
                .collect::<Vec<_>>();
            Json(serde_json::json!({"success": true, "novels": novels}))
        }
        Err(error) => Json(serde_json::json!({"success": false, "message": error.to_string()})),
    }
}

pub async fn api_virtual_create(
    State(_state): State<AppState>,
    Json(body): Json<CreateVirtualRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let members = match core::parse_member_ids(&body.members) {
        Ok(members) => members,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    match virtual_novel::create(NewVirtualNovel {
        title: body.title,
        author: body.author,
        sitename: body.sitename,
        domain: body.domain,
        tags: body.tags,
        members,
        attribute: body.attribute_tag,
    }) {
        Ok(record) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "id": record.id,
                "ncode": core::ncode(record.id),
                "title": record.title,
            })),
        ),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

/// 選択中のメンバーの属性構成を調べる (登録前の下見)。
pub async fn api_virtual_classify(
    State(_state): State<AppState>,
    Json(body): Json<ClassifyRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let members = match core::parse_member_ids(&body.members) {
        Ok(members) => members,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    // 登録と同じ条件で検証する (空 / 非正 / 重複 を弾く)。
    if let Err(message) = core::validate_members(&members) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }
    match virtual_novel::composition_of(&members) {
        Ok(composition) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "mixed": composition.is_mixed(),
                "attribute_tag": match composition.resolve(None, None) {
                    Ok(tag) => tag,
                    Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
                },
            })),
        ),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

/// 混在するバーチャル小説の属性タグを変更する。
pub async fn api_virtual_attribute(
    State(_state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AttributeRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match virtual_novel::set_attribute(id, &body.attribute_tag) {
        Ok(state) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "id": id,
                "ncode": core::ncode(id),
                "mixed": state.mixed,
                "attribute_tag": state.attribute_tag,
            })),
        ),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

pub async fn api_virtual_order(
    State(_state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ReorderVirtualRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let members = match core::parse_member_ids(&body.members) {
        Ok(members) => members,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    match virtual_novel::reorder_by_ids(id, members) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"success": true})),
        ),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

pub async fn api_virtual_add_members(
    State(_state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AddMembersRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let members = match core::parse_member_ids(&body.members) {
        Ok(members) => members,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    match virtual_novel::add_members(id, members) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "id": id,
                "ncode": core::ncode(id),
            })),
        ),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

pub async fn api_virtual_convert(
    State(_state): State<AppState>,
    Path(id): Path<i64>,
) -> (StatusCode, Json<serde_json::Value>) {
    let result = tokio::task::spawn_blocking(move || -> Result<(String, String), String> {
        let record = NativeNovelRepository::new()
            .get_sync(NovelId(id))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("仮想小説 vs_{id} が見つかりません"))?;
        let members = virtual_novel::load()
            .map_err(|error| error.to_string())?
            .get(id)
            .map(|members| members.to_vec())
            .ok_or_else(|| format!("仮想小説 vs_{id} のメンバーがありません"))?;
        let path = virtual_novel::convert_virtual(id, &record, &members, None, false, false)
            .map_err(|error| error.to_string())?;
        Ok((
            core::ncode(id),
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string(),
        ))
    })
    .await;
    match result {
        Ok(Ok((ncode, filename))) => (
            StatusCode::OK,
            Json(serde_json::json!({"success": true, "ncode": ncode, "filename": filename})),
        ),
        Ok(Err(message)) => error_response(StatusCode::BAD_REQUEST, message),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "変換処理が中断されました",
        ),
    }
}

pub async fn api_virtual_remove(
    State(_state): State<AppState>,
    Path(id): Path<i64>,
) -> (StatusCode, Json<serde_json::Value>) {
    match virtual_novel::remove(id) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"success": true})),
        ),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{self, Database};
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// テスト終了時にグローバルな DB binding を戻す。
    struct DatabaseGuard(Option<Database>);

    impl Drop for DatabaseGuard {
        fn drop(&mut self) {
            *db::DATABASE.lock() = self.0.take();
        }
    }

    /// 小説 1 件と漫画 1 件を持つ隔離されたライブラリを用意する。
    fn isolated_library() -> (
        tempfile::TempDir,
        crate::test_support::CurrentDirGuard,
        DatabaseGuard,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let mut database = String::new();
        for (id, title, attribute) in [(1, "Novel1", "小説"), (2, "Manga2", "漫画")] {
            database.push_str(&format!(
                "{id}:\n  id: {id}\n  title: \"{title}\"\n  author: \"Author\"\n  file_title: \"[Author] {title}\"\n  toc_url: \"https://mock.test/novel/a{id}/\"\n  sitename: \"mock.test\"\n  novel_type: 1\n  end: false\n  last_update: \"2026-01-02T00:00:00Z\"\n  tags: [\"共通\"]\n  attribute_tag: \"{attribute}\"\n"
            ));
        }
        std::fs::write(temp.path().join(".narou/database.yaml"), database).unwrap();
        let mut slot = db::DATABASE.lock();
        let previous = slot.take();
        drop(slot);
        let guard = DatabaseGuard(previous);
        db::init_database().unwrap();
        (temp, cwd_guard, guard)
    }

    /// ハンドラは AppState を実際には使わないが、型合わせには必要。
    fn test_state() -> AppState {
        let push_server = crate::web::push::PushServer::new();
        push_server.set_accepted_domains(["127.0.0.1", "localhost"]);
        let queue_dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-artifacts")
            .join(format!("web-virtual-attribute-{}", std::process::id()));
        std::fs::create_dir_all(&queue_dir).unwrap();
        AppState {
            port: 8080,
            ws_port: 8081,
            push_server: Arc::new(push_server),
            services: crate::web::default_app_services(Arc::new(
                crate::platform::mocks::MemoryNovelRepository::new(),
            )),
            server_security: crate::web::test_server_security(
                Some("Basic dXNlcjpwYXNz"),
                vec!["127.0.0.1".to_string(), "localhost".to_string()],
                false,
            ),
            bind_host: "127.0.0.1".into(),
            control_token: "control-token".to_string(),
            ui_token: "ui-token".to_string(),
            ui_bootstrap_ticket: "bootstrap-ticket".to_string(),
            ui_bootstrap_hosts: Arc::new(parking_lot::Mutex::new(HashSet::new())),
            queue: Arc::new(
                crate::queue::PersistentQueue::new(&queue_dir.join("queue.yaml")).unwrap(),
            ),
            restore_prompt_pending: Arc::new(AtomicBool::new(false)),
            restorable_tasks_available: Arc::new(AtomicBool::new(false)),
            running_jobs: Arc::new(parking_lot::Mutex::new(Vec::new())),
            running_child_pids: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            cancelled_job_ids: Arc::new(parking_lot::Mutex::new(HashSet::new())),
            library_backup: Arc::new(crate::web::library_backup::LibraryBackupState::new()),
            auto_update_scheduler: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    #[tokio::test]
    async fn classify_reports_the_composition_of_the_selected_members() {
        let _global = crate::test_support::global_state_guard();
        let _legacy = crate::test_support::legacy_yaml_guard();
        let (_temp, _cwd_guard, _db_guard) = isolated_library();

        let (status, body) = api_virtual_classify(
            State(test_state()),
            Json(ClassifyRequest {
                members: vec![serde_json::json!(1), serde_json::json!(2)],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], serde_json::json!(true));
        assert_eq!(body["mixed"], serde_json::json!(true));
        assert_eq!(body["attribute_tag"], serde_json::json!("小説"));

        let (status, body) = api_virtual_classify(
            State(test_state()),
            Json(ClassifyRequest {
                members: vec![serde_json::json!(1)],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["mixed"], serde_json::json!(false));
        assert_eq!(body["attribute_tag"], serde_json::json!("小説"));
    }

    #[tokio::test]
    async fn classify_rejects_an_unknown_member() {
        let _global = crate::test_support::global_state_guard();
        let _legacy = crate::test_support::legacy_yaml_guard();
        let (_temp, _cwd_guard, _db_guard) = isolated_library();

        let (status, body) = api_virtual_classify(
            State(test_state()),
            Json(ClassifyRequest {
                members: vec![serde_json::json!(99)],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["success"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn classify_rejects_invalid_member_ids_like_creation_does() {
        let _global = crate::test_support::global_state_guard();
        let _legacy = crate::test_support::legacy_yaml_guard();
        let (_temp, _cwd_guard, _db_guard) = isolated_library();

        // 空・非正・重複は登録と同じ条件で弾く。
        for members in [
            vec![],
            vec![serde_json::json!(0)],
            vec![serde_json::json!(-1)],
            vec![serde_json::json!(1), serde_json::json!(1)],
        ] {
            let (status, body) = api_virtual_classify(
                State(test_state()),
                Json(ClassifyRequest { members }),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body["success"], serde_json::json!(false));
        }
    }

    #[tokio::test]
    async fn a_single_kind_virtual_novel_cannot_change_its_attribute() {
        let _global = crate::test_support::global_state_guard();
        let _legacy = crate::test_support::legacy_yaml_guard();
        let (_temp, _cwd_guard, _db_guard) = isolated_library();

        let record = virtual_novel::create(NewVirtualNovel {
            title: "Novels".into(),
            author: "Author".into(),
            sitename: "mock.test".into(),
            domain: None,
            tags: Vec::new(),
            members: vec![1],
            attribute: None,
        })
        .unwrap();
        let (status, body) = api_virtual_attribute(
            State(test_state()),
            Path(record.id),
            Json(AttributeRequest {
                attribute_tag: "漫画".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["success"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn a_mixed_virtual_novel_accepts_the_chosen_attribute() {
        let _global = crate::test_support::global_state_guard();
        let _legacy = crate::test_support::legacy_yaml_guard();
        let (_temp, _cwd_guard, _db_guard) = isolated_library();

        let record = virtual_novel::create(NewVirtualNovel {
            title: "Mixed".into(),
            author: "Author".into(),
            sitename: "mock.test".into(),
            domain: None,
            tags: Vec::new(),
            members: vec![1, 2],
            attribute: None,
        })
        .unwrap();
        let (status, body) = api_virtual_attribute(
            State(test_state()),
            Path(record.id),
            Json(AttributeRequest {
                attribute_tag: "漫画".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], serde_json::json!(true));
        assert_eq!(body["mixed"], serde_json::json!(true));
        assert_eq!(body["attribute_tag"], serde_json::json!("漫画"));
    }

    #[tokio::test]
    async fn the_list_keeps_the_stored_attribute_when_the_classification_fails() {
        let _global = crate::test_support::global_state_guard();
        let _legacy = crate::test_support::legacy_yaml_guard();
        let (_temp, _cwd_guard, _db_guard) = isolated_library();

        let record = virtual_novel::create(NewVirtualNovel {
            title: "Mixed".into(),
            author: "Author".into(),
            sitename: "mock.test".into(),
            domain: None,
            tags: Vec::new(),
            members: vec![1, 2],
            attribute: Some("漫画".into()),
        })
        .unwrap();

        // 存在しないメンバー ID に差し替え、構成から分類を確定できない状態にする。
        {
            let repository = crate::native::novel_repository::NativeNovelRepository::new();
            repository
                .update_existing_sync(NovelId(record.id), |record| {
                    record.set_attribute_tag("漫画");
                    Ok(())
                })
                .unwrap();
            let mut members = crate::native::virtual_novel::load().unwrap();
            members.remove(record.id);
            members.insert(record.id, vec![99]).unwrap();
            crate::native::virtual_novel::save(&members).unwrap();
        }

        let body = api_virtual_list().await .0;
        let novels = body["novels"].as_array().unwrap();
        let entry = novels
            .iter()
            .find(|entry| entry["id"].as_i64() == Some(record.id))
            .expect("the virtual novel must be listed");
        // 分類を確定できないときは保存済みの値を偽装せず、編集不可を示す。
        assert_eq!(entry["attribute_tag"], serde_json::json!("漫画"));
        assert_eq!(entry["attribute_editable"], serde_json::json!(false));
    }
}

