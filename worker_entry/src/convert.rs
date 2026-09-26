//! Worker 内での変換ジョブ（`JobKind::Convert`）。
//!
//! 保存済みの TOC と本文から変換テキストを組み立て、`<prefix>/novel.txt` に
//! 書く。native の `convert_novel_by_id` が書く固定名ミラーと同じキーなので、
//! 続けて `download.epub` がそのまま配信できる（外部プロセスは使わない）。

use narou_rs::application::convert::{
    ConvertItemReport, ConvertService, ConvertedNovel, emit_convert_item_lines,
};
use narou_rs::application::jobs::{JobPlan, JobTarget};
use narou_rs::converter::ConverterCapabilities;
use narou_rs::downloader::http_policy::FetchPolicy;
use narou_rs::error::{NarouError, Result};

use crate::composition::WorkerRuntime;
use crate::executor::JobOutcome;
use crate::push_hub::{PushHubClient, PushHubSink};

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

    let push_sink = PushHubSink::install(push.clone());
    let report = emit_convert_item_lines(push_sink.as_ref(), &job.target.as_str(), target.0, || {
        convert(runtime, target)
    })
    .await;
    push_sink.drain().await;

    match report {
        ConvertItemReport::Written { .. } => JobOutcome::Succeeded,
        // レコード不在: ledger / queue_failed には従来通りのエラー文言を残す
        // (コンソール行は id_missing で既に出ている)。
        ConvertItemReport::Missing => JobOutcome::Permanent {
            reason: format!("小説 {} がありません", target.0),
        },
        ConvertItemReport::Failed { error } => classify(error),
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

    // native の CLI 変換と同じく、挿絵のローカライズ能力は渡さない
    // (`native::converter::native_capabilities` も assets/objects を None にする)。
    let capabilities = ConverterCapabilities {
        http: runtime.http_client(),
        rate_limiter: runtime.rate_limiter(),
        fetch_policy: FetchPolicy::default(),
        assets: None,
        objects: None,
        illustration_index: None,
        illustration_prefix: None,
        novel_record_resolver: None,
        fetch_policy_resolver: None,
    };

    ConvertService::new(
        runtime.objects(),
        runtime.assets(),
        runtime.settings_store(),
    )
    .with_capabilities(capabilities)
    .convert_and_store(&record)
    .await
    .map(Some)
}

fn classify(error: NarouError) -> JobOutcome {
    match &error {
        NarouError::Platform(_) | NarouError::Conversion(_) | NarouError::Yaml(_) => {
            JobOutcome::Permanent {
                reason: error.to_string(),
            }
        }
        _ => JobOutcome::Retryable {
            reason: error.to_string(),
        },
    }
}
