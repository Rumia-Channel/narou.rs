use axum::{extract::State, response::Json};

use crate::db::with_database;
use crate::queue::{JobType, PersistentQueue};

use super::AppState;
use super::state::{ApiResponse, ConvertBody, DownloadBody, UpdateBody};

pub async fn api_download(
    State(state): State<AppState>,
    Json(body): Json<DownloadBody>,
) -> Json<serde_json::Value> {
    let targets = body.targets;
    let queue = match open_queue() {
        Ok(queue) => queue,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
                "results": []
            })
            .into();
        }
    };
    let jobs: Vec<(JobType, String)> = targets
        .iter()
        .cloned()
        .map(|target| (JobType::Download, target))
        .collect();
    let ids = match queue.push_batch(&jobs) {
        Ok(ids) => ids,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": e.to_string(),
                "results": []
            })
            .into();
        }
    };

    let results: Vec<serde_json::Value> = targets
        .iter()
        .zip(ids.iter())
        .map(|(target, job_id)| {
            state.push_server.broadcast("download_queued", target);
            serde_json::json!({ "target": target, "job_id": job_id, "status": "queued" })
        })
        .collect();

    serde_json::json!({ "success": true, "results": results }).into()
}

pub async fn api_update(
    State(state): State<AppState>,
    Json(body): Json<UpdateBody>,
) -> Json<serde_json::Value> {
    let ids: Vec<i64> = if body.all.unwrap_or(false) {
        with_database(|db| Ok(db.ids())).unwrap_or_default()
    } else if let Some(id_list) = body.ids {
        id_list
    } else {
        Vec::new()
    };

    let queue = match open_queue() {
        Ok(queue) => queue,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
                "count": 0
            })
            .into();
        }
    };
    let jobs: Vec<(JobType, String)> = ids
        .iter()
        .map(|id| (JobType::Update, id.to_string()))
        .collect();
    let job_ids = match queue.push_batch(&jobs) {
        Ok(ids) => ids,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": e.to_string(),
                "count": 0
            })
            .into();
        }
    };

    state
        .push_server
        .broadcast("update_queued", &format!("{} novels", ids.len()));
    serde_json::json!({
        "success": true,
        "status": "queued",
        "count": ids.len(),
        "job_ids": job_ids
    })
    .into()
}

pub async fn api_convert(
    State(state): State<AppState>,
    Json(body): Json<ConvertBody>,
) -> Json<serde_json::Value> {
    let device = body.device.unwrap_or_else(|| "text".to_string());
    let queue = match open_queue() {
        Ok(queue) => queue,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
                "results": []
            })
            .into();
        }
    };
    let jobs: Vec<(JobType, String)> = body
        .targets
        .iter()
        .map(|target| (JobType::Convert, format!("{}\t{}", target, device)))
        .collect();
    let ids = match queue.push_batch(&jobs) {
        Ok(ids) => ids,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": e.to_string(),
                "results": []
            })
            .into();
        }
    };

    let results: Vec<serde_json::Value> = body
        .targets
        .iter()
        .zip(ids.iter())
        .map(|(target, job_id)| {
            state.push_server.broadcast("convert_queued", target);
            serde_json::json!({
                "target": target,
                "device": device,
                "job_id": job_id,
                "status": "queued"
            })
        })
        .collect();

    serde_json::json!({ "success": true, "results": results }).into()
}

pub async fn queue_status(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let queue = match open_queue() {
        Ok(queue) => queue,
        Err(message) => {
            return Json(serde_json::json!({
                "pending": 0,
                "completed": 0,
                "failed": 0,
                "error": message,
            }));
        }
    };

    Json(serde_json::json!({
        "pending": queue.pending_count(),
        "completed": queue.completed_count(),
        "failed": queue.failed_count(),
    }))
}

pub async fn queue_clear(State(_state): State<AppState>) -> Json<ApiResponse> {
    let result = open_queue().and_then(|q| q.clear().map_err(|e| e.to_string()));

    match result {
        Ok(_) => Json(ApiResponse {
            success: true,
            message: "Queue cleared".to_string(),
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            message: e.to_string(),
        }),
    }
}

pub(crate) fn open_queue() -> Result<PersistentQueue, String> {
    PersistentQueue::with_default()
        .or_else(|_| {
            let path = std::path::PathBuf::from(".narou").join("queue.yaml");
            PersistentQueue::new(&path)
        })
        .map_err(|e| e.to_string())
}
