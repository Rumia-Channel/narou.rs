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
use crate::platform::{ObjectListRequest, ObjectPrefix};

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

    let mut materialized = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut request = ObjectListRequest::new(prefix.clone(), NonZeroUsize::new(512).unwrap());
        if let Some(cursor) = cursor {
            request = request.after(cursor);
        }
        let page = stores.objects.list_page(&request).await?;
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
            let Some(mut stream) = stores.assets.read_stream(&metadata.key).await? else {
                continue;
            };
            std::fs::create_dir_all(&local_dir)?;
            let mut file = std::fs::File::create(&path)?;
            while let Some(chunk) = stream.next().await {
                file.write_all(&chunk?)?;
            }
            file.flush()?;
            materialized.push(path);
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(materialized)
}

/// 同期文脈 (converter) から [`materialize`] を呼ぶ。
pub fn materialize_blocking(novel_dir: &Path) -> Result<Vec<PathBuf>> {
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
