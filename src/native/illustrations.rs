//! S3 に置いた挿絵をローカルの `挿絵/` へ取り出す。
//!
//! EPUB 生成はファイルパスを要求する (組み込み Lite も外部 AozoraEpub3 も
//! 小説ディレクトリを読む)。挿絵を S3 に置く構成 (`s3.asset-backend=s3`) では
//! ローカルに実体が無いので、EPUB を作る直前にだけ取り出す。
//!
//! 取り出したファイルは同じ小説を読む最後のガードが終了してから消す。
//! 本文と挿絵は同じ共有スコープを使い、同時の EPUB 要求や変換で利用中の
//! ファイルを先に消さない。元からあるユーザーファイルは片付けの対象外。

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures::StreamExt;

use crate::error::{NarouError, Result};
use crate::platform::split_store::ILLUSTRATION_SEGMENT;
use crate::platform::store_migration::io_windows;
use crate::platform::{
    AssetStore, ObjectKey, ObjectListRequest, ObjectMetadata, ObjectPrefix, ObjectStore,
};

/// 小説ディレクトリ 1 件分の挿絵を取り出す。
///
/// 挿絵がローカルにある構成では、対象が既に存在するので何も取り出さない
/// (戻り値は空)。
pub async fn materialize(novel_dir: &Path) -> Result<Materialized> {
    let inventory = crate::db::inventory::Inventory::with_default_root()?;
    let archive_root = inventory
        .root_dir()
        .join(crate::downloader::types::ARCHIVE_ROOT_DIR);
    let Some(novel_key) =
        crate::native::object_store::logical_key_for_native_path(&archive_root, novel_dir)
    else {
        return Ok(Materialized::default());
    };
    let scope = crate::native::materialization::MaterializationScope::acquire(inventory.root_dir(), novel_dir)?;
    let stores = crate::native::object_store::NativeStores::for_narou_root(inventory.root_dir())?;
    let prefix = ObjectPrefix::new(novel_key.join(ILLUSTRATION_SEGMENT)?.as_ref())?;
    let local_dir = novel_dir.join(ILLUSTRATION_SEGMENT);

    materialize_from_stores(
        stores.objects.as_ref(),
        stores.assets.as_ref(),
        prefix,
        &local_dir,
        scope,
    )
    .await
}

/// ページを bounded なウィンドウ単位で取り出す。
///
/// - ウィンドウ: 一覧メタデータのサイズから `io_windows` が決める。大きい
///   オブジェクトは幅 1 になり、前後と重ならない。
/// - 公開: 書き込みは一意な一時名で行い、rename で final 名へ移す。生成途中の
///   ファイルが final 名で見えないので、競合する読み手や別プロセスが部分
///   ファイルを完成品と誤認しない。rename が既存ファイルと衝突したら相手の
///   完成品を優先し、自分の一時ファイルだけを捨てる。
/// - 片付け: この関数が公開したファイルだけが `Materialized` に入る。途中の
///   エラーやキャンセルでは、公開済みはガードが、書き込み中は `StagedFile` が
///   それぞれ自分が作ったものだけを消す。
async fn materialize_from_stores(
    objects: &dyn ObjectStore,
    assets: &dyn AssetStore,
    prefix: ObjectPrefix,
    local_dir: &Path,
    scope: Arc<crate::native::materialization::MaterializationScope>,
) -> Result<Materialized> {
    // Own every created file before another fallible operation. Failed downloads
    // must not leave a partial file that a later conversion mistakes for a mirror.
    let mut materialized = Materialized { paths: Vec::new(), _scope: Some(scope) };
    let mut cursor: Option<String> = None;
    loop {
        let mut request = ObjectListRequest::new(prefix.clone(), NonZeroUsize::new(512).unwrap());
        if let Some(cursor) = cursor {
            request = request.after(cursor);
        }
        let page = objects.list_page(&request).await?;
        let targets: Vec<(ObjectKey, u64)> = page
            .objects
            .iter()
            .map(|metadata| (metadata.key.clone(), metadata.size))
            .collect();
        let sizes: Vec<u64> = targets.iter().map(|(_, size)| *size).collect();
        for window in io_windows(&sizes) {
            let slice = &targets[window.start..window.end];
            let mut pending = Vec::with_capacity(slice.len());
            for (key, _) in slice {
                pending.push(stage_one(assets, key, local_dir));
            }
            let results: Vec<Result<Option<(PathBuf, StagedFile)>>> = futures::stream::iter(pending)
                .buffered(window.width())
                .collect()
                .await;
            // ここから先は await を挟まずに公開する (キャンセルが公開と登録を
            // 分断しない)。公開したパスはすぐガードへ登録し、公開できなかった
            // 一時ファイルは StagedFile の Drop が片付ける。
            let mut failure: Option<NarouError> = None;
            for result in results {
                match result {
                    Ok(Some((path, mut staged))) => match staged.publish(&path) {
                        Ok(true) => {
                            materialized._scope.as_ref().unwrap().register(&path);
                            materialized.paths.push(path);
                        }
                        // 競合した書き手の完成品をそのまま使う。
                        Ok(false) => {}
                        Err(error) => {
                            if failure.is_none() {
                                failure = Some(error);
                            }
                        }
                    },
                    Ok(None) => {}
                    Err(error) => {
                        if failure.is_none() {
                            failure = Some(error);
                        }
                    }
                }
            }
            if let Some(error) = failure {
                return Err(error);
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(materialized)
}

/// 1 つの挿絵を一時名で書き終える。公開 (rename) は収集側が行う。
///
/// `Ok(None)` は既にローカルにあるか、ストアに無い場合 (何も作っていない)。
async fn stage_one(
    assets: &dyn AssetStore,
    key: &ObjectKey,
    local_dir: &Path,
) -> Result<Option<(PathBuf, StagedFile)>> {
    let Some(filename) = key.as_ref().rsplit('/').next() else {
        return Ok(None);
    };
    if filename.is_empty() {
        return Ok(None);
    }
    let path = local_dir.join(filename);
    if path.is_file() {
        return Ok(None);
    }
    let Some(mut stream) = assets.read_stream(key).await? else {
        return Ok(None);
    };
    std::fs::create_dir_all(local_dir)?;
    let staged = StagedFile::new(temp_path_for(&path));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(staged.path())?;
    while let Some(chunk) = stream.next().await {
        file.write_all(&chunk?)?;
    }
    file.flush()?;
    drop(file);
    Ok(Some((path, staged)))
}

/// プロセスごとに一度だけ決まる一時名のサフィックス。
///
/// プロセス間の衝突と、pid 再利用で古い一時ファイルとぶつかる事故を避ける。
static TEMP_SEED: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
});
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// `挿絵/` に置く一時ファイル名。
///
/// 拡張子を `.tmp` にするので、拡張子で挿絵を判定するスキャンには載らない。
fn temp_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("illustration");
    let seed = *TEMP_SEED;
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!("{name}.{seed:016x}{seq:016x}.tmp"))
}

/// まだ公開していない一時ファイル。Drop は自分が作ったこのパスだけを消す。
struct StagedFile {
    path: PathBuf,
    published: bool,
}

impl StagedFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            published: false,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// rename で `destination` へ公開する。
    ///
    /// 既に競合した書き手の完成品があれば `Ok(false)` を返し、自分の一時
    /// ファイルは Drop が片付ける (相手の完成品は一切触らない)。公開した
    /// 場合は `Ok(true)` で、以後 Drop は何もしない。
    fn publish(&mut self, destination: &Path) -> Result<bool> {
        match std::fs::rename(&self.path, destination) {
            Ok(()) => {
                self.published = true;
                Ok(true)
            }
            Err(error) => {
                if destination.is_file() {
                    Ok(false)
                } else {
                    Err(error.into())
                }
            }
        }
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// 同期文脈 (converter) から [`materialize`] を呼ぶ。
pub fn materialize_blocking(novel_dir: &Path) -> Result<Materialized> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::scope(|scope| {
            let worker = std::thread::Builder::new()
                .spawn_scoped(scope, || materialize_blocking(novel_dir))
                .map_err(|error| crate::error::NarouError::Platform(error.to_string()))?;
            worker.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| crate::error::NarouError::Platform(error.to_string()))?;
    runtime.block_on(materialize(novel_dir))
}

/// ストア上だけにある挿絵の一覧 (孤児判定つき)。
pub struct StoredIllustrationScan {
    /// 保存済み挿絵オブジェクトの総数 (`挿絵/` 直下)。
    pub total: usize,
    /// 索引・`raw/*.html` から到達できない (表示ファイル名, オブジェクトキー)。
    pub orphans: Vec<(String, ObjectKey)>,
}

/// ローカル `挿絵/` に実体が無い構成向けの孤児判定。
///
/// 判定規則は [`crate::illustration_store::find_orphan_illustrations`] と同じ:
/// 索引 (`sources` / `mitemin_ids` / `hashes` の値)、`raw/*.html` の
/// `<img src>` の解決結果、内容ハッシュ名・既知 mitemin ID 名の自己参照の
/// いずれかから到達できるファイルは孤児にしない。ファイル一覧は
/// `挿絵/` の実体の代わりにオブジェクトキーを使う。
pub async fn scan_stored_illustrations(novel_dir: &Path) -> Result<StoredIllustrationScan> {
    let inventory = crate::db::inventory::Inventory::with_default_root()?;
    let archive_root = inventory
        .root_dir()
        .join(crate::downloader::types::ARCHIVE_ROOT_DIR);
    let Some(prefix) =
        crate::native::object_store::logical_key_for_native_path(&archive_root, novel_dir)
    else {
        return Ok(StoredIllustrationScan {
            total: 0,
            orphans: Vec::new(),
        });
    };
    let stores = crate::native::object_store::NativeStores::for_narou_root(inventory.root_dir())?;
    let objects = stores.objects.as_ref();

    let index = {
        let key = prefix.join(".illustration_cache.yaml")?;
        match objects.read_small(&key).await? {
            Some(bytes) => {
                serde_yaml::from_slice::<crate::illustration_store::IllustrationIndex>(&bytes)
                    .map_err(|error| NarouError::Database(format!("挿絵索引を読めません: {error}")))?
            }
            None => crate::illustration_store::IllustrationIndex::default(),
        }
    };

    let mut saved: BTreeMap<String, ObjectKey> = BTreeMap::new();
    for metadata in list_child_objects(objects, &prefix, ILLUSTRATION_SEGMENT).await? {
        if let Some(name) = metadata.key.as_ref().rsplit('/').next() {
            saved.insert(name.to_string(), metadata.key.clone());
        }
    }
    let total = saved.len();

    let mut reachable: HashSet<String> = index.filenames().into_iter().collect();
    for metadata in list_child_objects(objects, &prefix, "raw").await? {
        if !metadata.key.as_ref().ends_with(".html") {
            continue;
        }
        let Some(bytes) = objects.read_small(&metadata.key).await? else {
            continue;
        };
        let Ok(raw) = String::from_utf8(bytes) else {
            continue;
        };
        for source in extract_img_sources(&raw) {
            if let Some(filename) = index.filename_for_source(&source)
                && saved.contains_key(filename)
            {
                reachable.insert(filename.to_string());
                continue;
            }
            if let Some(basename) = crate::illustration_store::legacy_basename_from_source(&source)
                && let Some(filename) = saved_filename_with_prefix(&saved, &basename)
            {
                reachable.insert(filename);
            }
        }
    }

    let orphans = saved
        .into_iter()
        .filter(|(filename, _)| !is_self_referencing_stored_name(&index, filename))
        .filter(|(filename, _)| !reachable.contains(filename))
        .collect();
    Ok(StoredIllustrationScan { total, orphans })
}

/// 同期文脈 (CLI) から [`scan_stored_illustrations`] を呼ぶ。
pub fn scan_stored_illustrations_blocking(novel_dir: &Path) -> Result<StoredIllustrationScan> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::scope(|scope| {
            let worker = std::thread::Builder::new()
                .spawn_scoped(scope, || scan_stored_illustrations_blocking(novel_dir))
                .map_err(|error| crate::error::NarouError::Platform(error.to_string()))?;
            worker
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| crate::error::NarouError::Platform(error.to_string()))?;
    runtime.block_on(scan_stored_illustrations(novel_dir))
}

/// ストア上に保存済み挿絵があるか (件数だけ、内容は読まない)。
pub fn stored_illustration_count_blocking(novel_dir: &Path) -> Result<usize> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::scope(|scope| {
            let worker = std::thread::Builder::new()
                .spawn_scoped(scope, || stored_illustration_count_blocking(novel_dir))
                .map_err(|error| crate::error::NarouError::Platform(error.to_string()))?;
            worker
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| crate::error::NarouError::Platform(error.to_string()))?;
    runtime.block_on(count_stored_illustrations(novel_dir))
}

async fn count_stored_illustrations(novel_dir: &Path) -> Result<usize> {
    let inventory = crate::db::inventory::Inventory::with_default_root()?;
    let archive_root = inventory
        .root_dir()
        .join(crate::downloader::types::ARCHIVE_ROOT_DIR);
    let Some(prefix) =
        crate::native::object_store::logical_key_for_native_path(&archive_root, novel_dir)
    else {
        return Ok(0);
    };
    let stores = crate::native::object_store::NativeStores::for_narou_root(inventory.root_dir())?;
    Ok(
        list_child_objects(stores.objects.as_ref(), &prefix, ILLUSTRATION_SEGMENT)
            .await?
            .len(),
    )
}

/// `挿絵/` などの直下オブジェクトだけを列挙する (ページング付き)。
async fn list_child_objects(
    objects: &dyn ObjectStore,
    prefix: &ObjectKey,
    segment: &str,
) -> Result<Vec<ObjectMetadata>> {
    let list_prefix = ObjectPrefix::new(format!("{}/{}", prefix.as_ref(), segment))?;
    let expected_prefix = format!("{}/", list_prefix.as_ref());
    let mut cursor: Option<String> = None;
    let mut children = Vec::new();
    loop {
        let request = ObjectListRequest::new(
            list_prefix.clone(),
            NonZeroUsize::new(512).expect("page size is non-zero"),
        );
        let request = match &cursor {
            Some(cursor) => request.after(cursor.clone()),
            None => request,
        };
        let page = objects.list_page(&request).await?;
        for metadata in page.objects {
            if let Some(rest) = metadata.key.as_ref().strip_prefix(&expected_prefix)
                && !rest.contains('/')
            {
                children.push(metadata);
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(children),
        }
    }
}

/// `find_saved_illustration_filename` のストア版 (ソート済みキーから最初の
/// `<basename>.<ext>` を返す)。
fn saved_filename_with_prefix(
    saved: &BTreeMap<String, ObjectKey>,
    basename: &str,
) -> Option<String> {
    let prefix = format!("{basename}.");
    saved.keys().find(|name| name.starts_with(&prefix)).cloned()
}

fn is_self_referencing_stored_name(
    index: &crate::illustration_store::IllustrationIndex,
    filename: &str,
) -> bool {
    let stem = filename.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(filename);
    if stem.len() == 64 && stem.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    stem.strip_prefix('i').is_some_and(|digits| {
        !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
    }) && index.filename_for_mitemin_id(stem).is_some()
}

/// `raw/*.html` の `<img src>` を正規化して取り出す
/// ([`crate::illustration_store`] のファイル走査と同じ規則)。
fn extract_img_sources(raw: &str) -> Vec<String> {
    static IMG_SRC_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?is)<img\b[^>]*\bsrc=["']([^"']+)["']"#)
            .expect("valid img src regex")
    });
    IMG_SRC_RE
        .captures_iter(raw)
        .filter_map(|captures| {
            captures
                .get(1)
                .map(|matched| crate::illustration_store::normalize_illustration_url(matched.as_str()))
        })
        .collect()
}

/// Keep restored files alive until every reader of this novel finishes.
#[derive(Default)]
pub struct Materialized {
    paths: Vec<PathBuf>,
    _scope: Option<Arc<crate::native::materialization::MaterializationScope>>,
}

impl Materialized {
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::NarouError;
    use crate::platform::mocks::MemoryObjectStore;
    use crate::platform::{AssetStream, ObjectKey, ObjectMetadata, PlatformFuture};

    struct FailingAssets<'a> {
        store: &'a MemoryObjectStore,
        fail_key: ObjectKey,
    }

    impl AssetStore for FailingAssets<'_> {
        fn stat<'a>(
            &'a self,
            key: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
            AssetStore::stat(self.store, key)
        }

        fn read_stream<'a>(
            &'a self,
            key: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
            if key == &self.fail_key {
                Box::pin(async {
                    let stream: AssetStream = Box::pin(futures::stream::iter(vec![
                        Ok(b"partial".to_vec()),
                        Err(NarouError::Platform("injected read failure".into())),
                    ]));
                    Ok(Some(stream))
                })
            } else {
                self.store.read_stream(key)
            }
        }

        fn write_stream<'a>(
            &'a self,
            key: &'a ObjectKey,
            stream: AssetStream,
        ) -> PlatformFuture<'a, Result<()>> {
            self.store.write_stream(key, stream)
        }

        fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
            AssetStore::delete(self.store, key)
        }

        fn copy<'a>(
            &'a self,
            source: &'a ObjectKey,
            destination: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<()>> {
            self.store.copy(source, destination)
        }

        fn move_or_copy<'a>(
            &'a self,
            source: &'a ObjectKey,
            destination: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<()>> {
            self.store.move_or_copy(source, destination)
        }
    }

    #[test]
    fn failed_materialization_cleans_new_files_and_can_retry() {
        futures::executor::block_on(async {
            for fail_name in ["1.png", "2.png"] {
                let dir = tempfile::tempdir().unwrap();
                let local_dir = dir.path().join("挿絵");
                std::fs::create_dir_all(&local_dir).unwrap();
                std::fs::write(local_dir.join("0.png"), b"keep local").unwrap();
                let store = MemoryObjectStore::new();
                let prefix = ObjectPrefix::new("novels/site/title/挿絵").unwrap();
                for name in ["0.png", "1.png", "2.png"] {
                    let key = ObjectKey::try_new(format!("novels/site/title/挿絵/{name}")).unwrap();
                    store
                        .write_small(&key, b"complete image".to_vec())
                        .await
                        .unwrap();
                }
                let assets = FailingAssets {
                    store: &store,
                    fail_key: ObjectKey::try_new(format!("novels/site/title/挿絵/{fail_name}"))
                        .unwrap(),
                };
                let result =
                    materialize_from_stores(&store, &assets, prefix.clone(), &local_dir,
                        crate::native::materialization::MaterializationScope::acquire(dir.path(), dir.path()).unwrap(),
                    ).await;
                assert!(result.is_err());
                assert!(!local_dir.join("1.png").exists());
                assert!(!local_dir.join("2.png").exists());
                assert_eq!(
                    std::fs::read(local_dir.join("0.png")).unwrap(),
                    b"keep local"
                );

                let guard = materialize_from_stores(&store, &store, prefix, &local_dir,
                    crate::native::materialization::MaterializationScope::acquire(dir.path(), dir.path()).unwrap(),
                ).await.unwrap();
                assert_eq!(guard.paths().len(), 2);
                for name in ["1.png", "2.png"] {
                    assert_eq!(
                        std::fs::read(local_dir.join(name)).unwrap(),
                        b"complete image"
                    );
                }
                drop(guard);
                assert!(!local_dir.join("1.png").exists());
                assert!(!local_dir.join("2.png").exists());
                assert_eq!(
                    std::fs::read(local_dir.join("0.png")).unwrap(),
                    b"keep local"
                );
            }
        });
    }

    /// 公開 (rename) したファイルは Drop が消さず、一時ファイルも残らないこと。
    #[test]
    fn staged_publish_disarms_cleanup_and_removes_the_temp() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("0001.jpg");
        let temp = dir.path().join("0001.jpg.0123456789abcdef.tmp");
        std::fs::write(&temp, b"complete image").unwrap();
        let mut staged = StagedFile::new(temp.clone());
        assert!(staged.publish(&destination).unwrap(), "rename must publish");
        drop(staged);
        assert_eq!(std::fs::read(&destination).unwrap(), b"complete image");
        assert!(!temp.exists(), "the temp name must not survive publish");
    }

    /// 未公開の一時ファイルだけを Drop が消し、他のファイルに触らないこと。
    #[test]
    fn staged_drop_removes_only_its_own_temp() {
        let dir = tempfile::tempdir().unwrap();
        let sibling = dir.path().join("0002.png");
        std::fs::write(&sibling, b"other image").unwrap();
        let temp = dir.path().join("0001.jpg.fedcba9876543210.tmp");
        std::fs::write(&temp, b"partial image").unwrap();
        drop(StagedFile::new(temp.clone()));
        assert!(!temp.exists(), "the unpublished temp must be removed");
        assert_eq!(std::fs::read(&sibling).unwrap(), b"other image");
    }
}
