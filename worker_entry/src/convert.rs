//! Worker 内での変換ジョブ（`JobKind::Convert`）。
//!
//! 保存済みの TOC と本文から変換テキストを組み立て、`<prefix>/novel.txt` に
//! 書く。native の `convert_novel_by_id` が書く固定名ミラーと同じキーなので、
//! 続けて `download.epub` がそのまま配信できる（外部プロセスは使わない）。

use narou_rs::application::convert::ConvertService;
use narou_rs::application::jobs::{JobPlan, JobTarget};
use narou_rs::converter::ConverterCapabilities;
use narou_rs::downloader::http_policy::FetchPolicy;
use narou_rs::error::{NarouError, Result};

use crate::composition::WorkerRuntime;
use crate::executor::JobOutcome;

/// `Convert` プランを実行する。対象は単一小説の id のみ。
pub async fn execute_convert(runtime: &WorkerRuntime, job: &JobPlan) -> JobOutcome {
    let target = match job.target {
        JobTarget::Id(id) => id,
        _ => {
            return JobOutcome::Permanent {
                reason: "convert requires a numeric novel id".to_string(),
            };
        }
    };

    match convert(runtime, target).await {
        Ok(_) => JobOutcome::Succeeded,
        Err(error) => classify(error),
    }
}

async fn convert(runtime: &WorkerRuntime, id: narou_rs::platform::NovelId) -> Result<String> {
    let record = runtime
        .services
        .library
        .get(id)
        .await
        .map_err(|error| NarouError::Platform(error.to_string()))?
        .ok_or_else(|| NarouError::Platform(format!("小説 {} がありません", id.0)))?;

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
