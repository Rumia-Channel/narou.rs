//! Large generated text shares immutable content references, not control BLOBs.

use super::*;
use narou_rs::platform::{asset_codec::{PayloadCrc32, decode_stream, range_from_stream}, staging::{is_staging_key, validate_range}};

pub(super) fn is_private_or_generated(key: &ObjectKey) -> bool {
    is_staging_key(key) || key.as_ref().starts_with("generated/")
}

impl ContentStore {
    pub(super) async fn read_content_stream(&self, key: &ObjectKey) -> Result<Option<AssetStream>> {
        if is_private_or_generated(key) { return self.remote_assets.read_stream(key).await; }
        if !is_content_key(key) { return self.legacy.read_stream(key).await; }
        let Some(row) = self.reference(key).await? else {
            if key.as_ref().ends_with("/novel.txt") || backend(&self.db).await? != ContentBackend::S3 {
                return self.legacy.read_stream(key).await;
            }
            return Ok(None);
        };
        let raw = self.remote_assets.read_stream(&ObjectKey::try_new(row.blob_key)?).await?
            .ok_or_else(|| error("referenced asset is missing"))?;
        let decoded = decode_stream(raw, ObjectEncoding::parse(&row.encoding)?, row.size, row.crc32);
        Ok(Some(Box::pin(futures::stream::try_unfold(
            (decoded, Sha256::new(), row.sha256), |(mut stream, mut hash, expected)| async move {
                match stream.next().await {
                    Some(bytes) => { let bytes = bytes?; hash.update(&bytes); Ok(Some((bytes, (stream, hash, expected)))) }
                    None if hex::encode(hash.finalize()) != expected => Err(error("content asset SHA-256 mismatch")),
                    None => Ok(None),
                }
            },
        ))))
    }

    pub(super) async fn read_content_range(&self, key: &ObjectKey, range: std::ops::Range<u64>) -> Result<Option<Vec<u8>>> {
        validate_range(&range)?;
        if is_private_or_generated(key) { return self.remote_assets.read_range(key, range).await; }
        if !is_content_key(key) { return self.legacy.read_range(key, range).await; }
        let Some(row) = self.reference(key).await? else {
            if key.as_ref().ends_with("/novel.txt") || backend(&self.db).await? != ContentBackend::S3 {
                return self.legacy.read_range(key, range).await;
            }
            return Ok(None);
        };
        if range.end > row.size { return Err(error("content range exceeds object")); }
        if row.encoding == "none" {
            return self.remote_assets.read_range(&ObjectKey::try_new(row.blob_key)?, range).await;
        }
        range_from_stream(self.read_content_stream(key).await?.ok_or_else(|| error("content disappeared"))?, range).await.map(Some)
    }

    pub(super) async fn write_content_stream(&self, key: &ObjectKey, stream: AssetStream) -> Result<()> {
        if is_private_or_generated(key) { return self.remote_assets.write_stream(key, stream).await; }
        if !is_content_key(key) || backend(&self.db).await? == ContentBackend::D1 {
            return self.legacy.write_stream(key, stream).await;
        }
        let crc = Arc::new(std::sync::Mutex::new(PayloadCrc32::new()));
        let progress = crc.clone();
        let input: AssetStream = Box::pin(stream.map(move |chunk| {
            if let Ok(bytes) = &chunk { progress.lock().expect("CRC mutex").update(bytes); }
            chunk
        }));
        let stage = self.remote_assets.stage_stream(input).await?;
        let result = async {
            let (blob, now) = self.reserve_revision().await?;
            self.remote_assets.copy(&stage.key, &blob).await?;
            let crc = crc.lock().expect("CRC mutex").clone().finalize();
            let row = Reference { object_key: key.to_string(), blob_key: blob.to_string(), size: stage.size,
                stored_size: stage.size, encoding: "none".into(), crc32: crc, sha256: stage.sha256,
                updated_at: now, content_type: content_type_for_key(key).map(str::to_owned), verified: 1 };
            if !self.commit_reference(&row, "*", "").await? { return Err(error("asset publication conflict")); }
            self.collect_garbage(4).await?;
            Ok(())
        }.await;
        let cleanup = self.remote_assets.delete(&stage.key).await;
        result.and(cleanup)
    }

    pub(super) async fn copy_content_asset(&self, source: &ObjectKey, destination: &ObjectKey, remove: bool) -> Result<()> {
        if is_private_or_generated(source) && is_private_or_generated(destination) {
            return if remove { self.remote_assets.move_or_copy(source, destination).await }
                else { self.remote_assets.copy(source, destination).await };
        }
        if is_private_or_generated(source) || is_private_or_generated(destination)
            || is_content_key(source) != is_content_key(destination) {
            let stream = self.read_content_stream(source).await?.ok_or_else(|| error("asset copy source missing"))?;
            self.write_content_stream(destination, stream).await?;
            if remove { AssetStore::delete(self, source).await?; }
            return Ok(());
        }
        self.transfer(source, destination, remove).await
    }
}
