//! Web UI の能力 (capability) 定義 — native と Worker が共有する。
//!
//! `GET /api/webui/config` の `capabilities` キーとして両ランタイムが同じ形を
//! 返す。フロントエンドは「対応していない操作を出さない」ためにこれだけを見る
//! ので、ここに載る値は必ず実際の経路と一致させること:
//!
//! - `actions` はユーザー操作単位。Worker で 501 (`not_supported_on_worker`
//!   など) を返す経路は必ず `false` にする (成功を偽装しない)。
//! - `convert.devices` は `GET /api/devices` と同じ端末一覧・同じ availability。
//!   Worker 側のハンドラはこの表から JSON を組み立てる。`device_overrides` は
//!   `POST /api/convert` が受理する明示指定で、`device_override_keys` の写像を
//!   通して同じ表から導出する (端末一覧と検証が乖離しない)。
//! - 実行時の値 (コンソール履歴の保持数など) は構築時に渡す。

use std::borrow::Cow;

/// 実行ランタイム。native の Web サーバー / Cloudflare Worker。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Native,
    Worker,
}

/// ユーザー操作単位の可否。UI は `false` の操作を無効化/非表示にする。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ActionCapabilities {
    /// `/api/reorder_pending_tasks` (キューのドラッグ並べ替え)。
    pub queue_reorder: bool,
    /// `/api/csv/download`。
    pub csv_export: bool,
    /// `/api/csv/import`。
    pub csv_import: bool,
    /// `/api/update_general_lastup` (最新話掲載日の一括確認)。
    pub general_lastup_update: bool,
    /// `/api/mail`。
    pub mail: bool,
    /// `/api/send` (端末への送信)。
    pub send: bool,
    /// `/api/shutdown`。
    pub shutdown: bool,
    /// `/api/reboot`。
    pub reboot: bool,
    /// `/api/folder` (保存フォルダを開く)。
    pub open_folder: bool,
    /// `/api/backup` (zip バックアップの作成)。
    pub backup: bool,
    /// `/api/backup_bookmark` (端末側ブックマークの取り込み)。
    pub backup_bookmark: bool,
    /// `/api/library_backup` の作成 (POST)。状態取得 (GET) だけでは
    /// 「バックアップを作れる」ことにはならないので、この操作の可否を表す。
    pub library_backup: bool,
    /// 調査ログの生成 (`POST /api/inspect` と `narou inspect` 相当)。
    pub inspect: bool,
    /// `/api/setting_burn` (設定の焼き込み)。
    pub setting_burn: bool,
    /// 本文の版履歴 (`/api/diff` / `/api/diff_history` / `/api/diff_show`)。
    pub diff_history: bool,
    /// 版履歴からの復元・マージ (`/api/diff_restore` / `/api/diff_merge`)。
    pub diff_restore: bool,
    /// 自動アップデート (`/api/update/start`)。バージョン確認の読取は別。
    pub self_update: bool,
    /// `/api/storage/mode` の POST (管理方式の切替)。
    pub storage_mode_switch: bool,
}

impl ActionCapabilities {
    /// native は全操作が使える。ストレージモード固有の制約が生まれた場合は
    /// ここで絞る (唯一の定義点)。
    fn native() -> Self {
        Self {
            queue_reorder: true,
            csv_export: true,
            csv_import: true,
            general_lastup_update: true,
            mail: true,
            send: true,
            shutdown: true,
            reboot: true,
            open_folder: true,
            backup: true,
            backup_bookmark: true,
            library_backup: true,
            inspect: true,
            setting_burn: true,
            diff_history: true,
            diff_restore: true,
            self_update: true,
            storage_mode_switch: true,
        }
    }

    /// Worker ではどの操作にも経路が無い (`webui::native_only` /
    /// `read_views` / `queue_actions` / `library_backup` が 501 を返す)。
    /// 対応するハンドラを実装したら、そのフィールドだけ `true` へ戻す。
    fn worker() -> Self {
        Self {
            queue_reorder: false,
            csv_export: false,
            csv_import: false,
            general_lastup_update: false,
            mail: false,
            send: false,
            shutdown: false,
            reboot: false,
            open_folder: false,
            backup: false,
            backup_bookmark: false,
            library_backup: false,
            inspect: false,
            setting_burn: false,
            diff_history: false,
            diff_restore: false,
            self_update: false,
            storage_mode_switch: false,
        }
    }
}

/// コンソール履歴 (`GET /api/history` / `POST /api/clear_history`) の能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ConsoleHistoryCapabilities {
    /// `GET /api/history` が保持済みの履歴を返すか。
    pub available: bool,
    /// 返し得る履歴の最大件数。native は `/api/history` の行数上限、
    /// Worker は Durable Object が抱える件数。
    pub capacity: usize,
    /// `POST /api/clear_history` が履歴を実際に消すか。
    pub clear: bool,
    /// 返し得る履歴の最大バイト数 (古い方から落とす目安)。
    pub max_bytes: usize,
}

/// native `/api/history` が返す件数・バイト数の上限
/// (`src/web/push.rs` と共有)。
pub const NATIVE_CONSOLE_HISTORY_CAPACITY: usize = 1000;
pub const NATIVE_CONSOLE_HISTORY_MAX_BYTES: usize = 1024 * 1024;

/// `/api/devices` の 1 端末。`name` は native `OutputManager::available_devices`
/// と同じキー。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DeviceChoice {
    pub name: Cow<'static, str>,
    pub available: bool,
}

/// Worker の端末一覧。Worker がユーザーへ配信できる形式は Lite の EPUB だけ
/// (テキストは変換の内部形式で、ダウンロード経路が無い。ibunko / mobi / kobo /
/// reader / ibooks は外部プロセスやローカル FS 前提の出力で経路自体が無い)。
///
/// `GET /api/devices` (webui/settings.rs) と `POST /api/convert` の device 検証
/// はこの一覧が唯一の定義。
static WORKER_DEVICES: [DeviceChoice; 7] = [
    DeviceChoice { name: Cow::Borrowed("text"), available: false },
    DeviceChoice { name: Cow::Borrowed("epub"), available: true },
    DeviceChoice { name: Cow::Borrowed("mobi"), available: false },
    DeviceChoice { name: Cow::Borrowed("kobo"), available: false },
    DeviceChoice { name: Cow::Borrowed("ibunko"), available: false },
    DeviceChoice { name: Cow::Borrowed("reader"), available: false },
    DeviceChoice { name: Cow::Borrowed("ibooks"), available: false },
];

pub fn worker_devices() -> &'static [DeviceChoice] {
    &WORKER_DEVICES
}

/// native `/api/devices` の端末名一覧 (`OutputManager::available_devices` と
/// 同じ並び)。availability は実行環境 (外部ツールの有無) で変わるため、
/// native 側は実行時に検出した値を渡す。
pub const NATIVE_DEVICE_NAMES: &[&str] = &[
    "text", "epub", "mobi", "kobo", "ibunko", "reader", "ibooks",
];

/// 端末名 → `POST /api/convert` の device override キー
/// (`normalize_web_device_override` が返す正規化キー)。
///
/// `mobi` の override 名だけは `Device::from_str` の別名 (`kindle`) に揃える。
fn device_override_keys(name: &str) -> &'static [&'static str] {
    match name {
        "text" => &["text"],
        "epub" => &["epub"],
        "mobi" => &["kindle"],
        "kobo" => &["kobo"],
        "ibunko" => &["ibunko"],
        "reader" => &["reader"],
        "ibooks" => &["ibooks"],
        _ => &[],
    }
}

/// Worker が明示 device override として受理するキー。
pub fn worker_convert_device_overrides() -> Vec<&'static str> {
    worker_devices()
        .iter()
        .filter(|choice| choice.available)
        .flat_map(|choice| device_override_keys(&choice.name).iter().copied())
        .collect()
}

/// Worker の `POST /api/convert` が `device` 指定を受理できるか
/// (正規化済みキー)。`false` は「その出力形式に対応していない」= 応答を
/// 成功に見せてはいけない。
pub fn worker_convert_device_supported(device: &str) -> bool {
    worker_devices()
        .iter()
        .filter(|choice| choice.available)
        .any(|choice| device_override_keys(&choice.name).contains(&device))
}

/// native が受理する device override キー (外部ツールの有無に関わらず、
/// `normalize_web_device_override` の語彙そのもの)。
pub fn native_convert_device_overrides() -> Vec<&'static str> {
    NATIVE_DEVICE_NAMES
        .iter()
        .flat_map(|name| device_override_keys(name).iter().copied())
        .collect()
}

/// per-novel 設定 (`setting.ini` / 個別設定 API) の制約。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PerNovelSettingsCapabilities {
    /// UI と API が制限する、出力へ反映されない設定。
    pub ineffective: &'static [IneffectiveSetting],
}

impl PerNovelSettingsCapabilities {
    fn native() -> Self {
        Self {
            ineffective: &[],
        }
    }

    fn worker() -> Self {
        Self {
            ineffective: WORKER_INEFFECTIVE_NOVEL_SETTINGS,
        }
    }
}

/// 出力に効かない per-novel 設定 1 件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct IneffectiveSetting {
    pub name: &'static str,
    /// 機械可読な理由 (`inspection_log_is_native_only` など)。
    pub reason: &'static str,
}

pub const WORKER_INEFFECTIVE_NOVEL_SETTINGS: &[IneffectiveSetting] = &[
    IneffectiveSetting { name: "enable_inspect", reason: "inspection_log_is_native_only" },
];

pub fn worker_novel_setting_ineffective(name: &str) -> bool {
    WORKER_INEFFECTIVE_NOVEL_SETTINGS.iter().any(|setting| setting.name == name)
}

/// 変換・端末まわりの能力。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ConvertCapabilities {
    /// `GET /api/devices` と同じ形。
    pub devices: Cow<'static, [DeviceChoice]>,
    /// `POST /api/convert` が受理する明示 device override キー。
    pub device_overrides: Vec<&'static str>,
    pub per_novel_settings: PerNovelSettingsCapabilities,
}

/// `GET /api/webui/config` の `capabilities` 値。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WebUiCapabilities {
    pub runtime: RuntimeKind,
    pub actions: ActionCapabilities,
    pub console_history: ConsoleHistoryCapabilities,
    pub convert: ConvertCapabilities,
}

impl WebUiCapabilities {
    /// native の能力。`devices` は `OutputManager::available_devices()` の
    /// 実行時検出値を渡す。
    pub fn native(devices: Vec<DeviceChoice>) -> Self {
        Self {
            runtime: RuntimeKind::Native,
            actions: ActionCapabilities::native(),
            console_history: ConsoleHistoryCapabilities {
                available: true,
                capacity: NATIVE_CONSOLE_HISTORY_CAPACITY,
                clear: true,
                max_bytes: NATIVE_CONSOLE_HISTORY_MAX_BYTES,
            },
            convert: ConvertCapabilities {
                devices: Cow::Owned(devices),
                device_overrides: native_convert_device_overrides(),
                per_novel_settings: PerNovelSettingsCapabilities::native(),
            },
        }
    }

    /// Worker の能力。実装が増えたハンドラに応じて `ActionCapabilities::worker`
    /// を更新する。コンソール履歴は実装状況 (DO の履歴経路) を渡す。
    pub fn worker(console_history: ConsoleHistoryCapabilities) -> Self {
        Self {
            runtime: RuntimeKind::Worker,
            actions: ActionCapabilities::worker(),
            console_history,
            convert: ConvertCapabilities {
                devices: Cow::Borrowed(worker_devices()),
                device_overrides: worker_convert_device_overrides(),
                per_novel_settings: PerNovelSettingsCapabilities::worker(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Worker は epub 以外の明示指定 (特に kindle/mobi) を受理しない。
    #[test]
    fn worker_rejects_unsupported_device_overrides() {
        assert!(worker_convert_device_supported("epub"));
        for unsupported in ["text", "kindle", "mobi", "kobo", "ibunko", "reader", "ibooks"] {
            assert!(
                !worker_convert_device_supported(unsupported),
                "{unsupported} は Worker では出力できない"
            );
        }
        assert!(!worker_convert_device_overrides().contains(&"kindle"));
    }

}
