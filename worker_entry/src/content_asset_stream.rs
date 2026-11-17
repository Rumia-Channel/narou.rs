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
        self.verified_content_stream(row).await.map(Some)
    }

    async fn verified_content_stream(&self, row: Reference) -> Result<AssetStream> {
        let raw = self.remote_assets.read_stream(&ObjectKey::try_new(row.blob_key)?).await?
            .ok_or_else(|| error("referenced asset is missing"))?;
        let decoded = decode_stream(raw, ObjectEncoding::parse(&row.encoding)?, row.size, row.crc32);
        Ok(Box::pin(futures::stream::try_unfold(
            (decoded, Sha256::new(), row.sha256), |(mut stream, mut hash, expected)| async move {
                match stream.next().await {
                    Some(bytes) => { let bytes = bytes?; hash.update(&bytes); Ok(Some((bytes, (stream, hash, expected)))) }
                    None if hex::encode(hash.finalize()) != expected => Err(error("content asset SHA-256 mismatch")),
                    None => Ok(None),
                }
            },
        )))
    }

    pub(super) async fn read_content_range(&self, key: &ObjectKey, range: std::ops::Range<u64>) -> Result<Option<Vec<u8>>> {
        let length = validate_range(&range)?;
        if is_private_or_generated(key) { return self.remote_assets.read_range(key, range).await; }
        if !is_content_key(key) { return self.legacy.read_range(key, range).await; }
        let Some(row) = self.reference(key).await? else {
            if key.as_ref().ends_with("/novel.txt") || backend(&self.db).await? != ContentBackend::S3 {
                return self.legacy.read_range(key, range).await;
            }
            return Ok(None);
        };
        if range.end > row.size { return Err(error("content range exceeds object")); }
        let full = range.start == 0 && range.end == row.size;
        // Full-buffer reads of an unencoded immutable blob still verify the
        // published reference; only partial ranges (whose prefix was never
        // fetched) may use the raw fetch.
        if row.encoding == "none" && !full {
            return self.remote_assets.read_range(&ObjectKey::try_new(row.blob_key)?, range).await;
        }
        let mut stream = self.verified_content_stream(row).await?;
        if full {
            let mut bytes = Vec::with_capacity(length);
            // Consume EOF as well: range slicing stops at the last byte, but
            // the decoder's CRC/SHA verdict is emitted on its next poll.
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                if chunk.len() > length - bytes.len() { return Err(error("content range exceeds object")); }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(Some(bytes));
        }
        range_from_stream(stream, range).await.map(Some)
    }

    pub(super) async fn read_content_stream_range(&self,key:&ObjectKey,range:std::ops::Range<u64>)->Result<Option<AssetStream>>{
        if is_private_or_generated(key)||narou_rs::platform::is_transfer_key(key){return self.remote_assets.read_stream_range(key,range).await;}
        if !is_content_key(key){return self.legacy.read_stream_range(key,range).await;}
        let Some(row)=self.reference(key).await?else{if key.as_ref().ends_with("/novel.txt")||backend(&self.db).await?!=ContentBackend::S3{return self.legacy.read_stream_range(key,range).await;}return Ok(None);};
        let end=if range.end==u64::MAX{row.size}else{range.end};if range.start>end||end>row.size{return Err(error("content range exceeds object"));}
        // A full-object read can and must re-verify the immutable blob against
        // its published reference (decode CRC + decoded SHA-256 at EOF), so a
        // replaced/corrupt S3 body can never stream out as a successful read.
        // Only a true partial range keeps the raw fetch: a subrange cannot be
        // hash-verified without the prefix it skips.
        if range.start==0&&end==row.size{return self.verified_content_stream(row).await.map(Some);}
        if row.encoding=="none"{return self.remote_assets.read_stream_range(&ObjectKey::try_new(row.blob_key)?,range.start..end).await;}
        let stream=self.verified_content_stream(row).await?;Ok(Some(narou_rs::platform::asset_codec::slice_stream(stream,range.start..end)))
    }

    pub(super) async fn write_content_stream(&self, key: &ObjectKey, stream: AssetStream) -> Result<()> {
        self.write_content_stream_with_fence(key,stream,None).await
    }

    pub(super) async fn write_content_stream_with_fence(&self, key: &ObjectKey, stream: AssetStream, fence: Option<&narou_rs::platform::WriteFence>) -> Result<()> {
        if is_private_or_generated(key) { return self.remote_assets.write_stream(key, stream).await; }
        if !is_content_key(key) || backend(&self.db).await? == ContentBackend::D1 {
            return match fence { Some(fence)=>self.legacy.write_stream_guarded(fence,key,stream).await, None=>self.legacy.write_stream(key,stream).await };
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
            if !self.commit_reference_guarded(&row, "*", "", fence).await? { return Err(error("asset publication conflict")); }
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
