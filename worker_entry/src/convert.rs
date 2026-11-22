//! Worker 内での変換ジョブ（`JobKind::Convert`）。
//!
//! 保存済みの TOC と本文から変換テキストを組み立て、`<prefix>/novel.txt` に
//! 書く。native の `convert_novel_by_id` が書く固定名ミラーと同じキーなので、
//! 続けて `download.epub` がそのまま配信できる（外部プロセスは使わない）。

use narou_rs::application::novel_access::{NovelAccess, NovelReadScope, NovelWriteGuard};
use narou_rs::application::convert::{
    ConvertItemReport, ConvertService, ConvertedNovel, emit_convert_item_lines,
};
use narou_rs::application::jobs::{JobFailureClass, JobPlan, JobTarget, classify_failure};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::NovelMutation;

use crate::composition::WorkerRuntime;
use crate::executor::JobOutcome;
use crate::push_hub::{PushHubClient, PushHubSink};

/// 変換ジョブの行を送るコンソール。
///
/// native `web::worker::console_target_for_job` は外部通信を持たないジョブ
/// (convert/send/backup/mail) の行を、`concurrency` 有効時に `stdout2` へ寄せる
/// (DL/update の行と同じコンソールに混ざらないようにする)。
///
/// Worker は queue の `max_concurrency` でジョブが**常に並列**なので、native の
/// 「`concurrency=false` = 全ジョブを 1 レーンで逐次実行」に相当する状態が無い。
/// そのため既定を「有効」に倒し、明示的に `concurrency=false` を保存したときだけ
/// 1 コンソールへまとめる (native と同じ設定で同じ見え方になる)。
async fn console_target(runtime: &WorkerRuntime) -> Option<&'static str> {
    if concurrency_enabled(runtime).await {
        Some("stdout2")
    } else {
        None
    }
}

/// `concurrency` 設定 (未設定の Worker では有効)。
async fn concurrency_enabled(runtime: &WorkerRuntime) -> bool {
    match runtime
        .settings_store()
        .load(narou_rs::setting_core::SettingScope::Local)
        .await
    {
        Ok(local) => local
            .get("concurrency")
            .and_then(narou_rs::setting_core::setting_bool_value)
            .unwrap_or(true),
        // 設定を読めないときは分離側 (既定) に倒す。
        Err(_) => true,
    }
}

/// `Convert` プランを実行する。対象は単一小説の id のみ。
///
/// Deep output is scoped to this invocation; the FIFO also delivers ordinary
/// conversion lines before the job's terminal event.
pub async fn execute_convert(
    runtime: &WorkerRuntime,
    job: &JobPlan,
    job_id: &narou_rs::application::JobId,
    owner: std::sync::Arc<narou_rs::platform::JobExecutionOwner>,
    checkpoint: Option<&narou_rs::application::WorkerExecutionCheckpoint>,
    push: &PushHubClient,
) -> JobOutcome {
    let target = match job.target {
        JobTarget::Id(id) => id,
        _ => {
            return JobOutcome::Permanent {
                reason: "convert requires a numeric novel id".to_string(),
            };
        }
    };

    // The shared emitter wraps the whole attempt: namespace lookup, writer
    // admission, metadata revalidation and the conversion itself. Rejected
    // targets therefore still reach this job's console through the shared
    // id_missing/item_error lines under the started/processing header, exactly
    // like native `cmd_convert` prints them.
    let push_sink = PushHubSink::shared(push.clone(), console_target(runtime).await);
    let lines_sink = push_sink.clone();
    let context = narou_rs::application::messages::MessageContext::new(
        push_sink.clone(), crate::consumer::webui_debug_mode(runtime).await,
    );
    // Admission hands the guard/read-lease pair back through this slot so the
    // lease is released and the convert-failure flag is updated with the same
    // original writer after the emitter has drained its lines.
    let mut conversion = None;
    let conversion_slot = &mut conversion;
    let report = push_sink.run(context.scope(async move {
        emit_convert_item_lines(lines_sink.as_ref(), &job.target.as_str(), target.0, || async move {
            let Some(mut attempt) = prepare(runtime, job_id, checkpoint, target, owner).await? else {
                return Ok(None);
            };
            let converted = convert(runtime, &mut attempt.access, &attempt.record).await;
            *conversion_slot = Some(attempt);
            converted
        }).await
    })).await;

    let (guard, access) = match conversion {
        Some(PreparedConversion { guard, access, .. }) => (guard, access),
        None => {
            return match report {
                // レコード不在: ledger / queue_failed には従来通りのエラー文言を残す
                // (コンソール行は id_missing で既に出ている)。
                ConvertItemReport::Missing => JobOutcome::Permanent {
                    reason: format!("小説 {} がありません", target.0),
                },
                // admission (guard 捕捉・checkpoint 保存・再検証) の失敗は既に
                // item_error で出ている。失敗フラグを立てる guard が無いので
                // 分類だけ返す。
                ConvertItemReport::Failed { error } => classify(error),
                // 成功した変換は必ず pair をスロットへ戻す。
                ConvertItemReport::Converted { .. } => classify(NarouError::Platform(
                    "conversion writer lost after a successful convert".into(),
                )),
            };
        }
    };

    let released = access.release().await;
    if let Err(error) = released { return classify(error); }
    match report {
        ConvertItemReport::Converted { .. } => {
            // native `clear_convert_failure` 相当: 成功したら失敗フラグを落とす。
            match set_convert_failure_flag(runtime, &guard, target, false).await {
                Ok(()) => JobOutcome::Succeeded,
                Err(error) => classify(error),
            }
        }
        // レコード不在: ledger / queue_failed には従来通りのエラー文言を残す
        // (コンソール行は id_missing で既に出ている)。
        ConvertItemReport::Missing => JobOutcome::Permanent {
            reason: format!("小説 {} がありません", target.0),
        },
        // native `set_convert_failure` 相当: 失敗した変換はレコードに記録し、
        // 次回 update の差分なし (`None`) 実行で再変換されるようにする。
        ConvertItemReport::Failed { error } => {
            if let Err(flag_error) = set_convert_failure_flag(runtime, &guard, target, true).await { return classify(flag_error); }
            classify(error)
        }
    }
}

/// 変換 1 件分の admission が持ち帰るもの。emitter を抜けた後の read lease
/// 解放と失敗フラグ更新に同じ pair (writer guard + admission) を渡すため
/// 1 つにまとめる。
struct PreparedConversion {
    record: narou_rs::db::NovelRecord,
    guard: NovelWriteGuard,
    access: NovelAccess,
}

/// 名前空間 lookup → writer 捕捉 → checkpoint 保存 → read lease 許可 →
/// メタデータ再検証を一連の admission として行う。
///
/// The first lookup discovers only the namespace; `Ok(None)` maps to the
/// shared `id_missing` line without touching content. A chained job resumes
/// its saved origin and a standalone conversion captures once — either way
/// before any content access, and every `Err` also releases the admitted
/// reader before returning.
async fn prepare(
    runtime: &WorkerRuntime,
    job_id: &narou_rs::application::JobId,
    checkpoint: Option<&narou_rs::application::WorkerExecutionCheckpoint>,
    target: narou_rs::platform::NovelId,
    owner: std::sync::Arc<narou_rs::platform::JobExecutionOwner>,
) -> Result<Option<PreparedConversion>> {
    let Some(initial) = runtime.novels.get_current(target).await? else {
        return Ok(None);
    };
    let keys = narou_rs::platform::NovelObjectKeys::new(&initial.sitename, &initial.file_title, initial.use_subdirectory)?;
    let writer = match checkpoint.and_then(|saved| saved.write_origin.as_ref()) {
        Some(origin) => NovelWriteGuard::resume(runtime.write_objects(), Some(runtime.assets()), origin).await,
        None => NovelWriteGuard::capture_for(&runtime.write_objects(), Some(&runtime.assets()), &keys).await,
    };
    let mut guard = writer?;
    guard.bind_job_owner(std::sync::Arc::clone(&owner))?;
    if guard.prefix().as_ref().trim_end_matches('/') != keys.prefix().as_ref() {
        return Err(NarouError::Platform("conversion writer namespace changed".into()));
    }
    let durable = checkpoint.cloned().unwrap_or_else(|| narou_rs::application::WorkerExecutionCheckpoint::planned(job_id.clone(), Some(target)))
        .with_write_origin(guard.origin());
    narou_rs::application::JobQueue::save_checkpoint(runtime.ledger.as_ref(), job_id, owner.execution_token(), &durable).await?;
    let mut access = NovelAccess::from_writer(guard.clone(), "convert-job").await?;
    let loaded: Result<_> = async {
        access.ensure_read().await?.validate().await?;
        let record = runtime.novels.get_current(target).await?
            .ok_or_else(|| NarouError::NotFound(format!("小説 {} がありません", target.0)))?;
        let current = narou_rs::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
        access.ensure_read().await?.validate().await?;
        if current.prefix() != keys.prefix() {
            return Err(NarouError::Platform("conversion namespace changed during admission".into()));
        }
        Ok(record)
    }.await;
    match loaded {
        Ok(record) => Ok(Some(PreparedConversion { record, guard, access })),
        Err(error) => { let _ = access.release().await; Err(error) }
    }
}

/// 変換失敗フラグの更新 (native `commands::update.rs::set_convert_failure`
/// / `clear_convert_failure` 相当)。レコード遷移は共有
/// `novel_actions::mark_convert_failure` が持ち、更新するタイミング
/// (Converted で解除 / Failed で設定) は native と同じ。
/// The same origin is validated again before guarded bookkeeping.
async fn set_convert_failure_flag(runtime: &WorkerRuntime, guard: &NovelWriteGuard, id: narou_rs::platform::NovelId, failed: bool) -> Result<()> {
    let mut access = NovelAccess::from_writer(guard.clone(), "convert-failure-flag").await?;
    let result: Result<()> = async {
        access.ensure_read().await?.validate().await?;
        let Some(mut record) = runtime.novels.get_current(id).await? else {
            return Err(narou_rs::error::NarouError::NotFound(format!("novel {} disappeared during conversion", id.0)));
        };
        let keys = narou_rs::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
        access.ensure_read().await?.validate().await?;
        if keys.prefix().as_ref() != guard.prefix().as_ref().trim_end_matches('/') {
            return Err(narou_rs::error::NarouError::Platform("conversion bookkeeping namespace changed".into()));
        }
        if narou_rs::application::novel_actions::mark_convert_failure(&mut record, failed) {
            runtime.novels.apply_batch_guarded(guard.fence(), vec![NovelMutation::Upsert(record)]).await?;
        }
        Ok(())
    }.await;
    let released = access.release().await;
    result?;
    released
}

/// レコード解決込みで 1 件変換する。`Ok(None)` は id_missing に対応する。
async fn convert(
    runtime: &WorkerRuntime,
    access: &mut NovelAccess,
    record: &narou_rs::db::NovelRecord,
) -> Result<Option<ConvertedNovel>> {
    let service = ConvertService::new(runtime.write_objects(), runtime.assets(), runtime.settings_store());
    let keep_text = keep_converted_text(runtime).await;
    let converted = if record.ncode.as_deref().is_some_and(narou_rs::virtual_novel::is_virtual_ncode) {
        let persistence = narou_rs::downloader::persistence::PersistenceService::new(runtime.write_objects());
        let scope = NovelReadScope::new(&runtime.write_objects(), "virtual-convert");
        let book = crate::virtual_novels::load_book(scope, runtime.write_db(), record, runtime.novels.as_ref(), &persistence).await?;
        service.convert_source(access, record, book.toc, book.sections, keep_text).await?
    } else if keep_text {
        service.convert_and_store(access, record).await?
    } else {
        service.convert_only(access, record).await?
    };
    if !keep_text {
        let keys = narou_rs::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
        access.write()?.delete(&keys.converted_text()).await?;
    }
    Ok(Some(converted))
}


/// 変換済みテキストを保存するか (`convert.keep-txt`)。
///
/// 環境変数 `NAROU_RS_KEEP_TXT` (0/false/no/off) が最優先、次に D1 の
/// local 設定。Worker は D1 を食わないよう **既定 false** (保存しない) で、
/// 変換テキストは EPUB のダウンロード時に組み立て直す。true にすると
/// ダウンロードのたびの変換を省ける代わりに 1 作品あたり数 MB を D1 に持つ。
pub(crate) async fn keep_converted_text(runtime: &WorkerRuntime) -> bool {
    let local = runtime
        .settings_store()
        .load(narou_rs::setting_core::SettingScope::Local)
        .await
        .unwrap_or_default();
    keep_converted_text_with(&local)
}

/// Worker の `convert.keep-txt` 既定 (D1 を食わないよう保存しない)。
/// native のファイル経路と `ConvertService` は `true`。
pub(crate) const KEEP_CONVERTED_TEXT_DEFAULT: bool = false;

/// 変換済みテキストを保存するか (設定 map 版)。
///
/// 環境変数 `NAROU_RS_KEEP_TXT` (0/false/no/off) が最優先、次に local
/// 設定 (`convert.keep-txt`)。解釈は native と共有する
/// (`narou_rs::setting_core::keep_converted_text`)。Worker は
/// [`KEEP_CONVERTED_TEXT_DEFAULT`] で保存せず、変換テキストは EPUB の
/// ダウンロード時に組み立て直す。`download.epub` が保持した設定 map を
/// そのまま渡せる形。
pub(crate) fn keep_converted_text_with(
    local: &std::collections::HashMap<String, serde_yaml::Value>,
) -> bool {
    narou_rs::setting_core::keep_converted_text(
        local,
        std::env::var("NAROU_RS_KEEP_TXT").ok().as_deref(),
        KEEP_CONVERTED_TEXT_DEFAULT,
    )
}

/// 変換失敗をジョブ結果へ写す。分類は download 経路と同じ共有実装
/// (`application::jobs::classify_failure`) に寄せる — 変換で起きる
/// `NarouError` のうち Platform/Io は D1・オブジェクトストアの一時障害を
/// 含み得るためリトライ対象、NotFound/InvalidTarget/Conversion は恒久失敗。
fn classify(error: NarouError) -> JobOutcome {
    match classify_failure(&error) {
        JobFailureClass::Retryable => JobOutcome::Retryable {
            reason: error.to_string(),
        },
        JobFailureClass::Permanent => JobOutcome::Permanent {
            reason: error.to_string(),
        },
        JobFailureClass::Blocked => JobOutcome::Blocked {
            reason: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::classify;
    use crate::executor::JobOutcome;
    use narou_rs::error::NarouError;

    /// convert の失敗分類は共有 `classify_failure` と同じ結果を返す。
    /// かつてのローカル分岐と差がある点を固定する:
    /// - Platform (D1/オブジェクトストア障害を含む) はリトライ対象
    /// - 欠落セクションの Io(NotFound) もリトライ → 再実行で本文があれば
    ///   成功し得る (枯渇時は Permanent)
    /// - NotFound / InvalidTarget / Conversion は恒久失敗
    /// - Yaml は恒久的リトライではなく Blocked (設定/データ不整合)
    #[test]
    fn classify_matches_shared_failure_classes() {
        let platform = classify(NarouError::Platform("D1 timeout".into()));
        assert!(
            matches!(platform, JobOutcome::Retryable { .. }),
            "{platform:?}"
        );

        let missing_section = classify(NarouError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "section file not found: expected '2 x.yaml' in novels/site/title/本文",
        )));
        assert!(
            matches!(missing_section, JobOutcome::Retryable { .. }),
            "{missing_section:?}"
        );

        for error in [
            NarouError::NotFound("gone".into()),
            NarouError::InvalidTarget("bad".into()),
            NarouError::Conversion("broken".into()),
        ] {
            let outcome = classify(error);
            assert!(
                matches!(outcome, JobOutcome::Permanent { .. }),
                "{outcome:?}"
            );
        }

        let yaml = classify(NarouError::Yaml(
            serde_yaml::from_str::<serde_yaml::Value>("a: [").unwrap_err(),
        ));
        assert!(matches!(yaml, JobOutcome::Blocked { .. }), "{yaml:?}");
    }
}
