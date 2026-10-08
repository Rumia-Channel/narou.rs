//! Worker 内での変換ジョブ（`JobKind::Convert`）。
//!
//! 保存済みの TOC と本文から変換テキストを組み立て、`<prefix>/novel.txt` に
//! 書く。native の `convert_novel_by_id` が書く固定名ミラーと同じキーなので、
//! 続けて `download.epub` がそのまま配信できる（外部プロセスは使わない）。

use std::sync::Arc;

use narou_rs::application::convert::{
    ConvertItemReport, ConvertService, ConvertedNovel, emit_convert_item_lines,
};
use narou_rs::application::jobs::{JobFailureClass, JobPlan, JobTarget, classify_failure};
use narou_rs::converter::ConverterCapabilities;
use narou_rs::downloader::http_policy::FetchPolicy;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::NovelMutation;
use worker::console_log;

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
            .and_then(crate::composition::setting_bool)
            .unwrap_or(true),
        // 設定を読めないときは分離側 (既定) に倒す。
        Err(_) => true,
    }
}

/// `Convert` プランを実行する。対象は単一小説の id のみ。
///
/// コンソール行は native `narou convert <id>` と同じものを出す。download
/// ジョブと同じく `PushHubSink` を既定 sink としてインストールし (深い
/// 呼び出しの `emit_default` も同じ経路に乗る)、行自体は共有実装
/// (`application::convert::emit_convert_item_lines`) が送る。バッファ分は
/// ジョブの区切りで `drain()` がまとめて PushHub へ流す。
pub async fn execute_convert(
    runtime: &WorkerRuntime,
    job: &JobPlan,
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

    let push_sink = PushHubSink::install_with_console(push.clone(), console_target(runtime).await);
    let report = emit_convert_item_lines(push_sink.as_ref(), &job.target.as_str(), target.0, || {
        convert(runtime, target)
    })
    .await;
    push_sink.drain().await;
    // ジョブの sink を isolate から外す (executor と同じ理由)。
    narou_rs::application::messages::take_default_sink();

    match report {
        ConvertItemReport::Written { .. } => {
            // native `clear_convert_failure` 相当: 成功したら失敗フラグを落とす。
            set_convert_failure_flag(runtime, target, false).await;
            JobOutcome::Succeeded
        }
        // レコード不在: ledger / queue_failed には従来通りのエラー文言を残す
        // (コンソール行は id_missing で既に出ている)。
        ConvertItemReport::Missing => JobOutcome::Permanent {
            reason: format!("小説 {} がありません", target.0),
        },
        // native `set_convert_failure` 相当: 失敗した変換はレコードに記録し、
        // 次回 update の差分なし (`None`) 実行で再変換されるようにする。
        ConvertItemReport::Failed { error } => {
            set_convert_failure_flag(runtime, target, true).await;
            classify(error)
        }
    }
}

/// `commands::update.rs::set_convert_failure` / `clear_convert_failure` の
/// Worker 版。失敗してもジョブ結果を変えない (ログだけ残す best-effort)。
async fn set_convert_failure_flag(runtime: &WorkerRuntime, id: narou_rs::platform::NovelId, failed: bool) {
    let result: Result<()> = async {
        let Some(mut record) = runtime.novels.get(id).await? else {
            return Ok(());
        };
        if record.convert_failure != failed {
            record.convert_failure = failed;
            runtime
                .novels
                .apply_batch(vec![NovelMutation::Upsert(record)])
                .await?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        console_log!("convert flag update failed for novel {}: {error}", id.0);
    }
}

/// レコード解決込みで 1 件変換する。`Ok(None)` は id_missing に対応する。
async fn convert(
    runtime: &WorkerRuntime,
    id: narou_rs::platform::NovelId,
) -> Result<Option<ConvertedNovel>> {
    let Some(record) = runtime
        .services
        .library
        .get(id)
        .await
        .map_err(|error| NarouError::Platform(error.to_string()))?
    else {
        return Ok(None);
    };

    let service = convert_service(runtime, &record);
    // `convert.keep-txt=false` では変換済みテキストを保存しない
    // (EPUB のダウンロード時に組み立て直す。CPU と引き換えに D1 を節約する)。
    let converted = if keep_converted_text(runtime).await {
        service.convert_and_store(&record).await?
    } else {
        let converted = service.convert_only(&record).await?;
        // 以前に変換済みテキストが残っていれば掃除する
        // (`convert.keep-txt=false` は何も保存しない構成)。
        if let Ok(keys) = narou_rs::platform::NovelObjectKeys::new(
            &record.sitename,
            &record.file_title,
            record.use_subdirectory,
        ) {
            let _ = runtime.objects().delete(&keys.converted_text()).await;
        }
        converted
    };
    Ok(Some(converted))
}

/// 変換テキストだけを組み立てる (保存しない)。
///
/// 変換済みテキストを保存しない構成で、EPUB のダウンロード時に使う。
pub(crate) async fn convert_text_only(
    runtime: &WorkerRuntime,
    record: &narou_rs::db::NovelRecord,
) -> Result<String> {
    let service = convert_service(runtime, record);
    service
        .convert_only(record)
        .await
        .map(|converted| converted.text)
}

/// 1 小説ぶんの ConverterCapabilities を組む。
///
/// native の CLI 変換と同じく、挿絵のローカライズ能力は渡さない
/// (`native::converter::native_capabilities` も assets/objects を None にする)。
/// 挿絵を取るリクエストはサイト定義のヘッダが要る (i.pximg.net は Referer
/// 無しを 403 で弾く)。`fetch_policy` は対象小説のサイトのもの、
/// `fetch_policy_resolver` は native と同じ レコード→サイト定義→policy
/// 解決 (`FetchPolicy::for_record`) で、小説ごとの切替に対応する。
fn convert_service(runtime: &WorkerRuntime, record: &narou_rs::db::NovelRecord) -> ConvertService {
    let site_settings: Arc<[SiteSetting]> = runtime.site_settings().into();
    let capabilities = ConverterCapabilities {
        http: runtime.http_client(),
        rate_limiter: runtime.rate_limiter(),
        fetch_policy: FetchPolicy::for_record(&site_settings, record),
        assets: None,
        objects: None,
        illustration_index: None,
        illustration_prefix: None,
        novel_record_resolver: None,
        fetch_policy_resolver: Some(Arc::new(move |record| {
            FetchPolicy::for_record(&site_settings, record)
        })),
    };
    ConvertService::new(
        runtime.objects(),
        runtime.assets(),
        runtime.settings_store(),
    )
    .with_capabilities(capabilities)
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

/// 変換済みテキストを保存するか (設定 map 版)。
///
/// 環境変数 `NAROU_RS_KEEP_TXT` (0/false/no/off) が最優先、次に local
/// 設定 (`convert.keep-txt`)。Worker は D1 を食わないよう **既定 false**
/// (保存しない)。`download.epub` が保持した設定 map をそのまま渡せる形。
pub(crate) fn keep_converted_text_with(
    local: &std::collections::HashMap<String, serde_yaml::Value>,
) -> bool {
    if let Ok(value) = std::env::var("NAROU_RS_KEEP_TXT") {
        let value = value.trim().to_ascii_lowercase();
        return !matches!(value.as_str(), "0" | "false" | "no" | "off");
    }
    match local.get("convert.keep-txt") {
        Some(serde_yaml::Value::Bool(value)) => *value,
        Some(serde_yaml::Value::String(value)) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        // 未設定の Worker は保存しない (native は既定 true)。
        _ => false,
    }
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
