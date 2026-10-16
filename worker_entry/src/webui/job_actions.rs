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
//! - `POST /api/update` — 解決できなかったターゲットは native では子プロセスが
//!   実行時に報告するが、Worker には子プロセスが無いため応答の `results` に
//!   `status: "unresolved"` として載せる。1 件もキューに入れられなければ
//!   `success: false` + `message` (native の not-found 出力と同じ意味論)。

use std::collections::HashMap;
use std::collections::HashSet;
use narou_rs::application::webui::{
    MAX_WEB_TAGS_PER_REQUEST, CurrentSortState, normalize_web_device_override, sort_display_label,
    sort_ids_from_records, sort_records, targets_to_strings, validate_web_tag_name,
    validate_web_target_value,
};
use narou_rs::application::messages::jobs as job_msgs;
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

use super::{json_error, load_current_sort_state_for_writes};

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

    let runtime = match WorkerRuntime::build_ui(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return json_error(503, "service_unavailable", None);
        }
    };

    match route {
        Route::Convert => api_convert(&mut req, &env, &runtime).await,
        Route::Update => api_update(&mut req, &env, &runtime).await,
        Route::UpdateByTag => api_update_by_tag(&mut req, &env, &runtime).await,
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
/// ソート状態はリクエストごとに 1 回だけ読み、並べ替えと案内文
/// (更新開始メッセージ) の両方へ渡す。
fn sort_numeric_targets_for_state(
    sort_state: &CurrentSortState,
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
    sort_ids_from_records(&ids, records, sort_state)
        .into_iter()
        .map(|id| id.to_string())
        .collect()
}

/// native `sorted_update_all_ids`: 更新対象を全件スキャンし、サーバーソート順の
/// id 列にする (native は文字列にするが、Worker では plan 化まで id のまま)。
///
/// ソート状態は `current_sort` の任意列なので、レコード全件の該当フィールドが
/// 要る (ID 投影では並べ替えられない)。読み取りは 1 回で、対象 id を確定する
/// ためのもの。
async fn sorted_update_all_ids(
    runtime: &WorkerRuntime,
    sort_state: &CurrentSortState,
) -> Vec<i64> {
    let mut records = runtime
        .write_services()
        .library
        .records()
        .await
        .unwrap_or_default();
    sort_records(&mut records, sort_state);
    records.into_iter().map(|record| record.id).collect()
}

/// native `narou_rs::downloader::site_setting::effective_site_settings` 相当。
/// Worker 側は `bundled_sites::load_site_settings` (bundle + ユーザー定義の
/// マージ、30s L1 キャッシュ) が同じ役割。
async fn site_settings(runtime: &WorkerRuntime) -> std::sync::Arc<Vec<SiteSetting>> {
    // update/convert 投入のサイト解決と変換オプションに使う (mutation 判断)。
    // bundle + ユーザー定義は primary 直行の更新系ストアで読み、
    // isolate 側の設定キャッシュもバイパスする。
    crate::bundled_sites::load_site_settings_uncached(&runtime.write_objects())
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
/// 探索順と URL fallback の規則は `TargetLookupPlan::CONVERT` が唯一の定義。
async fn resolve_convert_target_to_id(
    runtime: &WorkerRuntime,
    aliases: &HashMap<String, String>,
    site_settings: &[SiteSetting],
    target: &str,
) -> Option<i64> {
    let library = &runtime.write_services().library;
    library
        .resolve_target(
            narou_rs::application::target_lookup::TargetLookupPlan::CONVERT,
            aliases,
            narou_rs::application::target_lookup::UrlSource::Settings(site_settings),
            target,
        )
        .await
        .ok()
        .flatten()
        .map(|record| record.id)
}

/// `commands/update.rs::resolve_target_to_id` (update 用): alias→id→URL→ncode→
/// title/ncode の順で `expand_tag_targets` 後の文字列を既存レコード id に
/// する。解決できないターゲットは native 同様に落とす (`unresolved_count`)。
/// 探索順・URL の厳格な captures は `TargetLookupPlan::UPDATE` が唯一の定義。
async fn resolve_update_target_to_id(
    runtime: &WorkerRuntime,
    aliases: &HashMap<String, String>,
    site_settings: &[SiteSetting],
    target: &str,
) -> Option<i64> {
    let library = &runtime.write_services().library;
    library
        .resolve_target(
            narou_rs::application::target_lookup::TargetLookupPlan::UPDATE,
            aliases,
            narou_rs::application::target_lookup::UrlSource::Settings(site_settings),
            target,
        )
        .await
        .ok()
        .flatten()
        .map(|record| record.id)
}

/// 別名が無く、存在が確定している数値ターゲットの id。
///
/// `resolve_update_target_to_id` / `resolve_convert_target_to_id` の数値分岐は
/// 「別名解決 → `library.get` → 存在すれば Some(id)」なので、その存在が
/// 事前の 1 クエリ (`records_by_ids`) で分かっている場合は `get` を省ける。
/// 別名があるときは native 同様エイリアス先を解決するため `None` を返し、
/// 通常の解決に落とす。
fn numeric_target_id(
    aliases: &HashMap<String, String>,
    existing: &HashSet<i64>,
    target: &str,
) -> Option<i64> {
    if aliases.contains_key(target) {
        return None;
    }
    target.parse::<i64>().ok().filter(|id| existing.contains(id))
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
    // 件数上限とソート状態はどちらも毎回必要な独立した設定なので同時に読む
    // (1 リクエストの設定読み取りを 1 往復分にまとめる)。
    let (max_targets, sort_state) = futures::join!(
        super::max_web_targets(runtime),
        load_current_sort_state_for_writes(runtime),
    );
    let targets =
        match narou_rs::application::jobs::normalize_update_targets(&raw_targets, max_targets) {
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
    //
    //     deviation from native: 解決できなかったターゲットは native では生の
    //     文字列のまま子プロセスに渡り、実行時に
    //     "[ERROR] <target> は管理小説の中に存在しません" (`messages::update::
    //     unmanaged_target`) を出して続行する。Worker には子プロセスが無く、
    //     ここで黙って落とすと UI から「積んだのに動かない」原因が分からなく
    //     なるので、落とした対象を `unresolved` に残し応答の `results` で
    //     報告する (キューに積める対象の扱いは native と同じ)。
    // ソート状態は冒頭の join で読み済み (並べ替えと開始案内で共用)。
    let mut unresolved: Vec<String> = Vec::new();
    let ids: Vec<i64> = if is_update_all {
        sorted_update_all_ids(runtime, &sort_state).await
    } else {
        // 数値ターゲットの存在確認と並べ替えに要るのは選択分のレコードだけ。
        // 非数値が混ざっていても数値ターゲットの存在確認には同じ 1 クエリを
        // 使う。別名表の読み込みとは同時に走らせる。
        let numeric_ids: Vec<i64> = targets
            .iter()
            .filter_map(|target| target.parse::<i64>().ok())
            .collect();
        let (aliases, records) = futures::join!(
            super::download::load_aliases(env),
            async {
                if numeric_ids.is_empty() {
                    Vec::new()
                } else {
                    runtime
                        .write_services()
                        .library
                        .records_by_ids(&numeric_ids)
                        .await
                        .unwrap_or_default()
                }
            },
        );
        let existing_numeric: HashSet<i64> =
            records.iter().map(|record| record.id).collect();
        let sorted = sort_numeric_targets_for_state(&sort_state, &records, &targets);
        let expanded = narou_rs::application::target_lookup::expand_tag_targets(
            &runtime.write_services().library,
            &sorted,
            &existing_numeric,
            narou_rs::application::target_lookup::TagExpansionVariant::Update,
        )
        .await;
        // サイト定義 (オブジェクトストア LIST) は URL 解決でしか要らない:
        // 別名解決後の分類に URL が 1 つも無ければ読まない。分類は解決側と
        // 同じ `alias_to_target_for_update` を通す。
        let needs_sites = expanded.iter().any(|target| {
            matches!(Downloader::get_target_type(&alias_to_target_for_update(&aliases, target)), TargetType::Url)
        });
        let site_snapshot = if needs_sites {
            Some(site_settings(runtime).await)
        } else {
            None
        };
        let site_settings = site_snapshot.as_deref().map(Vec::as_slice).unwrap_or(&[]);
        let mut resolved = Vec::new();
        let mut seen = HashSet::new();
        for target in &expanded {
            // 別名が無く、存在が確定している数値 id は解決クエリを省く
            // (`resolve_update_target_to_id` の数値分岐と同じ結果)。
            let resolved_id = match numeric_target_id(&aliases, &existing_numeric, target) {
                Some(id) => Some(id),
                None => {
                    resolve_update_target_to_id(runtime, &aliases, site_settings, target).await
                }
            };
            match resolved_id {
                Some(id) => {
                    // 同一 id への重複解決は native 同様 1 ジョブに束ねる
                    // (落とさない・未解決でもない)。
                    if seen.insert(id) {
                        resolved.push(id);
                    }
                }
                None => unresolved.push(target.clone()),
            }
        }
        resolved
    };

    // (b) native の `count` は結合 target の引数個数 (update-all では id 件数、
    //     明示指定でも 1 ターゲット = 1 引数)。Worker では tag 展開後の plan
    //     件数が実行単位なので、ここでは plan 件数を入れる。
    let count = ids.len();
    if count == 0 {
        // 何もキューに入れられなかったのに success に見せない (native の子
        // プロセスが not-found を出して失敗扱いにするのと同じ意思表示)。
        // 解決できた対象が 0 件で未解決も 0 件 (= 空の update-all) のときだけ
        // native と同じ `count: 0` の成功を返す。
        if !unresolved.is_empty() {
            return Response::from_json(&json!({
                "success": false,
                "message": unresolved_update_message(&unresolved),
                "count": 0,
                "results": unresolved_results(&unresolved),
            }));
        }
        let author_jobs = match runtime.enqueue_author_checks(true).await {
            Ok(jobs) => jobs,
            Err(error) => return update_failure(&error.to_string()),
        };
        if author_jobs.iter().any(|job| job.sent) { notify_queue_changed(env, runtime).await; }
        return Response::from_json(&json!({
            "success": true,
            "status": "queued",
            "count": 0,
            "job_ids": [],
            "author_job_ids": author_jobs.into_iter().map(|job| job.job_id).collect::<Vec<_>>(),
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
    //     分は同じ job_id に畳まれる。投入は 1 回の bounded batch で行い、
    //     対象ごとの lookup→insert→get 往復を無くす (順序は ids のまま)。
    let plans: Vec<JobPlan> = ids
        .iter()
        .map(|id| JobPlan {
            kind: JobKind::Update,
            target: JobTarget::Id(NovelId(*id)),
            options: options.clone(),
        })
        .collect();
    let outcomes = match runtime.enqueue_batch(&plans).await {
        Ok(outcomes) => outcomes,
        Err(error) => return update_failure(&error.to_string()),
    };
    let mut job_ids = Vec::with_capacity(ids.len());
    let mut queued_any = false;
    for outcome in &outcomes {
        if let Some(reason) = &outcome.blocked {
            // Update+Id は常に executable なので到達しないはず。万が一のときは
            // native の push 失敗と同じ失敗形で返す。
            return update_failure(reason);
        }
        if outcome.sent {
            queued_any = true;
        }
        job_ids.push(outcome.job_id.clone());
    }
    let author_jobs = match runtime.enqueue_author_checks(true).await {
        Ok(jobs) => jobs,
        Err(error) => return update_failure(&error.to_string()),
    };
    queued_any |= author_jobs.iter().any(|job| job.sent);

    // native は更新開始案内 ("全ての小説の更新を開始します…" / "更新を開始
    // します…") をキュー投入時に `webui.message_text` メタへ積み、子プロセス
    // の拾い上げでコンソールへ echo する。Worker はメタを持たないので、受理
    // した時点で同じ文言を同じ形 (灰色 span = `console_note`) で送る。
    if queued_any {
        // native `api_update` は `queued` のときだけ `notification.queue` を
        // 送る (既キューイング済みで重複が弾かれたときは送らない)。
        notify_queue_changed(env, runtime).await;
        let message = job_msgs::update_started(
            is_update_all,
            count,
            sort_display_label(Some(&sort_state)),
        );
        broadcast_console_note(env, runtime, &message).await;
    }

    // 未解決の対象が 1 つでもあれば `results` に載せる (native は子プロセス
    // の実行時出力で報告するため応答に持たない — 上の deviation コメント参)。
    // 解決できた対象は従来どおり `count` / `job_ids` で返す。
    let mut response = json!({
        "success": true,
        "status": if queued_any { "queued" } else { "already_queued" },
        "count": count,
        "job_ids": job_ids,
        "author_job_ids": author_jobs.into_iter().map(|job| job.job_id).collect::<Vec<_>>(),
    });
    if !unresolved.is_empty() {
        response["results"] = serde_json::Value::Array(unresolved_results(&unresolved));
    }
    Response::from_json(&response)
}

/// 未解決ターゲットの `results` 行 — `/api/convert` と同じ語彙
/// (`status: "unresolved"`)。
fn unresolved_results(unresolved: &[String]) -> Vec<serde_json::Value> {
    unresolved
        .iter()
        .map(|target| json!({ "target": target, "status": "unresolved" }))
        .collect()
}

/// 未解決ターゲットを列挙した失敗メッセージ (front-end は
/// `result.message` を通知に表示する)。
fn unresolved_update_message(unresolved: &[String]) -> String {
    format!("更新対象を解決できませんでした: {}", unresolved.join(", "))
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
    if let Some(device) = device.as_deref()
        && !narou_rs::application::webui::capabilities::worker_convert_device_supported(device)
    {
        return convert_failure(&format!("Cloudflare Workers 版では {device} 形式を出力できません。EPUB を指定してください"));
    }
    // ソート状態は 1 リクエスト 1 回だけ読む。
    let sort_state = load_current_sort_state_for_writes(runtime).await;
    // 並べ替えがレコードを使うのは全ターゲットが数値のときだけ
    // (`sort_numeric_targets_for_state`)。存在確認用にも同じ 1 クエリを使い、
    // 別名表の読み込みとは同時に走らせる。
    let numeric_ids: Vec<i64> = targets
        .iter()
        .filter_map(|target| target.parse::<i64>().ok())
        .collect();
    let (aliases, records) = futures::join!(
        super::download::load_aliases(env),
        async {
            if numeric_ids.is_empty() {
                Vec::new()
            } else {
                runtime
                    .write_services()
                    .library
                    .records_by_ids(&numeric_ids)
                    .await
                    .unwrap_or_default()
            }
        },
    );
    let existing_numeric: HashSet<i64> = records.iter().map(|record| record.id).collect();
    let ordered_targets = sort_numeric_targets_for_state(&sort_state, &records, &targets);

    // native の convert ジョブは複合 target 1 つを子プロセスへ渡し、
    // `narou convert` が `resolve_target_to_id` で id に落として処理する。
    // Worker の convert は `JobTarget::Id` しか実行できないので、ここで同じ
    // 解決 (commands/mod.rs::resolve_target_to_id) を行う。
    //
    // 解決できないターゲットの扱いは native と揃える:
    // - 数値 id はそのままキューに積む (native は resolve_target_to_id が
    //   None でも convert を実行し、「<target> は存在しません」を実行時に
    //   出す)。レコード無しの id を黙って落とすと results が空になり、
    //   UI から見て「変換が走らない」原因が分からなくなる。
    // - 数値でないターゲットは `JobTarget` で表せないためキューに積めず、
    //   `status: "unresolved"` として results に残す (落とさない)。
    // サイト定義 (オブジェクトストア LIST) は URL 解決でしか要らない:
    // 別名解決後の分類に URL が 1 つも無ければ読まない。
    let needs_sites = ordered_targets.iter().any(|target| {
        matches!(Downloader::get_target_type(&resolve_alias_target(&aliases, target)), TargetType::Url)
    });
    let site_snapshot = if needs_sites {
        Some(site_settings(runtime).await)
    } else {
        None
    };
    let site_settings = site_snapshot.as_deref().map(Vec::as_slice).unwrap_or(&[]);

    // Worker の出力は組み込み EPUB のみ。対応形式は上で検証済みなので、
    // 外部ツール用 device オプションをジョブへ積む必要はない。
    // ターゲットごとに (表示用文字列, 解決した id) を保持する。同一 id に
    // 解決したターゲットが複数あっても native 同様に行は出す (enqueue の
    // dedupe が同一 job_id を返す)。
    let mut resolved: Vec<(String, Option<i64>)> = Vec::with_capacity(ordered_targets.len());
    for target in &ordered_targets {
        // 別名が無く、存在が確定している数値 id は解決クエリを省く。
        // 存在しない数値 id は resolve が None を返し、下の parse フォール
        // バックで同じ id になる (native がミスを実行時に報告するのと同じ)。
        let id = match numeric_target_id(&aliases, &existing_numeric, target) {
            Some(id) => Some(id),
            None => {
                match resolve_convert_target_to_id(runtime, &aliases, site_settings, target).await
                {
                    Some(id) => Some(id),
                    None => target.parse::<i64>().ok(),
                }
            }
        };
        resolved.push((target.clone(), id));
    }
    // `sort_numeric_targets_for_state` はレコードに無い数値 id を捨てるので、
    // 元の targets から欠落した数値 id を拾ってキューに積む (実行時に
    // 「小説 N がありません」として見えるようにする)。
    for target in &targets {
        if let Ok(id) = target.parse::<i64>()
            && !resolved.iter().any(|(existing, _)| existing == target)
        {
            resolved.push((target.clone(), Some(id)));
        }
    }

    // 解決できた行だけを 1 回の bounded batch で投入する (対象ごとの
    // lookup→insert→get 往復を無くす)。results は unresolved 行を挟んだ
    // 行順のまま組み立てる。
    let mut plans = Vec::with_capacity(resolved.len());
    for (_, id) in &resolved {
        if let Some(id) = id {
            plans.push(JobPlan {
                kind: JobKind::Convert,
                target: JobTarget::Id(NovelId(*id)),
                options: Vec::new(),
            });
        }
    }
    let outcomes = match runtime.enqueue_batch(&plans).await {
        Ok(outcomes) => outcomes,
        Err(error) => return convert_failure(&error.to_string()),
    };
    let mut outcomes = outcomes.into_iter();
    let mut results = Vec::with_capacity(resolved.len());
    let mut queued_any = false;
    for (target, id) in &resolved {
        if id.is_none() {
            results.push(json!({
                "target": target,
                "device": device.as_deref(),
                "status": "unresolved",
            }));
            continue;
        }
        let Some(outcome) = outcomes.next() else {
            return convert_failure("internal enqueue mismatch");
        };
        if let Some(reason) = &outcome.blocked {
            return convert_failure(reason);
        }
        queued_any = true;
        results.push(json!({
            "target": target,
            "device": device.as_deref(),
            "job_id": outcome.job_id,
            "status": "queued",
        }));
    }

    // native `api_convert` は受理したジョブを積んだあとに
    // `notification.queue` を送る (request 単位で 1 回)。1 ジョブも積めなかった
    // (= 全ターゲット unresolved) 要求ではキューが変わらないので送らない。
    if queued_any {
        notify_queue_changed(env, runtime).await;
    }

    Response::from_json(&json!({ "success": true, "results": results }))
}

// ---------------------------------------------------------------------------
// POST /api/update_by_tag — native `api_update_by_tag`
// ---------------------------------------------------------------------------

async fn api_update_by_tag(
    req: &mut Request,
    env: &Env,
    runtime: &WorkerRuntime,
) -> worker::Result<Response> {
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
    //
    // タグ条件のマッチングには全レコードのタグが要る (ID 投影では判定
    // できない) が、ソート状態の読み取りとは同時に走らせる。
    let (server_sort_state, records) = futures::join!(
        load_current_sort_state_for_writes(runtime),
        runtime.write_services().library.records(),
    );
    let records = records.unwrap_or_default();
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
    // Worker では id ごとの discrete plan を 1 回の bounded batch で積む
    // (対象ごとの lookup→insert→get 往復を無くす。順序は snapshot_ids のまま)。
    let plans: Vec<JobPlan> = snapshot_ids
        .iter()
        .map(|id| JobPlan {
            kind: JobKind::Update,
            target: JobTarget::Id(NovelId(*id)),
            options: Vec::new(),
        })
        .collect();
    let outcomes = match runtime.enqueue_batch(&plans).await {
        Ok(outcomes) => outcomes,
        Err(error) => {
            return Response::from_json(&json!({
                "success": false,
                "message": error.to_string(),
                "count": 0,
            }));
        }
    };
    let mut queued_any = false;
    for outcome in &outcomes {
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

    // native と同じ更新開始案内 (タグ更新は `is_update_all=false` の形)。
    if queued_any {
        // native `api_update_by_tag` は `queued` のときだけ
        // `notification.queue` を送る。
        notify_queue_changed(env, runtime).await;
        let message = job_msgs::update_started(
            false,
            count,
            sort_display_label(Some(&server_sort_state)),
        );
        broadcast_console_note(env, runtime, &message).await;
    }

    Response::from_json(&json!({
        "success": true,
        "count": count,
        "status": if queued_any { "queued" } else { "already_queued" },
    }))
}

/// `console_note` 形 (灰色 span) の 1 行を PushHub の `echo` で送る。
/// native `src/web/worker.rs` が `webui.message_text` を broadcast_echo する
/// のと同じ見え方になる。送信失敗はユーザーをブロックしない (best-effort)。
async fn broadcast_console_note(env: &Env, runtime: &WorkerRuntime, text: &str) {
    let push = crate::push_hub::PushHubClient::new(env, runtime.subrequests.clone());
    push.broadcast_best_effort(&[crate::push_hub::echo(
        &job_msgs::console_note(text),
        "stdout",
    )])
    .await;
}

/// native `src/web/jobs.rs::notify_queue_changed` 相当。受理した投入のあとに
/// `notification.queue` を送る (native は request 単位で 1 回、`queued` の
/// ときだけ)。表시更新なので best-effort — 送信失敗でも応答は変えない。
async fn notify_queue_changed(env: &Env, runtime: &WorkerRuntime) {
    let push = crate::push_hub::PushHubClient::new(env, runtime.subrequests.clone());
    crate::consumer::notify_queue_changed(&push).await;
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
