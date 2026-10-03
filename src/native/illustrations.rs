//! S3 に置いた挿絵をローカルの `挿絵/` へ取り出す。
//!
//! EPUB 生成はファイルパスを要求する (組み込み Lite も外部 AozoraEpub3 も
//! 小説ディレクトリを読む)。挿絵を S3 に置く構成 (`s3.asset-backend=s3`) では
//! ローカルに実体が無いので、EPUB を作る直前にだけ取り出す。
//!
//! 取り出したファイルは呼び出し側が使い終わったら消す。ローカルに残すと
//! S3 へ逃がした意味が無くなるため、[`materialize`] は自分が取り出したパス
//! だけを返し、[`Materialized`] がスコープを抜けた時点で片付ける。

use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use futures::StreamExt;

use crate::error::Result;
use crate::platform::split_store::ILLUSTRATION_SEGMENT;
use crate::platform::{AssetStore, ObjectListRequest, ObjectPrefix, ObjectStore};

/// 小説ディレクトリ 1 件分の挿絵を取り出す。
///
/// 挿絵がローカルにある構成では、対象が既に存在するので何も取り出さない
/// (戻り値は空)。
pub async fn materialize(novel_dir: &Path) -> Result<Vec<PathBuf>> {
    let inventory = crate::db::inventory::Inventory::with_default_root()?;
    let archive_root = inventory
        .root_dir()
        .join(crate::downloader::types::ARCHIVE_ROOT_DIR);
    let Some(novel_key) =
        crate::native::object_store::logical_key_for_native_path(&archive_root, novel_dir)
    else {
        return Ok(Vec::new());
    };
    let stores = crate::native::object_store::NativeStores::for_narou_root(inventory.root_dir())?;
    let prefix = ObjectPrefix::new(novel_key.join(ILLUSTRATION_SEGMENT)?.as_ref())?;
    let local_dir = novel_dir.join(ILLUSTRATION_SEGMENT);

    materialize_from_stores(
        stores.objects.as_ref(),
        stores.assets.as_ref(),
        prefix,
        &local_dir,
    )
    .await
}

async fn materialize_from_stores(
    objects: &dyn ObjectStore,
    assets: &dyn AssetStore,
    prefix: ObjectPrefix,
    local_dir: &Path,
) -> Result<Vec<PathBuf>> {
    // Own every created file before another fallible operation. Failed downloads
    // must not leave a partial file that a later conversion mistakes for a mirror.
    let mut materialized = Materialized::new(Vec::new());
    let mut cursor: Option<String> = None;
    loop {
        let mut request = ObjectListRequest::new(prefix.clone(), NonZeroUsize::new(512).unwrap());
        if let Some(cursor) = cursor {
            request = request.after(cursor);
        }
        let page = objects.list_page(&request).await?;
        for metadata in &page.objects {
            let Some(filename) = metadata.key.as_ref().rsplit('/').next() else {
                continue;
            };
            if filename.is_empty() {
                continue;
            }
            let path = local_dir.join(filename);
            if path.is_file() {
                continue;
            }
            let Some(mut stream) = assets.read_stream(&metadata.key).await? else {
                continue;
            };
            std::fs::create_dir_all(local_dir)?;
            let mut file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_file() =>
                {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            materialized.0.push(path);
            while let Some(chunk) = stream.next().await {
                file.write_all(&chunk?)?;
            }
            file.flush()?;
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(std::mem::take(&mut materialized.0))
}

/// 同期文脈 (converter) から [`materialize`] を呼ぶ。
pub fn materialize_blocking(novel_dir: &Path) -> Result<Vec<PathBuf>> {
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

/// [`materialize`] が取り出したファイルを片付ける。
pub fn remove(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

/// 取り出したファイルを、スコープを抜けた時点で必ず片付けるガード。
///
/// EPUB 生成は途中で失敗しうる (`?` で抜ける) ので、呼び出し側で明示的に
/// 消す代わりにこれを持たせる。
pub struct Materialized(Vec<PathBuf>);

impl Materialized {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self(paths)
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.0
    }
}

impl Drop for Materialized {
    fn drop(&mut self) {
        remove(&self.0);
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
                    materialize_from_stores(&store, &assets, prefix.clone(), &local_dir).await;
                assert!(result.is_err());
                assert!(!local_dir.join("1.png").exists());
                assert!(!local_dir.join("2.png").exists());
                assert_eq!(
                    std::fs::read(local_dir.join("0.png")).unwrap(),
                    b"keep local"
                );

                let paths = materialize_from_stores(&store, &store, prefix, &local_dir)
                    .await
                    .unwrap();
                let guard = Materialized::new(paths);
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
}
