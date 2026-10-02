//! D1 → S3 の挿絵移行（テスト可能なコア）。
//!
//! 保存先の振り分け (`SplitStore`) を入れたので、移行の対象は**挿絵のバイナリ
//! だけ**。本文・メタデータは D1 に残す。進捗はカーソルで持ち、途中で失敗して
//! も続きから再開できる。
//!
//! I/O は trait 越しに行うので、native では `MemoryObjectStore` で挙動を固定
//! でき、Worker 側は D1/S3 を挿して `app_state` に進捗を残すだけになる。

use std::num::NonZeroUsize;
use std::sync::Arc;

use super::{
    AssetStore, ObjectListRequest, ObjectPrefix, ObjectStore, is_illustration_key,
};
use crate::error::{NarouError, Result};

/// 1 回の呼び出しで扱う既定件数。
pub const DEFAULT_LIMIT: usize = 100;
/// 1 回の呼び出しの上限。Queue/HTTP の実行時間を守るため。
pub const MAX_LIMIT: usize = 500;
/// `verify` が 1 オブジェクトを突き合わせる上限 (bounded read と同値)。
pub const VERIFY_CAP: u64 = 16 * 1024 * 1024;

/// 移行の進捗。`app_state` にそのまま置ける形。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoreMigrationState {
    /// 最後に処理した位置（次回はこの続きから）。
    pub cursor: Option<String>,
    pub copied: u64,
    pub verified: u64,
    /// `plan` が数えた「移行対象の件数」（書き込みはしない）。
    pub planned: u64,
    /// `plan` が数えた対象の合計バイト数。
    pub planned_bytes: u64,
    pub failed: Vec<String>,
    pub done: bool,
}

impl StoreMigrationState {
    /// 対象キーを 1 つ処理したことを記録する。
    pub fn record_copy(&mut self, key: &str) {
        self.copied += 1;
        self.failed.retain(|entry| !entry.starts_with(key));
    }

    pub fn record_verified(&mut self, key: &str) {
        self.verified += 1;
        self.failed.retain(|entry| !entry.starts_with(key));
    }

    pub fn record_failure(&mut self, entry: String) {
        if !self.failed.iter().any(|existing| existing == &entry) {
            self.failed.push(entry);
        }
    }
}

/// 移行の 1 ページ分を進める。
///
/// `from_objects`/`from_assets` が移行元 (D1)、`to_objects`/`to_assets` が
/// 移行先 (S3)。挿絵のキーだけを対象にし、それ以外は数えずに読み飛ばす。
pub async fn migrate_page(
    from_objects: &Arc<dyn ObjectStore>,
    from_assets: &Arc<dyn AssetStore>,
    to_assets: &Arc<dyn AssetStore>,
    to_objects: &Arc<dyn ObjectStore>,
    action: &str,
    limit: usize,
    state: &mut StoreMigrationState,
) -> Result<()> {
    if action != "copy" && action != "verify" && action != "plan" {
        return Err(NarouError::Platform(format!(
            "unknown store migration action: {action:?}"
        )));
    }
    let limit = limit.clamp(1, MAX_LIMIT);
    let prefix = ObjectPrefix::new("").map_err(|error| NarouError::Platform(error.to_string()))?;
    let page = from_objects
        .list_page(&ObjectListRequest {
            prefix,
            cursor: state.cursor.clone(),
            limit: NonZeroUsize::new(limit).expect("limit is at least 1"),
        })
        .await?;

    for metadata in &page.objects {
        let key = &metadata.key;
        if !is_illustration_key(key) {
            continue;
        }
        if action == "plan" {
            state.planned += 1;
            state.planned_bytes += metadata.size;
            continue;
        }
        if action == "copy" {
            match from_assets.read_stream(key).await? {
                Some(stream) => {
                    to_assets.write_stream(key, stream).await?;
                    state.record_copy(key.as_ref());
                }
                None => state.record_failure(format!("{}: missing in the source", key.as_ref())),
            }
        } else {
            match verify_object(from_objects, to_objects, key).await? {
                Ok(()) => state.record_verified(key.as_ref()),
                Err(reason) => state.record_failure(reason),
            }
        }
    }

    state.cursor = page.next_cursor.clone();
    state.done = page.next_cursor.is_none();
    Ok(())
}

/// 1 オブジェクトを突き合わせる。バイト一致なら `Ok(())`。
async fn verify_object(
    from: &Arc<dyn ObjectStore>,
    to: &Arc<dyn ObjectStore>,
    key: &super::ObjectKey,
) -> Result<std::result::Result<(), String>> {
    let source = from.read_small(key).await?;
    let target = to.read_small(key).await?;
    match (source, target) {
        (Some(source), Some(target)) => {
            if source.len() as u64 > VERIFY_CAP {
                return Ok(Err(format!(
                    "{}: exceeds the {VERIFY_CAP} byte verify cap",
                    key.as_ref()
                )));
            }
            if source == target {
                Ok(Ok(()))
            } else {
                Ok(Err(format!(
                    "{}: content differs (source {} bytes, target {} bytes)",
                    key.as_ref(),
                    source.len(),
                    target.len()
                )))
            }
        }
        (Some(_), None) => Ok(Err(format!("{}: missing in the target", key.as_ref()))),
        (None, _) => Ok(Err(format!("{}: missing in the source", key.as_ref()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::mocks::MemoryObjectStore;

    fn key(value: &str) -> super::super::ObjectKey {
        super::super::ObjectKey::try_new(value).unwrap()
    }

    async fn write(store: &dyn ObjectStore, key: &super::super::ObjectKey, data: &[u8]) {
        store.write_small(key, data.to_vec()).await.unwrap();
    }

    /// 挿絵だけがコピーされ、本文・メタデータは移行元に残ること。
    #[test]
    fn copy_moves_only_illustrations() {
        futures::executor::block_on(async {
            // 1 つのストアを ObjectStore / AssetStore の両方として使う
            // (本番の D1・S3 も同じ実体を共有する)。
            let d1_store = Arc::new(MemoryObjectStore::new());
            let d1: Arc<dyn ObjectStore> = d1_store.clone();
            let d1_assets: Arc<dyn AssetStore> = d1_store;
            let s3_store = Arc::new(MemoryObjectStore::new());
            let s3: Arc<dyn ObjectStore> = s3_store.clone();
            let s3_assets: Arc<dyn AssetStore> = s3_store;

            let picture = key("novels/site/title/挿絵/0001.jpg");
            let section = key("novels/site/title/本文/1 x.yaml");
            let cache = key("novels/site/title/.illustration_cache.yaml");
            // 挿絵は AssetStore (ストリーム)、それ以外は ObjectStore に書く。
            d1_assets
                .write_stream(
                    &picture,
                    Box::pin(futures::stream::iter(vec![Ok(b"picture".to_vec())])),
                )
                .await
                .unwrap();
            for key in [&section, &cache] {
                d1.write_small(key, b"data".to_vec()).await.unwrap();
            }

            let mut state = StoreMigrationState::default();
            migrate_page(&d1, &d1_assets, &s3_assets, &s3, "copy", 100, &mut state)
                .await
                .unwrap();

            assert_eq!(state.copied, 1, "only the illustration should move");
            assert!(state.done);
            assert!(s3_assets.read_stream(&picture).await.unwrap().is_some());
            assert!(s3.read_small(&section).await.unwrap().is_none());
            assert!(s3.read_small(&cache).await.unwrap().is_none());
            assert!(d1.read_small(&section).await.unwrap().is_some());
        });
    }

    /// `plan` は対象を数えるだけで書き込まないこと。
    #[test]
    fn plan_counts_targets_without_writing() {
        futures::executor::block_on(async {
            let d1_store = Arc::new(MemoryObjectStore::new());
            let d1: Arc<dyn ObjectStore> = d1_store.clone();
            let d1_assets: Arc<dyn AssetStore> = d1_store.clone();
            let s3_store = Arc::new(MemoryObjectStore::new());
            let s3: Arc<dyn ObjectStore> = s3_store.clone();
            let s3_assets: Arc<dyn AssetStore> = s3_store;

            let picture = key("novels/site/title/挿絵/0001.jpg");
            d1.write_small(&picture, b"picture".to_vec()).await.unwrap();

            let mut state = StoreMigrationState::default();
            migrate_page(&d1, &d1_assets, &s3_assets, &s3, "plan", 100, &mut state)
                .await
                .unwrap();

            assert_eq!(state.planned, 1);
            assert_eq!(state.planned_bytes, 7);
            assert_eq!(state.copied, 0);
            assert!(s3.read_small(&picture).await.unwrap().is_none());
        });
    }

    /// verify が欠落と内容不一致を報告すること。
    #[test]
    fn verify_reports_missing_and_differing_objects() {
        futures::executor::block_on(async {
            let d1: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
            let s3: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
            let d1_assets: Arc<dyn AssetStore> = Arc::new(MemoryObjectStore::new());
            let s3_assets: Arc<dyn AssetStore> = Arc::new(MemoryObjectStore::new());

            let picture = key("novels/site/title/挿絵/0001.jpg");
            write(d1.as_ref(), &picture, b"picture").await;
            let missing = key("novels/site/title/挿絵/0002.jpg");
            write(d1.as_ref(), &missing, b"missing").await;
            write(s3.as_ref(), &picture, b"other").await;

            let mut state = StoreMigrationState::default();
            migrate_page(&d1, &d1_assets, &s3_assets, &s3, "verify", 100, &mut state)
                .await
                .unwrap();

            assert_eq!(state.verified, 0);
            assert_eq!(state.failed.len(), 2, "{:?}", state.failed);
            assert!(state.failed.iter().any(|entry| entry.contains("differs")));
            assert!(
                state
                    .failed
                    .iter()
                    .any(|entry| entry.contains("missing in the target"))
            );
        });
    }
}
