//! ジョブ実行経路 (`worker_entry` の consumer/executor) が UI へ出す文言。
//! native は子プロセスの出力がそのまま echo されるのでメッセージを合成しない;
//! Worker は in-process 実行なのでここの文言を PushHub の `echo` に流す。
//! native と別行にならないよう、文言は native 側の対応文言に揃える。

use std::fmt::Debug;
use std::fmt::Display;

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
    format!("section-boundary {limit} budget expired before section {next_section_index} ({subrequests_used} subrequests used)")
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

#[cfg(test)]
mod tests {
    #[test]
    fn fixed_literals_match_worker() {
        assert_eq!(
            super::auto_update_needs_discrete_plan(),
            "auto-update must be planned into discrete per-novel jobs"
        );
        assert_eq!(
            super::download_failed_permanent(),
            "download failed: novel was not found or the update was refused"
        );
        assert_eq!(
            super::checkpoint_different_novel(),
            "execution checkpoint belongs to a different novel"
        );
        assert_eq!(
            super::checkpoint_no_cursor(),
            "execution checkpoint has no section cursor"
        );
    }

    #[test]
    fn lines_match_worker() {
        assert_eq!(
            super::enqueue_rejected("m1", "bad"),
            "キュー投入を拒否しました (m1): bad"
        );
        assert_eq!(
            super::resume_checkpoint_invalid("crc"),
            "再開チェックポイントが不正です (crc)。先頭から実行し直します"
        );
        #[derive(Debug)]
        enum Kind {
            Diff,
        }
        assert_eq!(
            super::unsupported_job_kind(Kind::Diff),
            "unsupported job kind Diff on the worker (no subprocess support)"
        );
        assert_eq!(
            super::job_canceled_interactive("download"),
            "job download canceled: an interactive decision (auth/adult/digest) has no terminal on the worker"
        );
        assert_eq!(
            super::budget_expired_partial("wall-clock", 7, 120),
            "section-boundary wall-clock budget expired before section 7 (120 subrequests used)"
        );
        assert_eq!(
            super::retries_exhausted(3, "timeout"),
            "retries exhausted after 3 attempts: timeout"
        );
        assert_eq!(
            super::checkpoint_job_mismatch("j1", "j2"),
            "execution checkpoint belongs to job j1, not j2"
        );
        assert_eq!(
            super::checkpoint_index_unrepresentable(99),
            "execution checkpoint section index 99 is not representable"
        );
        assert_eq!(
            super::malformed_envelope(2, "syntax"),
            "malformed v2 envelope: syntax"
        );
        assert_eq!(
            super::envelope_oversized(1024),
            "envelope exceeds queue payload limit (max 1024 bytes)"
        );
        assert_eq!(
            super::ledger_missing_job("j9"),
            "ledger has no row for queued job j9"
        );
        assert_eq!(
            super::unsupported_envelope_version(3),
            "unsupported envelope version 3"
        );
    }
}
