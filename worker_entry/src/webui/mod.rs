//! Web UI の API のうち、native (`src/web/*.rs`) から Worker へ移植したもの。
//!
//! 各モジュールは `pub async fn handle(req, env) -> worker::Result<Response>` を
//! 公開し、`worker_entry/src/lib.rs` のルート表から呼ばれる。JSON の形・キー名・
//! ステータスコードは native 実装と一致させる（フロントエンドを共有するため）。
//! Worker で実現できない操作（ローカル FS 前提など）は、成功を偽装せず 501 と
//! 機械可読な code を返す。
//!
//! このファイルにはサブモジュール共通の小さなヘルパを集約する。純粋な入力
//! バリデーション・ソート状態の正規化・上限値は native と同じ規則を使うため
//! `narou_rs::application::webui` (可搬層) が唯一の定義であり、ここには Worker
//! の `Response` / `WorkerRuntime` に依存するラッパだけを置く。

use narou_rs::application::web_payloads::ApiResponse;
use narou_rs::application::webui::{
    CURRENT_SORT_KEY, CurrentSortState, MAX_WEB_TARGETS_PER_REQUEST,
    default_current_sort_state, normalize_current_sort_value,
};
use narou_rs::setting_core::SettingScope;
use worker::Response;

use crate::composition::WorkerRuntime;

pub mod download;
pub mod job_actions;
pub mod library_backup;
pub mod read_views;
pub mod login_actions;
pub mod queue_actions;
pub mod row_actions;
pub mod tag_actions;
pub mod native_only;
pub mod list;
pub mod ui_prefs;
pub mod queue;

/// 機械可読なエラー応答 (`{error: {code, message?}}`)。
///
/// このモジュール群 (および `lib.rs`) で使う唯一の実装。ボディの組み立ては
/// 可搬層 `narou_rs::application::webui::json_error_body` に委譲するので、
/// JSON の形はテストで固定されている。
pub(crate) fn json_error(status: u16, code: &str, message: Option<&str>) -> worker::Result<Response> {
    Response::from_json(&narou_rs::application::webui::json_error_body(code, message))
        .map(|response| response.with_status(status))
}

/// native `ApiResponse` (`{success, message}`) と同じ形の JSON 値。
pub(crate) fn api_response(success: bool, message: impl Into<String>) -> serde_json::Value {
    serde_json::to_value(ApiResponse {
        success,
        message: message.into(),
    })
    .expect("ApiResponse serialization is infallible")
}

/// URL のクエリパラメータを 1 件取り出す。
pub(crate) fn query_param(url: &worker::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

/// native `max_web_targets_per_request` (`src/web/mod.rs`) parity:
/// `server-max-targets-per-request` 設定を読み、無い/不正なら既定上限を返す。
pub(crate) async fn max_web_targets(runtime: &WorkerRuntime) -> usize {
    runtime
        .services
        .settings
        .web_target_limit(MAX_WEB_TARGETS_PER_REQUEST)
        .await
}

/// native `load_current_sort_state`: `server_setting` (global) の
/// `current_sort` キーを読み、無ければ既定値。Worker では global スコープの
/// `current_sort` 行 (D1 `app_state`, `webui::ui_prefs` の保存先) がその値
/// そのもの。
pub(crate) async fn load_current_sort_state(runtime: &WorkerRuntime) -> CurrentSortState {
    runtime
        .services
        .settings
        .get_raw(SettingScope::Global, CURRENT_SORT_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|value| normalize_current_sort_value(&value))
        .unwrap_or_else(default_current_sort_state)
}

/// native `configured_tag_color` (`src/web/mod.rs`) parity:
/// `webui.new-tag-color` 設定を読み、小文字化 + 有効色チェックを通す。
pub(crate) async fn configured_tag_color(runtime: &WorkerRuntime) -> Option<String> {
    runtime
        .services
        .settings
        .get(narou_rs::application::tag_colors::NEW_TAG_COLOR_SETTING)
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_str().map(str::to_owned))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| narou_rs::application::tag_colors::is_valid_tag_color(value))
}
