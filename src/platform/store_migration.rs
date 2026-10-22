//! D1 → S3 の挿絵移行（テスト可能なコア）。
//!
//! 保存先の振り分け (`SplitStore`) を入れたので、移行の対象は**挿絵のバイナリ
//! だけ**。本文・メタデータは D1 に残す。進捗はカーソルで持ち、途中で失敗して
//! も続きから再開できる。
//!
//! I/O は trait 越しに行うので、native では `MemoryObjectStore` で挙動を固定
//! でき、Worker 側は D1/S3 を挿して `app_state` に進捗を残すだけになる。
//!
//! ページ内の対象は独立したオブジェクトなので、小さいものだけ bounded な
//! ウィンドウで重ねる (`io_windows`)。1 オブジェクトをまとめてメモリへ
//! 載せる実装 (D1/S3) では、大きいものを重ねると peak buffer がそのまま
//! 倍になるため、大きい対象は前後と重ねない。

use std::num::NonZeroUsize;
use std::sync::Arc;

use futures::stream::{self, StreamExt};

use super::{
    AssetStore, ObjectListRequest, ObjectMetadata, ObjectPrefix, ObjectStore, S3Store,
    is_illustration_key,
};
use crate::error::{NarouError, Result};
use crate::illustration_store::is_sha256_hex_stem;

/// 1 回の呼び出しで扱う既定件数。
pub const DEFAULT_LIMIT: usize = 100;
/// 1 回の呼び出しの上限。Queue/HTTP の実行時間を守るため。
pub const MAX_LIMIT: usize = 500;
/// `verify` が 1 オブジェクトを突き合わせる上限 (bounded read と同値)。
pub const VERIFY_CAP: u64 = 16 * 1024 * 1024;

/// 並行ウィンドウに載せてよい 1 オブジェクトの上限。
///
/// D1 / S3 の `read_stream` / `write_stream` は 1 オブジェクトをまとめて
/// メモリへ載せる実装なので、これより大きいものは他の処理と重ねず、
/// 同時に抱えるバッファ数を増やさない。
pub(crate) const PARALLEL_MAX_OBJECT_BYTES: u64 = 4 * 1024 * 1024;
/// 並行ウィンドウの見積り合計バイト数の上限。
///
/// 1 オブジェクトは移行元・移行先のバッファを同時に持ちうるので見積りは
/// `2 × size`。in-flight の見積り合計がこれを超えないようにウィンドウを切る。
pub(crate) const PARALLEL_IN_FLIGHT_BYTES: u64 = 8 * 1024 * 1024;
/// 見積りが小さくても同時に進める処理数の上限。
pub(crate) const MAX_IN_FLIGHT: usize = 4;

/// 並行 I/O の 1 ウィンドウ (連続した対象範囲)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IoWindow {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl IoWindow {
    /// 同時に進める処理数。
    pub(crate) fn width(&self) -> usize {
        self.end - self.start
    }
}

/// 1 オブジェクトの同時バッファ見積り (移行元 + 移行先)。
fn io_cost(size: u64) -> u64 {
    size.saturating_mul(2)
}

/// 対象のサイズ列を、見積り上限を超えない連続ウィンドウに切る。
///
/// 上限超過、またはサイズ未報告 (0 = 未知) の対象は幅 1 の単独ウィンドウに
/// して前後と重ねない。小さいものは `MAX_IN_FLIGHT` と
/// `PARALLEL_IN_FLIGHT_BYTES` の両方に収まる範囲でまとめる。サイズは
/// 一覧メタデータの値を使い、追加の `stat` はしない。
pub(crate) fn io_windows(sizes: &[u64]) -> Vec<IoWindow> {
    let mut windows = Vec::new();
    let mut start = 0;
    while start < sizes.len() {
        if !is_small_known(sizes[start]) {
            windows.push(IoWindow {
                start,
                end: start + 1,
            });
            start += 1;
            continue;
        }
        let mut end = start + 1;
        let mut bytes = io_cost(sizes[start]);
        while end < sizes.len() && end - start < MAX_IN_FLIGHT {
            if !is_small_known(sizes[end]) {
                break;
            }
            let next = io_cost(sizes[end]);
            if bytes.saturating_add(next) > PARALLEL_IN_FLIGHT_BYTES {
                break;
            }
            bytes = bytes.saturating_add(next);
            end += 1;
        }
        windows.push(IoWindow { start, end });
        start = end;
    }
    windows
}

/// 前後と重ねても安全と言えるサイズか (既知かつ上限以内)。
fn is_small_known(size: u64) -> bool {
    size != 0
        && size <= PARALLEL_MAX_OBJECT_BYTES
        && io_cost(size) <= PARALLEL_IN_FLIGHT_BYTES
}

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

    // 対象はページ順のまま集める。サイズは一覧メタデータを使い、
    // キーごとの stat は呼ばない (見積りと実測を二重に取りに行かない)。
    let targets: Vec<&ObjectMetadata> = page
        .objects
        .iter()
        .filter(|metadata| is_illustration_key(&metadata.key))
        .collect();
    let sizes: Vec<u64> = targets.iter().map(|metadata| metadata.size).collect();

    match action {
        // 数えるだけで I/O しない。
        "plan" => {
            for metadata in &targets {
                state.planned += 1;
                state.planned_bytes += metadata.size;
            }
        }
        "copy" => {
            for window in io_windows(&sizes) {
                let slice = &targets[window.start..window.end];
                copy_window(from_assets, to_assets, slice, window.width(), state).await?;
            }
        }
        _ => {
            for window in io_windows(&sizes) {
                let slice = &targets[window.start..window.end];
                verify_window(from_objects, to_objects, slice, window.width(), state).await?;
            }
        }
    }

    state.cursor = page.next_cursor.clone();
    state.done = page.next_cursor.is_none();
    Ok(())
}

/// コピー 1 件の結果。
enum CopyOutcome {
    Copied,
    MissingInSource,
}

/// ウィンドウ内の対象を `width` 並行でコピーし、結果をキー順に記録する。
///
/// 途中で失敗したキー以降の結果は記録しない (再開時に同じページをやり直せ
/// る)。並行していても記録の順序は一覧順で決定的。
async fn copy_window(
    from_assets: &Arc<dyn AssetStore>,
    to_assets: &Arc<dyn AssetStore>,
    targets: &[&ObjectMetadata],
    width: usize,
    state: &mut StoreMigrationState,
) -> Result<()> {
    let results: Vec<(super::ObjectKey, Result<CopyOutcome>)> = stream::iter(
        targets.iter().map(|metadata| {
            let key = metadata.key.clone();
            let from_assets = Arc::clone(from_assets);
            let to_assets = Arc::clone(to_assets);
            async move {
                let outcome = copy_object(&from_assets, &to_assets, &key).await;
                (key, outcome)
            }
        }),
    )
    .buffered(width)
    .collect()
    .await;

    for (key, outcome) in results {
        match outcome? {
            CopyOutcome::Copied => state.record_copy(key.as_ref()),
            CopyOutcome::MissingInSource => {
                state.record_failure(format!("{}: missing in the source", key.as_ref()))
            }
        }
    }
    Ok(())
}

/// 1 オブジェクトを移行元から移行先へ写す。
///
/// 途中で切れたストリームは `write_stream` がエラーにするので、部分コピーを
/// 成功として数えない。
async fn copy_object(
    from_assets: &Arc<dyn AssetStore>,
    to_assets: &Arc<dyn AssetStore>,
    key: &super::ObjectKey,
) -> Result<CopyOutcome> {
    match from_assets.read_stream(key).await? {
        Some(stream) => {
            to_assets.write_stream(key, stream).await?;
            Ok(CopyOutcome::Copied)
        }
        None => Ok(CopyOutcome::MissingInSource),
    }
}

/// ウィンドウ内の対象を `width` 並行で突き合わせ、結果をキー順に記録する。
async fn verify_window(
    from: &Arc<dyn ObjectStore>,
    to: &Arc<dyn ObjectStore>,
    targets: &[&ObjectMetadata],
    width: usize,
    state: &mut StoreMigrationState,
) -> Result<()> {
    let results: Vec<(super::ObjectKey, Result<std::result::Result<(), String>>)> = stream::iter(
        targets.iter().map(|metadata| {
            let key = metadata.key.clone();
            let listed_size = metadata.size;
            let from = Arc::clone(from);
            let to = Arc::clone(to);
            async move {
                let outcome = verify_object(&from, &to, &key, listed_size).await;
                (key, outcome)
            }
        }),
    )
    .buffered(width)
    .collect()
    .await;

    for (key, outcome) in results {
        match outcome {
            Ok(Ok(())) => state.record_verified(key.as_ref()),
            Ok(Err(reason)) => state.record_failure(reason),
            Err(error) => state.record_failure(format!("{}: {error}", key.as_ref())),
        }
    }
    Ok(())
}

/// 1 オブジェクトを突き合わせる。バイト一致なら `Ok(())`。
///
/// 小さい対象は移行元・移行先の read を重ねて待ち時間を隠す (どのみち比較時点で
/// 2 バッファを持つので peak は増えない)。大きい対象は順に読み、バッファを
/// 同時に広げない。`listed_size` は一覧メタデータの値で、追加の `stat` は
/// 呼ばない。
async fn verify_object(
    from: &Arc<dyn ObjectStore>,
    to: &Arc<dyn ObjectStore>,
    key: &super::ObjectKey,
    listed_size: u64,
) -> Result<std::result::Result<(), String>> {
    if listed_size > VERIFY_CAP {
        return Ok(Err(format!("{}: exceeds the {VERIFY_CAP} byte verify cap", key.as_ref())));
    }
    let (source, target) = if is_small_known(listed_size) {
        let (source, target) = futures::join!(from.read_small(key), to.read_small(key));
        (source?, target?)
    } else {
        (from.read_small(key).await?, to.read_small(key).await?)
    };
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

/// 旧 `挿絵/` 配置の S3 挿絵を共通プールへ移す進捗。
///
/// `plan` は件数を数えるだけで書き込まない。`copy` は hex 名の挿絵だけを
/// 対象に、プールへ書き込んでから旧配置を削除する (旧配置の削除はプールに
/// 実体があることを確認した後)。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IllustrationDedupState {
    /// 最後に処理した論理キー。次回はこの続きから。
    pub cursor: Option<String>,
    /// 対象 (hex 名の旧配置) 件数。
    pub planned: u64,
    /// プールへ新たに移した件数。
    pub migrated: u64,
    /// 既にプールにあった件数。
    pub already_pooled: u64,
    /// hex 名でないため対象外にした件数 (mitemin の iNNN など)。
    pub skipped_non_hex: u64,
    pub failed: Vec<String>,
    pub done: bool,
}

impl IllustrationDedupState {
    /// `app_state` などに保存する前に失敗リストを上限で切る。
    pub fn trim_failures(&mut self, limit: usize) {
        if self.failed.len() > limit {
            self.failed.truncate(limit);
        }
    }

    fn record_failure(&mut self, message: String) {
        if self.failed.len() < 100 {
            self.failed.push(message);
        }
    }
}

/// 旧 `挿絵/` 配置の S3 オブジェクトを、全作品共通の
/// `illustrations/<base64url(sha256)>.<ext>` プールへ移す 1 ページ分。
///
/// `s3` は dedup 有効で構築されている必要がある。`action` は `plan` または
/// `copy`。`limit` は 1 回で走査する旧配置オブジェクト数で、進捗は `state` に
/// 残すので呼び出し側が `done` まで繰り返せる。
///
/// 対象は hex 名 (`<sha256-hex>.<ext>`) の旧配置だけ。mitemin の `iNNN` 名など
/// 内容ハッシュでない名前は、index の対応を壊さずには移せないため従来配置を
/// 保つ (新規取得分は dedup により最初から hex 名になる)。
pub async fn deduplicate_illustrations_page(
    s3: &S3Store,
    action: &str,
    limit: usize,
    state: &mut IllustrationDedupState,
) -> Result<()> {
    if action != "plan" && action != "copy" {
        return Err(NarouError::Platform(format!(
            "unknown illustration dedup action: {action:?}"
        )));
    }
    if state.done {
        return Ok(());
    }
    let limit = limit.clamp(1, MAX_LIMIT);
    let prefix =
        ObjectPrefix::new("novels").map_err(|error| NarouError::Platform(error.to_string()))?;
    let mut scanned = 0usize;
    while scanned < limit && !state.done {
        let request = ObjectListRequest::new(
            prefix.clone(),
            NonZeroUsize::new(limit - scanned).expect("remaining limit is at least 1"),
        );
        let request = match &state.cursor {
            Some(cursor) => request.after(cursor.clone()),
            None => request,
        };
        let page = s3.list_page(&request).await?;
        if page.objects.is_empty() {
            state.done = true;
            break;
        }
        for metadata in &page.objects {
            if scanned >= limit {
                break;
            }
            scanned += 1;
            let key = &metadata.key;
            if !is_illustration_key(key) {
                continue;
            }
            let file = key.as_ref().rsplit('/').next().unwrap_or("");
            let stem = file.rsplit_once('.').map(|(stem, _)| stem).unwrap_or("");
            if !is_sha256_hex_stem(stem) {
                state.skipped_non_hex += 1;
                continue;
            }
            state.planned += 1;
            if action == "plan" {
                continue;
            }

            let already_pooled = s3.pool_exists(key).await?;
            if already_pooled {
                state.already_pooled += 1;
            } else {
                let Some(stream) = s3.read_stream(key).await? else {
                    state.record_failure(format!("{}: source object missing", key.as_ref()));
                    continue;
                };
                if let Err(error) = s3.write_stream(key, stream).await {
                    state.record_failure(format!(
                        "{}: pool write failed: {error}",
                        key.as_ref()
                    ));
                    continue;
                }
                state.migrated += 1;
            }
            // プールに実体があることを確認してから旧配置を消す。delete は
            // dedup 無効時と同じく旧 `挿絵/` の物理キーだけを指す。
            if let Err(error) = ObjectStore::delete(s3, key).await {
                state.record_failure(format!("{}: legacy delete failed: {error}", key.as_ref()));
            }
        }
        state.cursor = page.next_cursor.clone();
        if state.cursor.is_none() {
            state.done = true;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{NarouError, Result};
    use crate::platform::mocks::MemoryObjectStore;
    use crate::platform::{
        AssetStore, AssetStream, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata,
        ObjectStore, PlatformFuture,
    };

    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use futures::stream;

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
            // 移行は写すだけ。移行元 (= 復帰先) からは何も消さない。
            assert!(d1_assets.read_stream(&picture).await.unwrap().is_some());
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

    /// ウィンドウが件数と見積りバイト数の両方で有界であること。
    #[test]
    fn io_windows_bound_parallelism_by_count_and_bytes() {
        // 小さい対象は件数の上限 (MAX_IN_FLIGHT) までまとまる。
        let windows = io_windows(&[10; 9]);
        assert_eq!(
            windows
                .iter()
                .map(|window| (window.start, window.end))
                .collect::<Vec<_>>(),
            vec![(0, 4), (4, 8), (8, 9)]
        );

        // 上限サイズを超える対象は単独ウィンドウになり、前後と重ならない。
        let large = PARALLEL_MAX_OBJECT_BYTES + 1;
        let windows = io_windows(&[10, large, 10, 10]);
        assert_eq!(
            windows
                .iter()
                .map(|window| (window.start, window.end))
                .collect::<Vec<_>>(),
            vec![(0, 1), (1, 2), (2, 4)]
        );

        // 単独で予算を使い切る対象 (上限サイズちょうど = 2×size が予算) も
        // 他の対象と重ねない。
        let windows = io_windows(&[PARALLEL_MAX_OBJECT_BYTES, 10, 10]);
        assert_eq!(
            windows
                .iter()
                .map(|window| (window.start, window.end))
                .collect::<Vec<_>>(),
            vec![(0, 1), (1, 3)]
        );

        // 累積見積りが予算を超える位置で切る。
        let half_budget = PARALLEL_IN_FLIGHT_BYTES / 4;
        let windows = io_windows(&[half_budget, half_budget, 10]);
        assert_eq!(
            windows
                .iter()
                .map(|window| (window.start, window.end))
                .collect::<Vec<_>>(),
            vec![(0, 2), (2, 3)]
        );

        // サイズ未報告 (0) は未知として単独で処理する。
        let windows = io_windows(&[0, 10, 0]);
        assert_eq!(
            windows
                .iter()
                .map(|window| (window.start, window.end))
                .collect::<Vec<_>>(),
            vec![(0, 1), (1, 2), (2, 3)]
        );
    }

    // ---- 並行度の観測用ストア ----

    /// 同時に in-flight だった読みの数。
    #[derive(Default)]
    struct ProbeCounters {
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    impl ProbeCounters {
        fn max_active(&self) -> usize {
            self.max_active.load(Ordering::SeqCst)
        }
    }

    /// 観測区間の間だけ `active` を数えるガード。
    struct ActiveGuard(Arc<ProbeCounters>);

    impl ActiveGuard {
        fn enter(counters: Arc<ProbeCounters>) -> Self {
            let active = counters.active.fetch_add(1, Ordering::SeqCst) + 1;
            counters.max_active.fetch_max(active, Ordering::SeqCst);
            Self(counters)
        }
    }

    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// 一度 Pending を返して自分を起こす協調 yield (executor 非依存)。
    ///
    /// これで「まだ完了していない読み」を作り、重なりを観測可能にする。
    fn yield_once() -> impl Future<Output = ()> {
        let mut yielded = false;
        futures::future::poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    /// チャンク間に一度協調 yield (Pending + self-wake) を挟むストリーム。
    ///
    /// 消費中ずっと `active` を数え、まだ完了していない読みを作ることで
    /// ウィンドウの重なりを観測できるようにする。Unpin なので Box 化して
    /// `AssetStream` として扱える。
    struct YieldingChunks {
        chunks: std::vec::IntoIter<Result<Vec<u8>>>,
        yield_next: bool,
        _guard: ActiveGuard,
    }

    impl futures::Stream for YieldingChunks {
        type Item = Result<Vec<u8>>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            if self.yield_next {
                self.yield_next = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            match self.chunks.next() {
                Some(item) => {
                    self.yield_next = true;
                    Poll::Ready(Some(item))
                }
                None => Poll::Ready(None),
            }
        }
    }

    /// メモリ上のストアに、読みの同時数カウント・一覧サイズの差し替え・
    /// 遅いキーのログを足したテスト用ストア。
    struct ProbeStore {
        inner: MemoryObjectStore,
        counters: Arc<ProbeCounters>,
        /// 0 以外なら `list_page` が報告するサイズをこの値に差し替える
        /// (大きい対象の扱いを実際のメモリを使わず検証する)。
        reported_size: u64,
        /// このキーの `read_small` は追加の self-wake を挟み、遅く完了する。
        slow_keys: Vec<String>,
        /// `read_small` が完了したキー (完了順)。
        resolved: parking_lot::Mutex<Vec<String>>,
    }

    impl ProbeStore {
        fn new(counters: Arc<ProbeCounters>) -> Self {
            Self {
                inner: MemoryObjectStore::new(),
                counters,
                reported_size: 0,
                slow_keys: Vec::new(),
                resolved: parking_lot::Mutex::new(Vec::new()),
            }
        }
    }

    impl ObjectStore for ProbeStore {
        fn stat<'a>(
            &'a self,
            key: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
            ObjectStore::stat(&self.inner, key)
        }

        fn read_small<'a>(
            &'a self,
            key: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
            let counters = Arc::clone(&self.counters);
            let slow = self.slow_keys.iter().any(|slow| slow.as_str() == key.as_ref());
            Box::pin(async move {
                let _guard = ActiveGuard::enter(counters);
                yield_once().await;
                if slow {
                    for _ in 0..3 {
                        yield_once().await;
                    }
                }
                let data = self.inner.read_small(key).await;
                self.resolved.lock().push(key.as_ref().to_string());
                data
            })
        }

        fn write_small<'a>(
            &'a self,
            key: &'a ObjectKey,
            data: Vec<u8>,
        ) -> PlatformFuture<'a, Result<()>> {
            self.inner.write_small(key, data)
        }

        fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
            ObjectStore::delete(&self.inner, key)
        }

        fn list_page<'a>(
            &'a self,
            request: &'a ObjectListRequest,
        ) -> PlatformFuture<'a, Result<ObjectListPage>> {
            let reported = self.reported_size;
            Box::pin(async move {
                let page = self.inner.list_page(request).await?;
                if reported == 0 {
                    return Ok(page);
                }
                Ok(ObjectListPage {
                    objects: page
                        .objects
                        .into_iter()
                        .map(|mut metadata| {
                            metadata.size = reported;
                            metadata
                        })
                        .collect(),
                    next_cursor: page.next_cursor,
                })
            })
        }
    }

    impl AssetStore for ProbeStore {
        fn stat<'a>(
            &'a self,
            key: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
            AssetStore::stat(&self.inner, key)
        }

        fn read_stream<'a>(
            &'a self,
            key: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
            let counters = Arc::clone(&self.counters);
            Box::pin(async move {
                let Some(bytes) = self.inner.read_small(key).await? else {
                    return Ok(None);
                };
                let guard = ActiveGuard::enter(counters);
                let chunks: Vec<Result<Vec<u8>>> = bytes
                    .chunks(4 * 1024)
                    .map(|chunk| Ok(chunk.to_vec()))
                    .collect();
                Ok(Some(
                    Box::pin(YieldingChunks {
                        chunks: chunks.into_iter(),
                        yield_next: false,
                        _guard: guard,
                    }) as AssetStream,
                ))
            })
        }

        fn write_stream<'a>(
            &'a self,
            key: &'a ObjectKey,
            stream: AssetStream,
        ) -> PlatformFuture<'a, Result<()>> {
            self.inner.write_stream(key, stream)
        }

        fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
            AssetStore::delete(&self.inner, key)
        }

        fn copy<'a>(
            &'a self,
            source: &'a ObjectKey,
            destination: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<()>> {
            self.inner.copy(source, destination)
        }

        fn move_or_copy<'a>(
            &'a self,
            source: &'a ObjectKey,
            destination: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<()>> {
            self.inner.move_or_copy(source, destination)
        }
    }

    /// 指定キーの `read_stream` だけ「1 チャンク後にエラー」にする。
    struct FailingAssets {
        inner: Arc<ProbeStore>,
        fail_key: ObjectKey,
    }

    impl AssetStore for FailingAssets {
        fn stat<'a>(
            &'a self,
            key: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
            AssetStore::stat(self.inner.as_ref(), key)
        }

        fn read_stream<'a>(
            &'a self,
            key: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
            if key == &self.fail_key {
                Box::pin(async {
                    let stream: AssetStream = Box::pin(stream::iter(vec![
                        Ok(b"partial".to_vec()),
                        Err(NarouError::Platform("injected read failure".into())),
                    ]));
                    Ok(Some(stream))
                })
            } else {
                AssetStore::read_stream(self.inner.as_ref(), key)
            }
        }

        fn write_stream<'a>(
            &'a self,
            key: &'a ObjectKey,
            stream: AssetStream,
        ) -> PlatformFuture<'a, Result<()>> {
            AssetStore::write_stream(self.inner.as_ref(), key, stream)
        }

        fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
            AssetStore::delete(self.inner.as_ref(), key)
        }

        fn copy<'a>(
            &'a self,
            source: &'a ObjectKey,
            destination: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<()>> {
            AssetStore::copy(self.inner.as_ref(), source, destination)
        }

        fn move_or_copy<'a>(
            &'a self,
            source: &'a ObjectKey,
            destination: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<()>> {
            AssetStore::move_or_copy(self.inner.as_ref(), source, destination)
        }
    }

    fn illustrated_key(index: usize) -> super::super::ObjectKey {
        key(&format!("novels/site/title/挿絵/{index:04}.jpg"))
    }

    /// 小さい対象は重なり、in-flight 上限を超えないこと。
    #[test]
    fn copy_overlaps_small_objects_within_the_in_flight_bound() {
        futures::executor::block_on(async {
            let counters = Arc::new(ProbeCounters::default());
            let source = Arc::new(ProbeStore::new(Arc::clone(&counters)));
            let destination = Arc::new(MemoryObjectStore::new());
            for index in 0..6 {
                source
                    .inner
                    .write_small(&illustrated_key(index), vec![b'x'; 8 * 1024])
                    .await
                    .unwrap();
            }
            let from_objects: Arc<dyn ObjectStore> = source.clone();
            let from_assets: Arc<dyn AssetStore> = source.clone();
            let to_objects: Arc<dyn ObjectStore> = destination.clone();
            let to_assets: Arc<dyn AssetStore> = destination.clone();

            let mut state = StoreMigrationState::default();
            migrate_page(
                &from_objects,
                &from_assets,
                &to_assets,
                &to_objects,
                "copy",
                100,
                &mut state,
            )
            .await
            .unwrap();

            assert_eq!(state.copied, 6);
            assert!(
                counters.max_active() >= 2,
                "small objects should overlap: {}",
                counters.max_active()
            );
            assert!(
                counters.max_active() <= MAX_IN_FLIGHT,
                "overlap must stay bounded: {}",
                counters.max_active()
            );
        });
    }

    /// 一覧が大きいと報告する対象は幅 1 で処理され、重ならないこと。
    #[test]
    fn large_listed_objects_are_processed_sequentially() {
        futures::executor::block_on(async {
            let counters = Arc::new(ProbeCounters::default());
            let mut source = ProbeStore::new(Arc::clone(&counters));
            source.reported_size = PARALLEL_MAX_OBJECT_BYTES + 1;
            let source = Arc::new(source);
            let destination = Arc::new(MemoryObjectStore::new());
            for index in 0..3 {
                source
                    .inner
                    .write_small(&illustrated_key(index), vec![b'x'; 8 * 1024])
                    .await
                    .unwrap();
            }
            let from_objects: Arc<dyn ObjectStore> = source.clone();
            let from_assets: Arc<dyn AssetStore> = source.clone();
            let to_objects: Arc<dyn ObjectStore> = destination.clone();
            let to_assets: Arc<dyn AssetStore> = destination.clone();

            let mut state = StoreMigrationState::default();
            migrate_page(
                &from_objects,
                &from_assets,
                &to_assets,
                &to_objects,
                "copy",
                100,
                &mut state,
            )
            .await
            .unwrap();

            assert_eq!(state.copied, 3);
            assert_eq!(
                counters.max_active(),
                1,
                "large-listed objects must not overlap"
            );
        });
    }

    /// 途中で切れたコピーは成功として数えず、同じページからやり直せること。
    #[test]
    fn partial_copy_failure_is_not_counted_and_resumes() {
        futures::executor::block_on(async {
            let counters = Arc::new(ProbeCounters::default());
            let source = Arc::new(ProbeStore::new(Arc::clone(&counters)));
            let destination = Arc::new(MemoryObjectStore::new());
            let first = illustrated_key(1);
            let second = illustrated_key(2);
            source
                .inner
                .write_small(&first, b"first".to_vec())
                .await
                .unwrap();
            source
                .inner
                .write_small(&second, b"second".to_vec())
                .await
                .unwrap();

            let from_objects: Arc<dyn ObjectStore> = source.clone();
            let to_objects: Arc<dyn ObjectStore> = destination.clone();
            let to_assets: Arc<dyn AssetStore> = destination.clone();
            let failing: Arc<dyn AssetStore> = Arc::new(FailingAssets {
                inner: Arc::clone(&source),
                fail_key: first.clone(),
            });

            let mut state = StoreMigrationState::default();
            let result = migrate_page(
                &from_objects,
                &failing,
                &to_assets,
                &to_objects,
                "copy",
                100,
                &mut state,
            )
            .await;
            assert!(result.is_err(), "a broken stream must abort the page");
            assert_eq!(state.copied, 0, "a partial copy must not count as success");
            assert!(!state.done);
            assert!(
                destination.read_small(&first).await.unwrap().is_none(),
                "the broken object must not be published"
            );

            // 同じページの先頭からやり直すと、まだ数えていない分だけが進む。
            let healthy: Arc<dyn AssetStore> = source.clone();
            migrate_page(
                &from_objects,
                &healthy,
                &to_assets,
                &to_objects,
                "copy",
                1,
                &mut state,
            )
            .await
            .unwrap();
            assert_eq!(state.copied, 1, "resume restarts at the failed page");
            migrate_page(
                &from_objects,
                &healthy,
                &to_assets,
                &to_objects,
                "copy",
                1,
                &mut state,
            )
            .await
            .unwrap();
            assert_eq!(state.copied, 2);
            assert!(destination.read_small(&first).await.unwrap().is_some());
            assert!(destination.read_small(&second).await.unwrap().is_some());
        });
    }

    /// verify は小さい対象で移行元・移行先の read を重ねること。
    #[test]
    fn verify_overlaps_source_and_target_reads_for_small_objects() {
        futures::executor::block_on(async {
            let counters = Arc::new(ProbeCounters::default());
            let source = Arc::new(ProbeStore::new(Arc::clone(&counters)));
            let target = Arc::new(ProbeStore::new(Arc::clone(&counters)));
            let picture = illustrated_key(1);
            source
                .inner
                .write_small(&picture, b"same".to_vec())
                .await
                .unwrap();
            target
                .inner
                .write_small(&picture, b"same".to_vec())
                .await
                .unwrap();

            let from_objects: Arc<dyn ObjectStore> = source.clone();
            let from_assets: Arc<dyn AssetStore> = source.clone();
            let to_objects: Arc<dyn ObjectStore> = target.clone();
            let to_assets: Arc<dyn AssetStore> = target.clone();

            let mut state = StoreMigrationState::default();
            migrate_page(
                &from_objects,
                &from_assets,
                &to_assets,
                &to_objects,
                "verify",
                100,
                &mut state,
            )
            .await
            .unwrap();

            assert_eq!(state.verified, 1);
            assert_eq!(
                counters.max_active(),
                2,
                "source and target reads should overlap"
            );
        });
    }

    /// verify は大きい対象を source → target の順に読むこと。
    #[test]
    fn verify_reads_large_listed_objects_sequentially() {
        futures::executor::block_on(async {
            let counters = Arc::new(ProbeCounters::default());
            let mut source = ProbeStore::new(Arc::clone(&counters));
            source.reported_size = PARALLEL_MAX_OBJECT_BYTES + 1;
            let source = Arc::new(source);
            // verify の見積りは移行元の一覧だけを見る (target 側の一覧は使わない)。
            let target = Arc::new(ProbeStore::new(Arc::clone(&counters)));
            let picture = illustrated_key(1);
            source
                .inner
                .write_small(&picture, b"same".to_vec())
                .await
                .unwrap();
            target
                .inner
                .write_small(&picture, b"same".to_vec())
                .await
                .unwrap();

            let from_objects: Arc<dyn ObjectStore> = source.clone();
            let from_assets: Arc<dyn AssetStore> = source.clone();
            let to_objects: Arc<dyn ObjectStore> = target.clone();
            let to_assets: Arc<dyn AssetStore> = target.clone();

            let mut state = StoreMigrationState::default();
            migrate_page(
                &from_objects,
                &from_assets,
                &to_assets,
                &to_objects,
                "verify",
                100,
                &mut state,
            )
            .await
            .unwrap();

            assert_eq!(state.verified, 1);
            assert_eq!(counters.max_active(), 1, "large reads must not overlap");
        });
    }

    /// 完了順が入れ替わっても、失敗は一覧順で記録されること。
    #[test]
    fn verify_records_failures_in_listing_order() {
        futures::executor::block_on(async {
            let counters = Arc::new(ProbeCounters::default());
            let mut source = ProbeStore::new(Arc::clone(&counters));
            let target = Arc::new(ProbeStore::new(Arc::clone(&counters)));
            let first = illustrated_key(1);
            let second = illustrated_key(2);
            source
                .inner
                .write_small(&first, b"one".to_vec())
                .await
                .unwrap();
            source
                .inner
                .write_small(&second, b"two".to_vec())
                .await
                .unwrap();
            target
                .inner
                .write_small(&first, b"other".to_vec())
                .await
                .unwrap();
            // 0001 の source read だけを遅らせ、完了順を入れ替える。
            source.slow_keys = vec![first.as_ref().to_string()];
            let source = Arc::new(source);

            let from_objects: Arc<dyn ObjectStore> = source.clone();
            let from_assets: Arc<dyn AssetStore> = source.clone();
            let to_objects: Arc<dyn ObjectStore> = target.clone();
            let to_assets: Arc<dyn AssetStore> = target.clone();

            let mut state = StoreMigrationState::default();
            migrate_page(
                &from_objects,
                &from_assets,
                &to_assets,
                &to_objects,
                "verify",
                100,
                &mut state,
            )
            .await
            .unwrap();

            assert_eq!(state.verified, 0);
            let resolved = source.resolved.lock().clone();
            assert_eq!(
                resolved.first().map(String::as_str),
                Some(second.as_ref()),
                "the second object should resolve first: {resolved:?}"
            );
            assert_eq!(
                state.failed,
                vec![
                    format!(
                        "{}: content differs (source 3 bytes, target 5 bytes)",
                        first.as_ref()
                    ),
                    format!("{}: missing in the target", second.as_ref()),
                ]
            );
        });
    }
}
