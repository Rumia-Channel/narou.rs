//! Web UI のジョブ投入系エンドポイント (native: `src/web/jobs.rs` の
//! `api_update` / `api_convert` / `api_update_by_tag` /
//! `api_update_general_lastup`、`src/web/update.rs` の `api_update_start`)。
//!
//! `/api/download_request` は native のルート表 (`src/web/mod.rs`) に存在
//! しない (削除済み・WEBUI.md §6.10) ので、Worker でも実装しない ——
//! native と同じく 404 のまま。
//!
//! native は `PersistentQueue` に 1 リクエスト = 1 ジョブ (タブ区切りの複合
//! ターゲット) を積み、解決・実行は子プロセスに委譲する。Worker の ledger
//! (`worker_jobs`) は 1 ジョブ = 1 `JobTarget` しか持てず実行も消費側で
//! 行うので、投入時に native CLI (`narou update` / `narou convert`) が
//! 子プロセス内でやる展開・解決をここで済ませ、対象ごとに
//! `WorkerRuntime::enqueue_plan` で discrete なジョブを積む。
//!
//! 対象解決は `webui/download.rs` と同じ手順 (エイリアス → id / ncode / URL →
//! タイトル / ncode 検索)。別名表の読み込みは `webui::download::load_aliases`、
//! 解決自体は可搬層 `narou_rs::application::aliases` を共有する。
//!
//! native との JSON 一致の要点:
//! - 成功/失敗はいずれも HTTP 200。失敗は `{success:false, message, ...}`
//!   で返し、4xx/5xx は HTTP 層の誤り (壊れた JSON・メソッド違い等) に限る。
//! - `POST /api/update_general_lastup` — `narou update --gl` は小説本体を
//!   落とさず general_lastup だけを舐める専用ジョブで、Worker の
//!   `JobKind::Update` (本体を DL する更新) では表現できない。実行不能な
//!   操作を成功に見せないため、native の option 検証までは踏襲し、
//!   受理可能な要求には 501 + `not_supported_on_worker` を返す
//!   (`webui/native_only.rs` と同じ code)。
//! - `POST /api/update/start` — 同梱 updater のダウンロードとプロセス再起動
//!   は Worker では原理的に不可能。同じく 501 + `not_supported_on_worker`。

use std::collections::HashMap;
use std::collections::HashSet;
use narou_rs::application::webui::{
    MAX_WEB_TAGS_PER_REQUEST, normalize_web_device_override,
    sort_ids_from_records, sort_records, targets_to_strings, validate_web_tag_name,
    validate_web_target_value,
};
use narou_rs::application::aliases::{alias_to_target_for_update, resolve_alias_target};
use narou_rs::application::{JobKind, JobPlan, JobTarget};
use narou_rs::db::NovelRecord;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::downloader::{Downloader, TargetType};
use narou_rs::platform::NovelId;
use serde::Deserialize;
use serde_json::json;
use worker::{Env, Method, Request, Response, console_log};

use crate::composition::WorkerRuntime;

use super::{json_error, load_current_sort_state};

/// native `UpdateBody` (`src/web/state.rs`) と同じ受理形。
#[derive(Debug, Deserialize)]
struct UpdateBody {
    #[serde(default)]
    targets: Vec<serde_json::Value>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    update_all: bool,
    /// native はリクエスト由来のソート状態を信用しない
    /// (`request_sort_state` は常に None を返す)。受理だけはする。
    #[serde(default)]
    #[allow(dead_code)]
    sort_state: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    timestamp: Option<u64>,
}

/// native `ConvertBody` と同じ受理形。
#[derive(Debug, Deserialize)]
struct ConvertBody {
    targets: Vec<String>,
    device: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    sort_state: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    timestamp: Option<u64>,
}

/// native `UpdateByTagBody` と同じ受理形。
#[derive(Debug, Deserialize)]
struct UpdateByTagBody {
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    exclusion_tags: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    sort_state: Option<serde_json::Value>,
    #[allow(dead_code)]
    timestamp: Option<u64>,
}

/// native `UpdateStartBody` (`src/web/update.rs`) と同じ受理形。
/// 受理だけして 501 を返す。
#[derive(Debug, Deserialize, Default)]
struct UpdateStartBody {
    #[serde(default)]
    #[allow(dead_code)]
    asset_url: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    variant: Option<String>,
}

/// Entry point; `lib.rs` routes the five POST endpoints here.
pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    #[derive(Clone, Copy)]
    enum Route {
        Convert,
        Update,
        UpdateStart,
        UpdateByTag,
        UpdateGeneralLastup,
    }
    let path = req.path();
    let route = match (req.method(), path.as_str()) {
        (Method::Post, "/api/convert") => Route::Convert,
        (Method::Post, "/api/update") => Route::Update,
        (Method::Post, "/api/update/start") => Route::UpdateStart,
        (Method::Post, "/api/update_by_tag") => Route::UpdateByTag,
        (Method::Post, "/api/update_general_lastup") => Route::UpdateGeneralLastup,
        (
            _,
            "/api/convert"
            | "/api/update"
            | "/api/update/start"
            | "/api/update_by_tag"
            | "/api/update_general_lastup",
        ) => return json_error(405, "method_not_allowed", None),
        _ => {
            return json_error(404, "not_found", Some("route is not handled by this Worker"));
        }
    };

    // 実行不能ルートは runtime を組み立てる前に返す (native_only.rs と同じ)。
    match route {
        Route::UpdateStart => return api_update_start(&mut req).await,
        Route::UpdateGeneralLastup => return api_update_general_lastup(&mut req).await,
        _ => {}
    }

    let runtime = match WorkerRuntime::build(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return json_error(503, "service_unavailable", None);
        }
    };

    match route {
        Route::Convert => api_convert(&mut req, &env, &runtime).await,
        Route::Update => api_update(&mut req, &env, &runtime).await,
        Route::UpdateByTag => api_update_by_tag(&mut req, &runtime).await,
        Route::UpdateStart | Route::UpdateGeneralLastup => unreachable!(),
    }
}

/// native `api_update` の失敗応答: HTTP 200 + `{success:false, message,
/// count:0}`。
fn update_failure(message: &str) -> worker::Result<Response> {
    Response::from_json(&json!({
        "success": false,
        "message": message,
        "count": 0,
    }))
}

/// native `api_convert` の失敗応答: HTTP 200 + `{success:false, message,
/// results:[]}`。
fn convert_failure(message: &str) -> worker::Result<Response> {
    Response::from_json(&json!({
        "success": false,
        "message": message,
        "results": [],
    }))
}

/// 実行不能ルートの 501。`native_only.rs` と同じ
/// `error.code = "not_supported_on_worker"` に、native の失敗キー
/// (`success`/`message`) も併記する (フロントは `result.message` を読む)。
fn not_supported_on_worker(message: &str) -> worker::Result<Response> {
    Response::from_json(&json!({
        "success": false,
        "message": message,
        "error": { "code": "not_supported_on_worker", "message": message },
    }))
    .map(|response| response.with_status(501))
}


// ---------------------------------------------------------------------------
// sort_state (native `src/web/sort_state.rs` のサーバー側ソート)
// 型・正規化・比較は `narou_rs::application::webui` の共有実装を使う。
// リクエストの sort_state は常に無視し、global スコープの `current_sort`
// だけを真実源にする。
// ---------------------------------------------------------------------------

/// native `sort_numeric_targets_for_state`: 全ターゲットが数値のときだけ
/// サーバーソート順に並べ替える (1 つでも非数値があれば入力順のまま)。
async fn sort_numeric_targets_for_state(
    runtime: &WorkerRuntime,
    records: &[NovelRecord],
    targets: &[String],
) -> Vec<String> {
    let Some(ids) = targets
        .iter()
        .map(|target| target.parse::<i64>().ok())
        .collect::<Option<Vec<_>>>()
    else {
        return targets.to_vec();
    };
    let sort_state = load_current_sort_state(runtime).await;
    sort_ids_from_records(&ids, records, &sort_state)
        .into_iter()
        .map(|id| id.to_string())
        .collect()
}

/// native `sorted_update_all_ids`: 更新対象を全件スキャンし、サーバーソート順の
/// id 列にする (native は文字列にするが、Worker では plan 化まで id のまま)。
async fn sorted_update_all_ids(runtime: &WorkerRuntime) -> Vec<i64> {
    let sort_state = load_current_sort_state(runtime).await;
    let mut records = runtime
        .services
        .library
        .records()
        .await
        .unwrap_or_default();
    sort_records(&mut records, &sort_state);
    records.into_iter().map(|record| record.id).collect()
}

/// native `narou_rs::downloader::site_setting::effective_site_settings` 相当。
/// Worker 側は `bundled_sites::load_site_settings` (bundle + ユーザー定義の
/// マージ、30s L1 キャッシュ) が同じ役割。
async fn site_settings(runtime: &WorkerRuntime) -> Vec<SiteSetting> {
    crate::bundled_sites::load_site_settings(&runtime.objects())
        .await
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// native `src/commands/mod.rs` / `update.rs` のターゲット解決
// (native は子プロセス側でこれをやるので、Worker では投入時に済ませる)
// ---------------------------------------------------------------------------

/// `commands/mod.rs::resolve_target_to_id` (convert 用): alias → 数値 id →
/// URL → ncode → title/ncode の順で既存レコードの id に解決する。
/// 解決できなければ `None` (native 同様に対象を落とす)。
async fn resolve_convert_target_to_id(
    runtime: &WorkerRuntime,
    aliases: &HashMap<String, String>,
    site_settings: &[SiteSetting],
    target: &str,
) -> Option<i64> {
    let library = &runtime.services.library;
    let target = resolve_alias_target(aliases, target);
    if let Ok(id) = target.parse::<i64>() {
        // `narou convert` は `get_sync` で存在確認してから使う。
        return library
            .get(NovelId(id))
            .await
            .ok()
            .flatten()
            .map(|record| record.id);
    }
    match Downloader::get_target_type(&target) {
        TargetType::Url => {
            let setting = site_settings.iter().find(|s| s.matches_url(&target))?;
            let toc_url = setting
                .toc_url_with_url_captures(&target)
                .unwrap_or_else(|| setting.toc_url());
            library
                .find_by_toc_url(&toc_url)
                .await
                .ok()
                .flatten()
                .map(|record| record.id)
        }
        TargetType::Ncode => library
            .find_by_ncode(&target)
            .await
            .ok()
            .flatten()
            .map(|record| record.id),
        TargetType::Id => None,
        TargetType::Other => {
            if let Some(record) = library.find_by_title(&target).await.ok().flatten() {
                return Some(record.id);
            }
            // なろう式の ncode 判定 (`n\d+[a-z]+`) に当たらない ncode
            // (Pixiv の `n29204764` / `s16299140` など) はここで拾う。
            library
                .find_by_ncode(&target)
                .await
                .ok()
                .flatten()
                .map(|record| record.id)
        }
    }
}

/// `commands/update.rs::expand_tag_targets` の写し。
///
/// `tag:` / `^tag:` / 裸のタグ名を、そのタグを持つ既存レコードの id に展開し
/// 重複を除く。タグに該当が無いときは native 同様、プレフィックスを剥がした
/// 名前だけを残す (後段の id 解決で落とされる)。数値 id は存在確認済みの
/// ものだけ残る。
fn expand_tag_targets(records: &[NovelRecord], targets: &[String]) -> Vec<String> {
    // `scan_ids` (ORDER BY id ASC) 相当: records() は id 昇順で返るので
    // 全件スキャン順とタグ絞り順が native と一致する。
    let all_ids: Vec<i64> = records.iter().map(|record| record.id).collect();
    let existing: HashSet<i64> = all_ids.iter().copied().collect();
    let tag_ids = |tag_name: &str| -> Vec<i64> {
        records
            .iter()
            .filter(|record| record.tags.iter().any(|tag| tag == tag_name))
            .map(|record| record.id)
            .collect()
    };

    let mut expanded = Vec::new();
    for target in targets {
        if let Ok(id) = target.parse::<i64>() {
            if existing.contains(&id) {
                expanded.push(id.to_string());
                continue;
            }
        }

        if let Some(tag_name) = target.strip_prefix("^tag:") {
            let exclude: HashSet<i64> = tag_ids(tag_name).into_iter().collect();
            if !exclude.is_empty() {
                expanded.extend(
                    all_ids
                        .iter()
                        .filter(|id| !exclude.contains(id))
                        .map(|id| id.to_string()),
                );
            } else {
                expanded.push(tag_name.to_string());
            }
        } else if let Some(tag_name) = target.strip_prefix("tag:") {
            let ids = tag_ids(tag_name);
            if !ids.is_empty() {
                expanded.extend(ids.iter().map(|id| id.to_string()));
            } else {
                expanded.push(tag_name.to_string());
            }
        } else {
            let ids = tag_ids(target);
            if !ids.is_empty() {
                expanded.extend(ids.iter().map(|id| id.to_string()));
            } else {
                expanded.push(target.clone());
            }
        }
    }

    // native `expand_tag_targets` 末尾と同じく文字列ベースで重複除去する。
    let mut seen = HashSet::new();
    expanded
        .into_iter()
        .filter(|target| seen.insert(target.clone()))
        .collect()
}

/// `commands/update.rs::resolve_target_to_id` の写し (alias→id→URL→ncode→
/// title/ncode)。`expand_tag_targets` 後の文字列を既存レコード id にする。
/// 解決できないターゲットは native 同様に落とす (`unresolved_count`)。
async fn resolve_update_target_to_id(
    runtime: &WorkerRuntime,
    aliases: &HashMap<String, String>,
    site_settings: &[SiteSetting],
    target: &str,
) -> Option<i64> {
    let library = &runtime.services.library;
    let target = alias_to_target_for_update(aliases, target);
    if let Ok(id) = target.parse::<i64>()
        && library.get(NovelId(id)).await.ok().flatten().is_some()
    {
        return Some(id);
    }
    match Downloader::get_target_type(&target) {
        TargetType::Url => {
            // native `resolve_url_to_id`: `toc_url_with_url_captures` が
            // 取れなければ落とす (ここだけは `toc_url()` フォールバック無し)。
            let setting = site_settings.iter().find(|s| s.matches_url(&target))?;
            let toc_url = setting.toc_url_with_url_captures(&target)?;
            library
                .find_by_toc_url(&toc_url)
                .await
                .ok()
                .flatten()
                .map(|record| record.id)
        }
        TargetType::Ncode => library
            .find_by_ncode(&target)
            .await
            .ok()
            .flatten()
            .map(|record| record.id),
        TargetType::Id => None,
        TargetType::Other => {
            if let Some(record) = library.find_by_title(&target).await.ok().flatten() {
                return Some(record.id);
            }
            library
                .find_by_ncode(&target)
                .await
                .ok()
                .flatten()
                .map(|record| record.id)
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/update — native `api_update`
// ---------------------------------------------------------------------------

async fn api_update(
    req: &mut Request,
    env: &Env,
    runtime: &WorkerRuntime,
) -> worker::Result<Response> {
    let body: UpdateBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };

    let raw_targets = targets_to_strings(&body.targets);
    let max_targets = super::max_web_targets(runtime).await;
    let targets = match normalize_update_targets(&raw_targets, max_targets) {
        Ok(targets) => targets,
        Err(message) => return update_failure(&message),
    };

    // native `api_update` には `targets` に `--xxx` が残ると結合 1 ジョブに
    // する `has_flags` 経路があるが、`normalize_update_targets` は
    // `validate_web_target_value` 経由で `-` 始まりを弾く (残るのは
    // `tag:…` だけ) ので native でも dead code。Worker では複合 target
    // 文字列自体が表現できないため、その分岐は持たない。
    let is_update_all = body.update_all || targets.is_empty();
    let force = body.force;

    // (a) 更新対象 id の列を作る。native では web 層がソート済みの文字列列を
    //     作り、子プロセス (`narou update`) が tag: 展開と id 解決を行う。
    //     Worker ではここで両方を済ませて id ごとの plan にする。
    let ids: Vec<i64> = if is_update_all {
        sorted_update_all_ids(runtime).await
    } else {
        let records = runtime
            .services
            .library
            .records()
            .await
            .unwrap_or_default();
        let sorted = sort_numeric_targets_for_state(runtime, &records, &targets).await;
        let expanded = expand_tag_targets(&records, &sorted);
        let aliases = super::download::load_aliases(env).await;
        let site_settings = site_settings(runtime).await;
        let mut resolved = Vec::new();
        let mut seen = HashSet::new();
        for target in &expanded {
            if let Some(id) =
                resolve_update_target_to_id(runtime, &aliases, &site_settings, target).await
                && seen.insert(id)
            {
                resolved.push(id);
            }
        }
        resolved
    };

    // (b) native の `count` は結合 target の引数個数 (update-all では id 件数、
    //     明示指定でも 1 ターゲット = 1 引数)。Worker では tag 展開後の plan
    //     件数が実行単位なので、ここでは plan 件数を入れる。
    let count = ids.len();
    if count == 0 {
        return Response::from_json(&json!({
            "success": true,
            "status": "queued",
            "count": 0,
            "job_ids": [],
        }));
    }

    // (c) `--force` は native の args 先頭相当。executor は plan.options の
    //     `--force`/`-f` を見るので、全 plan に同じ options を載せる。
    let options = if force {
        vec!["--force".to_string()]
    } else {
        Vec::new()
    };

    // (d) `push_update_job_if_needed` parity: native は結合 target 文字列の
    //     完全一致で重複を弾く。Worker の ledger dedupe は kind+target+
    //     options 単位なので、同一リクエスト内の重複 id と既キューイング済み
    //     の id が自動で潰れる (dedupe-hit で job_id が再利用される)。
    let mut job_ids = Vec::with_capacity(ids.len());
    let mut queued_any = false;
    for id in &ids {
        let plan = JobPlan {
            kind: JobKind::Update,
            target: JobTarget::Id(NovelId(*id)),
            options: options.clone(),
        };
        let outcome = match runtime.enqueue_plan(plan).await {
            Ok(outcome) => outcome,
            Err(error) => return update_failure(&error.to_string()),
        };
        if let Some(reason) = &outcome.blocked {
            // Update+Id は常に executable なので到達しないはず。万が一のときは
            // native の push 失敗と同じ失敗形で返す。
            return update_failure(reason);
        }
        if outcome.sent {
            queued_any = true;
        }
        job_ids.push(outcome.job_id);
    }

    Response::from_json(&json!({
        "success": true,
        "status": if queued_any { "queued" } else { "already_queued" },
        "count": count,
        "job_ids": job_ids,
    }))
}

/// native `normalize_update_targets` の写し。`--tag=<name>` / `--tag <name>`
/// を `tag:<name>` に正規化し、それ以外は `validate_web_target_value` で
/// 検証する (エラー文字列も native と同じ)。
fn normalize_update_targets(targets: &[String], max_targets: usize) -> Result<Vec<String>, String> {
    if targets.len() > max_targets {
        return Err("too many targets".to_string());
    }
    let mut normalized = Vec::with_capacity(targets.len());
    let mut i = 0usize;
    while i < targets.len() {
        let target = &targets[i];
        if let Some(tag) = target.strip_prefix("--tag=") {
            let tag =
                validate_web_tag_name(tag).map_err(|_| "--tag requires a tag name".to_string())?;
            normalized.push(format!("tag:{}", tag));
            i += 1;
            continue;
        }
        if target == "--tag" {
            let Some(tag) = targets.get(i + 1) else {
                return Err("--tag requires a tag name".to_string());
            };
            let tag =
                validate_web_tag_name(tag).map_err(|_| "--tag requires a tag name".to_string())?;
            normalized.push(format!("tag:{}", tag));
            i += 2;
            continue;
        }
        normalized.push(
            validate_web_target_value(target).map_err(|_| "invalid target".to_string())?,
        );
        i += 1;
    }
    Ok(normalized)
}

// ---------------------------------------------------------------------------
// POST /api/convert — native `api_convert`
// ---------------------------------------------------------------------------

async fn api_convert(
    req: &mut Request,
    env: &Env,
    runtime: &WorkerRuntime,
) -> worker::Result<Response> {
    let body: ConvertBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };

    if body.targets.len() > super::max_web_targets(runtime).await {
        return convert_failure("too many targets");
    }
    let targets: Vec<String> = match body
        .targets
        .iter()
        .map(|target| validate_web_target_value(target))
        .collect()
    {
        Ok(targets) => targets,
        Err(message) => return convert_failure(&message),
    };
    let device = match normalize_web_device_override(body.device.as_deref()) {
        Ok(device) => device,
        Err(message) => return convert_failure(&message),
    };
    let records = runtime
        .services
        .library
        .records()
        .await
        .unwrap_or_default();
    let ordered_targets = sort_numeric_targets_for_state(runtime, &records, &targets).await;

    // native の convert ジョブは複合 target 1 つを子プロセスへ渡し、
    // `narou convert` が `resolve_target_to_id` で id に落として処理する。
    // Worker の convert は `JobTarget::Id` しか実行できないので、ここで同じ
    // 解決 (commands/mod.rs::resolve_target_to_id) を行い、解決できない
    // ターゲットは native の実行時スキップ ("<target> は存在しません") と
    // 同じく落とす。
    let aliases = super::download::load_aliases(env).await;
    let site_settings = site_settings(runtime).await;
    let mut resolved: Vec<(String, i64)> = Vec::new();
    for target in &ordered_targets {
        if let Some(id) =
            resolve_convert_target_to_id(runtime, &aliases, &site_settings, target).await
        {
            resolved.push((target.clone(), id));
        }
    }

    // `NAROU_RS_WEB_DEVICE` (native meta `device`) は現在の Worker convert
    // (`convert.rs::execute_convert` → `ConvertService::convert_and_store`)
    // では使われない。device 固有の出力量を偽らないため plan.options には
    // 載せず、応答の `device` フィールドだけ native と同じく echo する。
    let mut results = Vec::with_capacity(resolved.len());
    for (target, id) in &resolved {
        let plan = JobPlan {
            kind: JobKind::Convert,
            target: JobTarget::Id(NovelId(*id)),
            options: Vec::new(),
        };
        let outcome = match runtime.enqueue_plan(plan).await {
            Ok(outcome) => outcome,
            Err(error) => return convert_failure(&error.to_string()),
        };
        if let Some(reason) = &outcome.blocked {
            return convert_failure(reason);
        }
        results.push(json!({
            "target": target,
            "device": device.as_deref(),
            "job_id": outcome.job_id,
            "status": "queued",
        }));
    }

    Response::from_json(&json!({ "success": true, "results": results }))
}

// ---------------------------------------------------------------------------
// POST /api/update_by_tag — native `api_update_by_tag`
// ---------------------------------------------------------------------------

async fn api_update_by_tag(req: &mut Request, runtime: &WorkerRuntime) -> worker::Result<Response> {
    let body: UpdateByTagBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };

    if body.tags.len() + body.exclusion_tags.len() > MAX_WEB_TAGS_PER_REQUEST {
        return Response::from_json(&json!({
            "success": false,
            "message": "too many tags",
        }));
    }
    let tags: Vec<String> = match body
        .tags
        .iter()
        .map(|tag| validate_web_tag_name(tag))
        .collect()
    {
        Ok(tags) => tags,
        Err(message) => {
            return Response::from_json(&json!({
                "success": false,
                "message": message,
            }));
        }
    };
    let exclusion_tags: Vec<String> = match body
        .exclusion_tags
        .iter()
        .map(|tag| validate_web_tag_name(tag))
        .collect()
    {
        Ok(tags) => tags,
        Err(message) => {
            return Response::from_json(&json!({
                "success": false,
                "message": message,
            }));
        }
    };
    if tags.is_empty() && exclusion_tags.is_empty() {
        return Response::from_json(&json!({
            "success": false,
            "message": "tags or exclusion_tags required",
        }));
    }
    // native: 現在のマッチングを投入時点でスナップショットし、選択 id 更新と
    // 同じ経路 (snapshot_ids の列) で流す。native が meta に積む
    // `tag_params`/`snapshot_ids`/`sort_by` は Worker の ledger に入らない
    // (投入時点で id 展開済みなので実行には不要)。
    let server_sort_state = load_current_sort_state(runtime).await;
    let records = runtime
        .services
        .library
        .records()
        .await
        .unwrap_or_default();
    let snapshot_ids: Vec<i64> = {
        let mut matching = records
            .into_iter()
            .filter(|record| {
                let include = tags
                    .iter()
                    .any(|tag| record.tags.iter().any(|value| value == tag));
                let exclude = exclusion_tags
                    .iter()
                    .any(|tag| record.tags.iter().any(|value| value == tag));
                (tags.is_empty() || include) && !exclude
            })
            .collect::<Vec<_>>();
        sort_records(&mut matching, &server_sort_state);
        matching.into_iter().map(|record| record.id).collect()
    };

    let count = snapshot_ids.len();
    if snapshot_ids.is_empty() {
        return Response::from_json(&json!({
            "success": true,
            "count": count,
            "status": "no_targets",
        }));
    }

    // native はスナップショット済み id 列を 1 ジョブに積む (`tag:` 引数自体は
    // worker 側の `update_by_tag_update_args` で snapshot_ids に置き換わる)。
    // Worker では id ごとの discrete plan として積む。
    let mut queued_any = false;
    for id in &snapshot_ids {
        let plan = JobPlan {
            kind: JobKind::Update,
            target: JobTarget::Id(NovelId(*id)),
            options: Vec::new(),
        };
        let outcome = match runtime.enqueue_plan(plan).await {
            Ok(outcome) => outcome,
            Err(error) => {
                return Response::from_json(&json!({
                    "success": false,
                    "message": error.to_string(),
                    "count": 0,
                }));
            }
        };
        if let Some(reason) = &outcome.blocked {
            return Response::from_json(&json!({
                "success": false,
                "message": reason,
                "count": 0,
            }));
        }
        if outcome.sent {
            queued_any = true;
        }
    }

    Response::from_json(&json!({
        "success": true,
        "count": count,
        "status": if queued_any { "queued" } else { "already_queued" },
    }))
}

// ---------------------------------------------------------------------------
// POST /api/update_general_lastup — 実行不能 (501)
// ---------------------------------------------------------------------------

async fn api_update_general_lastup(req: &mut Request) -> worker::Result<Response> {
    let body: serde_json::Value = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };

    // native と同じく option の検証だけは先に行う (不正な option には native
    // と同じ 200 + success:false を返す)。
    let option = body["option"].as_str().unwrap_or("all");
    let _option = match validate_general_lastup_option(option) {
        Ok(option) => option,
        Err(message) => {
            return Response::from_json(&json!({
                "success": false,
                "message": message,
            }));
        }
    };
    let _is_update_modified = body["is_update_modified"].as_str() == Some("true")
        || body["is_update_modified"].as_bool() == Some(true);

    // `narou update --gl` は小説本体を落とさず general_lastup だけを更新する
    // 専用経路で、Worker の JobKind::Update は本体 DL しかできない。実行不能
    // な操作を「queued」に見せないため 501 を返す (native_only.rs と同じ
    // code)。`is_update_modified` のフォローアップ更新もこのジョブ内なので
    // 同時に実行不能。
    not_supported_on_worker("この Worker 環境では最新話掲載日の一括確認は利用できません")
}

/// native `validate_general_lastup_option` の写し。
fn validate_general_lastup_option(option: &str) -> Result<Option<&'static str>, String> {
    match option {
        "all" => Ok(None),
        "narou" => Ok(Some("narou")),
        "other" => Ok(Some("other")),
        _ => Err("invalid general_lastup option".to_string()),
    }
}

// ---------------------------------------------------------------------------
// POST /api/update/start — 実行不能 (501)
// ---------------------------------------------------------------------------

async fn api_update_start(req: &mut Request) -> worker::Result<Response> {
    // axum の `Option<Json<UpdateStartBody>>` は本文を読み損ねても `None`
    // (= 既定値) として続行するので、ここも失敗時は既定で進める。
    let _body: UpdateStartBody = req.json().await.unwrap_or_default();

    // native `api_update_start` は同梱 updater を起動してプロセスを再起動
    // する。Worker にはセルフアップデート経路が無い (`NoopSelfUpdateService`)
    // ので 501 を返す (フロントは非 2xx をそのまま通知に表示する)。
    not_supported_on_worker("この Worker 環境では自動アップデートは利用できません")
}
