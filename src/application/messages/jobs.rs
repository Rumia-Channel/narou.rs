//! ジョブ実行経路 (`worker_entry` の consumer/executor/scheduler と、native
//! の `src/web` キュー/スケジューラ) が UI へ出す文言。
//! native は子プロセスの出力がそのまま echo されるので CLI 本体は
//! メッセージを合成しない; Worker は in-process 実行なのでここの文言を
//! PushHub の `echo` に流す。Web UI のキュー行・スケジューラ案内・
//! コンソール装飾 (span) も native/Worker で同じ文言にするためここへ集約
//! する。native と別行にならないよう、文言は native 側の対応文言に揃える。
use std::fmt::{Debug, Display};

/// キューへの投入を拒否したときの通知 (message_id + 理由)。
pub fn enqueue_rejected(message_id: impl Display, reason: impl Display) -> String {
    format!("キュー投入を拒否しました ({message_id}): {reason}")
}

/// 再開チェックポイントが壊れていたので先頭からやり直す通知。
pub fn resume_checkpoint_invalid(reason: impl Display) -> String {
    format!("再開チェックポイントが不正です ({reason})。先頭から実行し直します")
}

/// Worker が実行できないジョブ種別。
pub fn unsupported_job_kind(kind: impl Debug) -> String {
    format!("unsupported job kind {kind:?} on the worker (no subprocess support)")
}

/// `JobTarget::All` はワーカー上で展開済みジョブが前提。
pub fn auto_update_needs_discrete_plan() -> &'static str {
    "auto-update must be planned into discrete per-novel jobs"
}

/// 対話判断が必要なジョブはワーカーでは進められない。
pub fn job_canceled_interactive(kind: impl Display) -> String {
    format!("job {kind} canceled: an interactive decision (auth/adult/digest) has no terminal on the worker")
}

/// 小説が見つからない / 更新が拒否された永久失敗。
pub fn download_failed_permanent() -> &'static str {
    "download failed: novel was not found or the update was refused"
}

/// セクション境界でのバジェット切れ (yield したジョブの理由)。
pub fn budget_expired_partial(
    limit: impl Display,
    next_section_index: impl Display,
    subrequests_used: impl Display,
) -> String {
    format!("実行枠の制限に達したため取得を一時中断し、保存済みの続きから自動再開します（{limit}、再開位置 {next_section_index}、subrequests {subrequests_used}）")
}

/// リトライ回数を使い切ったときの永久失敗理由。
pub fn retries_exhausted(attempts: impl Display, reason: impl Display) -> String {
    format!("retries exhausted after {attempts} attempts: {reason}")
}

// --- 再開チェックポイント検証の失敗理由 ---

pub fn checkpoint_job_mismatch(found: impl Display, expected: impl Display) -> String {
    format!("execution checkpoint belongs to job {found}, not {expected}")
}

pub fn checkpoint_different_novel() -> &'static str {
    "execution checkpoint belongs to a different novel"
}

pub fn checkpoint_no_cursor() -> &'static str {
    "execution checkpoint has no section cursor"
}

pub fn checkpoint_index_unrepresentable(index: impl Display) -> String {
    format!("execution checkpoint section index {index} is not representable")
}

// --- エンベロープ拒否理由 (ledger.last_error + UI echo 共通) ---

pub fn malformed_envelope(version: impl Display, error: impl Display) -> String {
    format!("malformed v{version} envelope: {error}")
}

pub fn envelope_oversized(max_bytes: impl Display) -> String {
    format!("envelope exceeds queue payload limit (max {max_bytes} bytes)")
}

pub fn ledger_missing_job(job_id: impl Display) -> String {
    format!("ledger has no row for queued job {job_id}")
}

pub fn unsupported_envelope_version(version: impl Display) -> String {
    format!("unsupported envelope version {version}")
}

// --- Web UI のキュー投入・スケジューラ案内 (`src/web/jobs.rs` /
// `src/web/scheduler.rs` / `src/web/worker.rs` と Worker で共有) ---

/// 更新ジョブの開始案内。native はキューの実行 spec メタ
/// (`webui.message_text`) に乗せ、実行開始時にコンソールへ echo する。
/// `sort_display` はソート状態の表示名 ("ID順" / "タイトル昇順" など)。
pub fn update_started(is_update_all: bool, count: usize, sort_display: impl Display) -> String {
    if is_update_all {
        format!("全ての小説の更新を開始します（{count}件を{sort_display}で処理）")
    } else {
        format!("更新を開始します（{count}件を{sort_display}で処理）")
    }
}

/// 自動アップデートの実行予告。cron 発火 (= スケジュール時刻) に出す。
/// native (`src/web/scheduler.rs`) は `Local` 時刻を渡し、Worker は
/// `update.auto-schedule.timezone` (既定 UTC) の壁時計を渡す。
pub fn auto_update_scheduled(at: impl Display) -> String {
    format!("自動アップデートが予定されています: {at}")
}

/// スケジュールを逃した分の自動アップデート追いつき実行 (catch-up)。
/// 今のところ native (`src/web/scheduler.rs`) だけが発するが、文言は
/// Worker 側と同じ出所に置く。
pub fn auto_update_catch_up(at: impl Display) -> String {
    format!("自動アップデートを catch-up 実行します: {at}")
}

/// 自動アップデートのジョブがキューへ入った通知。
pub fn auto_update_queued(job_id: impl Display) -> String {
    format!("自動アップデートをキューに追加しました ({job_id})")
}

/// 自動アップデートのキュー追加そのものが失敗した通知。
pub fn auto_update_enqueue_failed(error: impl Display) -> String {
    format!("自動アップデートのキュー追加に失敗しました: {error}")
}

/// 自動アップデート最終実行時刻の保存失敗 (native `persist_last_auto_update_run`)。
pub fn auto_update_last_run_save_failed(error: impl Display) -> String {
    format!("自動アップデート最終実行時刻の保存に失敗しました: {error}")
}

/// `update.auto-schedule` の時刻指定が不正 (native `start_auto_update_scheduler`)。
pub fn auto_update_schedule_invalid(schedule: impl Display) -> String {
    format!("自動アップデートスケジューラーの時刻指定が不正です: {schedule}")
}

/// `update.auto-schedule.timezone` が IANA 名でない (native `load_schedule_timezone`)。
pub fn auto_update_timezone_invalid(name: impl Display) -> String {
    format!("update.auto-schedule.timezone の値が不正です (IANA 名で指定): {name}")
}

/// キューイング済みの自動アップデートが既にある通知。
pub fn auto_update_already_queued() -> &'static str {
    "自動アップデートは既にキューまたは実行中に存在します"
}

// --- 自動アップデート実行フェーズ (`src/web/scheduler.rs` の echo 行) ---
// Worker の自動更新はページ単位のジョブ投入なので、ここのフェーズ文言は
// 現状 native だけが発する (worker は `auto_update_scheduled` /
// `auto_update_queued` のみ)。文言の出所はここに集約しておく。

/// 自動アップデート実行開始行 (実行時刻つき)。
pub fn auto_update_running(at: impl Display) -> String {
    format!("自動アップデートを実行中... ({at})")
}

/// フェーズ単位の失敗まとめ ("自動アップデート失敗: {label}")。
pub fn auto_update_failed(label: impl Display) -> String {
    format!("自動アップデート失敗: {label}")
}

/// modified タグの付いた小説が無い通知。
pub fn auto_update_no_modified() -> &'static str {
    "自動アップデート: modified タグの付いた小説はありません"
}

/// modified タグの付いた小説を更新する予告 (件数つき)。
pub fn auto_update_modified(count: usize) -> String {
    format!("自動アップデート: modified タグの付いた小説を更新します ({count}件)")
}

/// `modified タグの付いた小説を更新します` の装飾付き行 (native web では
/// `colored(.., "yellow")` で出す本体 — Worker 側でも span 形を使う)。
pub fn auto_update_modified_note() -> String {
    console_span("modified タグの付いた小説を更新します", "goldenrod")
}

/// 通常更新対象のその他小説が無い通知。
pub fn auto_update_no_others() -> &'static str {
    "自動アップデート: 通常更新の対象となるその他小説はありません"
}

/// その他小説を通常更新する予告 (件数つき)。
pub fn auto_update_others(count: usize) -> String {
    format!("自動アップデート: その他小説を通常更新します ({count}件)")
}

/// 自動アップデートの正常終了通知。
pub fn auto_update_completed() -> &'static str {
    "自動アップデートが正常に完了しました"
}

/// WebUI のソート設定を適用したときの通知 (ソートキー名つき)。
pub fn auto_update_sort_applied(sort_key: impl Display) -> String {
    format!("自動アップデート: WebUIソート設定を適用 ({sort_key})")
}

/// ソート設定が無いときの既定実行通知。
pub fn auto_update_default_sort() -> &'static str {
    "自動アップデート: デフォルトソート順序で実行"
}

/// フェーズ実行が致命傷で止まった行 (起動・終了待ち・exit code など)。
/// native は `label` にフェーズ名を入れる ("なろうAPIによる更新確認" 等)。
pub fn auto_update_fatal(label: impl Display, detail: impl Display) -> String {
    format!("{label} で重大なエラーが発生しました（{detail}）")
}

/// 実行ファイル自体を取得できなかった。
pub fn auto_update_fatal_no_exe() -> &'static str {
    "実行ファイルを取得できません"
}

/// update 子プロセスを起動できなかった。
pub fn auto_update_fatal_spawn(error: impl Display) -> String {
    format!("update を起動できません: {error}")
}

/// update 子プロセスの終了待ちに失敗した。
pub fn auto_update_fatal_wait() -> &'static str {
    "update の終了待機に失敗しました"
}

/// 終了コードが読めなかった (シグナル終了など)。
pub fn auto_update_fatal_no_exit_code() -> &'static str {
    "終了コード不明"
}

/// 異常な終了コード (128 以降) で止まった。
pub fn auto_update_fatal_exit_code(code: impl Display) -> String {
    format!("終了コード: {code}")
}

/// フェーズの正常終了。
pub fn auto_update_phase_done(label: impl Display) -> String {
    format!("{label} が完了しました")
}

/// フェーズが件数エラーありで完了した。
pub fn auto_update_phase_done_with_errors(label: impl Display, count: impl Display) -> String {
    format!("{label} が完了しました（{count}件の小説でエラーがありました）")
}

/// フェーズ後の DB 再読み込み失敗。
pub fn auto_update_db_refresh_failed(label: impl Display, error: impl Display) -> String {
    format!("{label} 後のDB再読み込みに失敗しました: {error}")
}

/// 再配信が途絶えてリトライ枠を枯渇させた永久失敗理由 (cron reaper)。
pub fn retries_exhausted_dead_lettered(attempts: impl Display, reason: impl Display) -> String {
    format!("retries exhausted after {attempts} attempts and the scheduled redelivery was lost (dead-lettered): {reason}")
}

// --- Web コンソールの HTML span 装飾 (native `termcolor::colored` /
// `bold_colored` の Web モード出力と一致させる) ---

/// `termcolor` の CSS 色名 → CSS 値マップ (`color_css`) と同じ対応。
/// Worker / messages 層では `is_web_mode` を持たないので HTML 文字列を
/// 直接組み立てる。`{text}` は呼び出し側が渡す文言で、native の span 出力
/// (text をエスケープしない `<span>` 内包) と揃えるためここではエスケープ
/// しない — フロントの `appendConsole` が span タグの style を制限し、残り
/// の HTML はテキスト化する。
pub fn console_span(text: impl Display, css_color: impl Display) -> String {
    format!("<span style=\"color:{css_color}\">{text}</span>")
}

/// native `bold_colored` の Web モード出力に対応する太字 + 明色の span。
pub fn console_span_bold(text: impl Display, css_color: impl Display) -> String {
    format!("<span style=\"font-weight:bold;color:{css_color}\">{text}</span>")
}

/// 更新開始案内のコンソール用形 (native `append_update_args` と同じ灰色)。
pub fn console_note(text: impl Display) -> String {
    console_span(text, "#bbb")
}

/// `[ERROR]` タグのコンソール用形 (native `bold_colored("[ERROR]", "red")`
/// の Web モード出力と同じ)。
pub fn console_error_tag() -> String {
    console_span_bold("[ERROR]", "red")
}

/// 前回変換失敗小説の再変換予告のコンソール用形
/// (native `colored(reconvert_after_failure(), "yellow")` の Web モード出力と同じ)。
pub fn console_reconvert_note() -> String {
    console_span(crate::application::messages::update::reconvert_after_failure(), "goldenrod")
}

