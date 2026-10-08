//! `GET/POST /api/global_setting`。
//!
//! 設定ページが読む JSON は native と共有する
//! [`narou_rs::application::settings_view`] が組み立てる（`D1SettingsStore` の上に
//! 載った `SettingsService` を渡すだけ）。native と違い、保存後の副作用
//! （自動更新スケジューラーの再起動や `webui.*` の再読み込み）は無い。Worker の
//! スケジューラーは cron 側で動くため。

use narou_rs::application::settings_view::{self, SAVE_MESSAGE};
use worker::{console_log, Env, Method, Request, Response};

use crate::webui::metadata;

/// 設定一覧の取得と保存。
pub async fn api_global_setting(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let method = req.method();
    if method != Method::Get && method != Method::Post {
        return Response::error("Method Not Allowed", 405);
    }
    let started_ms = js_sys::Date::now();
    if method == Method::Get {
        let settings = match metadata::settings(&env) {
            Ok(settings) => settings,
            Err(error) => {
                console_log!("service composition failed: {error}");
                return Response::error("Service Unavailable", 503);
            }
        };
        let mut view = settings_view::load_view(&settings).await;
        mark_worker_ineffective(&mut view);
        return metadata::timed_response(Response::from_json(&view)?, "settings", started_ms);
    }
    // POST は load→save の read-modify-write なので、primary 直行 +
    // isolate キャッシュ無しのストアを使う。replica/cached スナップ
    // ショットで保存すると同時編集の値を黙って消す。
    let settings = match metadata::settings_for_writes(&env) {
        Ok(settings) => settings,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service Unavailable", 503);
        }
    };
    let body: serde_json::Value = match req.json().await {
        Ok(body) => body,
        Err(_) => return Response::error("Bad Request", 400),
    };
    // Worker に実行経路が無い項目は画面側で disabled にするが、API を直接
    // 叩かれた場合も値が変わらないようここで弾く (保存自体は行わない)。
    if let Some(changes) = settings_view::parse_changes(&body) {
        let rejected: Vec<&str> = changes
            .iter()
            .map(|(name, _)| name.as_str())
            .filter(|name| is_worker_ineffective(name))
            .collect();
        if !rejected.is_empty() {
            return Response::from_json(&result_body(
                false,
                &format!(
                    "この設定は Cloudflare Workers 版では変更できません: {}",
                    rejected.join(", ")
                ),
            ));
        }
    }
    match settings_view::apply_save(&settings, &body).await {
        Ok(_effects) => Response::from_json(&result_body(true, SAVE_MESSAGE)),
        Err(message) => Response::from_json(&result_body(false, &message)),
    }
}

/// `ApiResponse` と同じ形（native の応答と揃える）。
fn result_body(success: bool, message: &str) -> serde_json::Value {
    serde_json::json!({
        "success": success,
        "message": message,
    })
}

/// `GET /api/global_setting` の各項目に、Worker では経路が無い (= 変更しても
/// 効かない) 設定へ `worker_ineffective` と `worker_note` を付ける。
///
/// native では `webui_help_override` 相当の表示上書きで調整しているが、
/// Worker には対象の実行経路自体が無い項目が多い (ローカル FS / 外部
/// プロセス / サーバーバインド前提の設定)。native の応答にはこのキーが
/// 乗らないため、フロント側は「フラグが無ければ従来どおり表示」で良い。
///
/// 対象の選定根拠 (Worker 側に該当コードが無いことをコードで確認済み):
/// - `server-*` … 認証は `NAROU_ADMIN_TOKEN` 固定 (`lib.rs::auth_state`)。
///   `server-max-targets-per-request` だけ `webui::max_web_targets` が読む
///   ため除外する。
/// - `mail.*` / `send.*` … `/api/mail` `/api/send` は
///   `webui/native_only.rs` で 501 (`not_supported_on_worker`)。
/// - `hotentry` … 新着まとめ生成は `send`/メール経路前提で Worker に無い。
/// - `logging` / `logging.*` … `src/logger.rs` (native ファイル出力) のみが
///   読む。Worker のログは console + D1 履歴。
/// - `difftool` … 外部 diff ツールを spawn する `narou diff` 専用。
/// - `download.narou-api.*` … なろう API 一括更新は `native-runtime` ゲート
///   (`src/downloader/narou_api.rs` の `narou_api_user_agent` / `interval`)。
/// - `download.choices-of-digest-options` … digest 化検知の対話プロンプトは
///   `native-runtime` ゲート (`downloader::process_digest`)。Worker は
///   `NarouError::Unsupported` で Blocked になるため読まれない。
/// - `default_args.*` … CLI コマンドの既定引数。Worker に CLI が無い。
/// - 個別名 … `aozoraepub3dir` / `line-height` (外部 AozoraEpub3 前提。
///   Lite は `convert.epub-font` 同様 Worker では参照しない)、`device` /
///   `convert.multi-device` (Worker convert は端末選択を持たない。
///   `webui/job_actions.rs` が device を plan に載せないと明記)、
///   `convert.copy-*` / `folder` / `normalize-filename` /
///   `filename-length-limit` / `folder-length-limit` (ローカル FS の
///   出力・コピー・ファイル名前提)、`concurrency.*` (native コンソールの
///   キュー表示整形。Worker は読まない。`concurrency` 自体は Worker でも
///   効く — convert ジョブの行を `stdout2` へ分ける)、
///   `economy` / `no-color` / `color-parser` / `multiple-delimiter` /
///   `time-zone` (CLI・コンソール・native ファイル出力専用)、
///   `narou-compat` (D1 固定ストアの切替対象が無い)、
///   `self-update.variant` (self-update 経路自体が無い)、
///   `update.max-parallel-domains` (native の `commands/update.rs` のみが
///   読む。Worker の update ジョブはドメイン並列化を持たない)。
///
/// 逆に Worker で読まれるため対象外: `download.use-subdirectory` /
/// `guard-spoiler` / `auto-add-tags` / `update.strong` / `over18` /
/// `user-agent` / `download.interval` / `download.wait-steps` /
/// `update.interval` / `update.sort-by` / `update.auto-schedule*` /
/// `queue.*` / `webui.*` / `server-max-targets-per-request`
/// (`composition.rs::LOCAL_BOOL_KEYS` と `scheduler.rs` / `ui_prefs.rs` /
/// `webui::max_web_targets` が D1 から読む)、`update.convert-only-new-arrival`
/// (`executor.rs` の自動変換連鎖が読む)、`convert.filename-to-ncode` /
/// `ebook-filename-length-limit` (`lib.rs::epub_download_filename` が
/// `OutputNamingEnv::from_local_map` で読む)、`convert.rotate-image`
/// (`lib.rs` の `download.epub` が `EpubBuildOptions::rotate_image` へ渡す)、
/// および個別変換設定の `default.*` / `force.*` 全般 (`ConvertService` が
/// `NovelSettings::from_sources` 経由で適用)。
fn mark_worker_ineffective(view: &mut serde_json::Value) {
    const NOTE: &str = "この設定は Cloudflare Workers 版では効きません (実行経路が native 専用)";
    let Some(items) = view.get_mut("settings").and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for item in items {
        let Some(name) = item.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if is_worker_ineffective(name) {
            item["worker_ineffective"] = serde_json::json!(true);
            item["worker_note"] = serde_json::json!(NOTE);
        }
    }
}

/// Worker では実行経路が無い (= 設定を変えても効かない) 設定名かどうか。
/// `mark_worker_ineffective` (表示) と POST の拒否判定が共用する。
fn is_worker_ineffective(name: &str) -> bool {
    /// 前方一致で無効になる prefix。`server-` は `server-max-targets-per-request`
    /// が Worker でも読まれるため `server_max_targets` で除外する。
    const PREFIXES: &[&str] = &[
        "server-",
        "mail.",
        "send.",
        "hotentry",
        "logging.",
        "concurrency.",
        "difftool",
        "download.narou-api.",
        "default_args.",
    ];
    /// 個別名。変換のコピー先・端末依存のもの、サーバー bind、ローカル FS・
    /// CLI/コンソール・外部プロセスを前提とする設定は Worker では読み出す
    /// 経路自体が無い。
    const NAMES: &[&str] = &[
        "aozoraepub3dir",
        "color-parser",
        "convert.add-dc-subject-to-epub",
        "convert.copy-to",
        "convert.copy-to-grouping",
        "convert.copy-zip-to",
        "convert.dc-subject-exclude-tags",
        "convert.epub-font",
        "convert.inspect",
        "convert.make-zip",
        "convert.multi-device",
        "convert.no-mobi",
        "convert.no-open",
        "convert.no-zip",
        "device",
        "download.choices-of-digest-options",
        "economy",
        "filename-length-limit",
        "folder",
        "folder-length-limit",
        "line-height",
        "logging",
        "multiple-delimiter",
        "narou-compat",
        "no-color",
        "normalize-filename",
        "self-update.variant",
        "time-zone",
        "update.max-parallel-domains",
    ];
    PREFIXES.iter().any(|prefix| {
        name.starts_with(prefix) && (name != "server-max-targets-per-request")
    }) || NAMES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::is_worker_ineffective;

    /// Worker で読まれる設定 (D1 に書き込む意味があるもの) を誤って無効化
    /// しないことを固定する。
    #[test]
    fn effective_settings_are_not_marked() {
        for name in [
            // scheduler / downloader / convert が D1 から読むもの
            "download.interval",
            "download.use-subdirectory",
            "update.interval",
            "update.sort-by",
            "update.strong",
            "update.auto-schedule.enable",
            "update.convert-only-new-arrival",
            "queue.max-retries",
            "user-agent",
            "over18",
            "webui.theme",
            "webui.debug-mode",
            "webui.new-tag-color",
            // UI のデュアルコンソール表示と convert ジョブの行の宛先に効く
            "concurrency",
            // server- prefix でも Worker が読む唯一の例外
            "server-max-targets-per-request",
            // download.epub のファイル名に効く命名設定と挿絵回転
            "convert.filename-to-ncode",
            "convert.rotate-image",
            "ebook-filename-length-limit",
            // 個別変換設定の default.*/force.* は ConvertService が適用する
            "default.enable_yokogaki",
            "force.enable_illust",
        ] {
            assert!(
                !is_worker_ineffective(name),
                "{name} は Worker で効くので無効化しない"
            );
        }
    }

    /// Worker に実行経路が無い設定が無効化対象になることを固定する。
    #[test]
    fn ineffective_settings_are_marked() {
        for name in [
            "aozoraepub3dir",
            "device",
            "convert.copy-to",
            "mail.address",
            "send.without-freeze",
            "server-port",
            "server-basic-auth.enable",
            "default_args.convert",
            "logging",
            "concurrency.format-queue-style",
            "time-zone",
            "difftool.arg",
            "download.narou-api.interval",
            "self-update.variant",
        ] {
            assert!(
                is_worker_ineffective(name),
                "{name} は Worker に経路が無いので無効化する"
            );
        }
    }
}
