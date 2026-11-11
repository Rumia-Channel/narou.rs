//! Chunked D1 assets and private atomic write staging.

use super::*;
use futures::StreamExt;
use narou_rs::platform::{ObjectEncoding, asset_codec::{decode_stream, range_from_stream, PayloadCrc32}, staging::{ASSET_CHUNK_BYTES, STAGE_TTL_SECONDS, is_staging_key, new_staging_key, validate_range}};

#[derive(Clone, Deserialize)]
struct Header {
    size: u64,
    encoding: String,
    crc32: u32,
    updated_at: String,
    inline: i64,
    stored_size: u64,
}

impl D1ObjectStore {
    async fn renew_stage_progress(&self, key: &ObjectKey) -> Result<()> {
        let now = chrono::Utc::now();
        if !is_staging_key(key) || !self.stage_leases.needs_renewal(key, now.timestamp()) { return Ok(()); }
        #[derive(Deserialize)] struct Renewed { object_key: String }
        let result = self.prepare("UPDATE objects SET updated_at=? WHERE object_key=? RETURNING object_key",
            vec![now.to_rfc3339().into(), key.as_ref().into()])?.run().await.map_err(worker_error)?;
        crate::d1_repository::ensure_batch_success(std::slice::from_ref(&result))?;
        if !result.results::<Renewed>().map_err(worker_error)?.iter().any(|row| row.object_key == key.as_ref()) {
            return Err(worker_error("stage expired or was deleted"));
        }
        self.stage_leases.renewed(key, now.timestamp());
        Ok(())
    }

    async fn asset_header(&self, key: &ObjectKey) -> Result<Option<Header>> {
        self.prepare("SELECT size,encoding,crc32,updated_at,data IS NOT NULL AS inline,COALESCE(length(data),(SELECT SUM(length(data)) FROM object_chunks WHERE object_key=objects.object_key),0) AS stored_size FROM objects WHERE object_key=?",
            vec![key.as_ref().into()])?.first(None).await.map_err(worker_error)
    }

    async fn raw_slice(&self, key: &ObjectKey, header: &Header, seq: u64, offset: u64, length: usize) -> Result<(Vec<u8>, u64)> {
        self.renew_stage_progress(key).await?;
        let (sql, values) = if header.inline != 0 {
            ("SELECT substr(data,?,?),length(data) FROM objects WHERE object_key=? AND (updated_at=? OR object_key GLOB '.narou-staging/*')",
                vec![JsValue::from_f64((offset + 1) as f64), JsValue::from_f64(length as f64), key.as_ref().into(), header.updated_at.as_str().into()])
        } else {
            ("SELECT substr(c.data,?,?),length(c.data) FROM object_chunks c JOIN objects o ON o.object_key=c.object_key WHERE c.object_key=? AND (o.updated_at=? OR o.object_key GLOB '.narou-staging/*') AND c.seq=?",
                vec![JsValue::from_f64((offset + 1) as f64), JsValue::from_f64(length as f64), key.as_ref().into(), header.updated_at.as_str().into(), JsValue::from_f64(seq as f64)])
        };
        let rows = self.prepare(sql, values)?.raw_js_value().await.map_err(worker_error)?;
        let row = rows.into_iter().next().ok_or_else(|| worker_error("asset changed or stored chunk missing"))?;
        let columns = worker::js_sys::Array::from(&row);
        let bytes = bounded_blob_bytes(&columns.get(0), length)?;
        let stored_length = js_value_to_i64(&columns.get(1));
        if stored_length < 0 { return Err(worker_error("negative stored length")); }
        Ok((bytes, stored_length as u64))
    }

    pub(super) async fn read_asset(&self, key: &ObjectKey) -> Result<Option<AssetStream>> {
        let Some(header) = self.asset_header(key).await? else { return Ok(None); };
        self.renew_stage_progress(key).await?;
        let encoding = ObjectEncoding::parse(&header.encoding)?;
        let (size, crc32) = (header.size, header.crc32);
        let raw: AssetStream = Box::pin(futures::stream::try_unfold(
            (self.clone(), key.clone(), header, 0u64, 0u64, 0u64),
            |(store, key, header, seq, offset, total)| async move {
                if total == header.stored_size { return Ok(None); }
                let (bytes, length) = store.raw_slice(&key, &header, seq, offset, ASSET_CHUNK_BYTES).await?;
                if bytes.is_empty() || bytes.len() as u64 > header.stored_size - total {
                    return Err(worker_error("asset stored length mismatch"));
                }
                let next = total + bytes.len() as u64;
                let offset = offset + bytes.len() as u64;
                let (seq, offset) = if offset == length { (seq + 1, 0) } else { (seq, offset) };
                Ok(Some((bytes, (store, key, header, seq, offset, next))))
            },
        ));
        Ok(Some(decode_stream(raw, encoding, size, crc32)))
    }

    pub(super) async fn read_asset_range(&self, key: &ObjectKey, range: std::ops::Range<u64>) -> Result<Option<Vec<u8>>> {
        let length = validate_range(&range)?;
        let Some(header) = self.asset_header(key).await? else { return Ok(None); };
        self.renew_stage_progress(key).await?;
        if range.end > header.size { return Err(worker_error("asset range exceeds object")); }
        if header.encoding != "none" {
            let stream = self.read_asset(key).await?.ok_or_else(|| worker_error("asset disappeared"))?;
            return range_from_stream(stream, range).await.map(Some);
        }
        let mut result = Vec::with_capacity(length);
        let mut position = range.start;
        while position < range.end {
            let (seq, offset) = if header.inline != 0 { (0, position) }
                else { (position / CHUNK_SIZE as u64, position % CHUNK_SIZE as u64) };
            let wanted = (range.end - position).min(CHUNK_SIZE as u64 - offset) as usize;
            let (bytes, _) = self.raw_slice(key, &header, seq, offset, wanted).await?;
            if bytes.len() != wanted { return Err(worker_error("asset range truncated")); }
            position += bytes.len() as u64;
            result.extend_from_slice(&bytes);
        }
        Ok(Some(result))
    }

    pub(super) async fn write_asset(&self, destination: &ObjectKey, mut stream: AssetStream) -> Result<()> {
        let temporary = if is_staging_key(destination) { destination.clone() } else { new_staging_key()? };
        let now = chrono::Utc::now().to_rfc3339();
        let reserved = self.prepare("INSERT INTO objects(object_key,size,updated_at,content_type,encoding,crc32,data) VALUES (?,0,?,NULL,'none',0,NULL)",
            vec![temporary.as_ref().into(), now.as_str().into()])?.run().await.map_err(worker_error)?;
        crate::d1_repository::ensure_batch_success(&[reserved])?;
        self.stage_leases.renewed(&temporary, chrono::Utc::now().timestamp());
        let result = async {
            let mut buffer = Vec::with_capacity(CHUNK_SIZE);
            let mut size = 0u64;
            let mut crc = PayloadCrc32::new();
            let mut seq = 0u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                if !chunk.is_empty() { self.renew_stage_progress(&temporary).await?; }
                crc.update(&chunk);
                size = size.checked_add(chunk.len() as u64).filter(|&size| size <= i64::MAX as u64)
                    .ok_or_else(|| worker_error("asset size overflow"))?;
                let mut offset = 0;
                while offset < chunk.len() {
                    let length = (CHUNK_SIZE - buffer.len()).min(chunk.len() - offset);
                    buffer.extend_from_slice(&chunk[offset..offset + length]);
                    offset += length;
                    if buffer.len() == CHUNK_SIZE {
                        self.write_asset_chunk(&temporary, seq, &buffer).await?;
                        seq += 1;
                        buffer.clear();
                    }
                }
            }
            self.renew_stage_progress(&temporary).await?;
            if !buffer.is_empty() { self.write_asset_chunk(&temporary, seq, &buffer).await?; }
            #[derive(Deserialize)] struct Finalized { object_key: String }
            let finalized = self.prepare("UPDATE objects SET size=?,crc32=?,content_type=?,updated_at=? WHERE object_key=? RETURNING object_key",
                vec![JsValue::from_f64(size as f64), JsValue::from_f64(crc.finalize() as f64),
                    content_type_for_key(destination).map(JsValue::from_str).unwrap_or(JsValue::NULL),
                    chrono::Utc::now().to_rfc3339().into(), temporary.as_ref().into()])?.run().await.map_err(worker_error)?;
            crate::d1_repository::ensure_batch_success(std::slice::from_ref(&finalized))?;
            if !finalized.results::<Finalized>().map_err(worker_error)?.iter().any(|row| row.object_key == temporary.as_ref()) {
                return Err(worker_error("stage expired before finalization"));
            }
            if temporary != *destination { self.copy_asset(&temporary, destination, true).await?; }
            Ok(())
        }.await;
        if result.is_err() { let _ = self.delete_all(&temporary).await; }
        result
    }

    async fn write_asset_chunk(&self, key: &ObjectKey, seq: u64, bytes: &[u8]) -> Result<()> {
        #[derive(Deserialize)] struct Written { object_key: String }
        let results = self.db.batch(vec![
            self.prepare("INSERT INTO object_chunks(object_key,seq,data) SELECT ?,?,? WHERE EXISTS(SELECT 1 FROM objects WHERE object_key=?) RETURNING object_key",
                vec![key.as_ref().into(), JsValue::from_f64(seq as f64), JsValue::from(worker::js_sys::Uint8Array::from(bytes)), key.as_ref().into()])?,
            self.prepare("UPDATE objects SET updated_at=? WHERE object_key=?", vec![chrono::Utc::now().to_rfc3339().into(), key.as_ref().into()])?,
        ]).await.map_err(worker_error)?;
        crate::d1_repository::ensure_batch_success(&results)?;
        if !results[0].results::<Written>().map_err(worker_error)?.iter().any(|row| row.object_key == key.as_ref()) {
            return Err(worker_error("stage expired before writing its next chunk"));
        }
        Ok(())
    }

    pub(super) async fn copy_asset(&self, source: &ObjectKey, destination: &ObjectKey, remove: bool) -> Result<()> {
        if source == destination { return Ok(()); }
        self.renew_stage_progress(source).await?;
        #[derive(Deserialize)] struct Copied { object_key: String }
        let mut statements = vec![
            self.prepare("DELETE FROM object_chunks WHERE object_key=? AND EXISTS(SELECT 1 FROM objects WHERE object_key=?)", vec![destination.as_ref().into(), source.as_ref().into()])?,
            self.prepare("INSERT INTO objects(object_key,size,updated_at,content_type,encoding,crc32,data) SELECT ?,size,?, ?,encoding,crc32,data FROM objects WHERE object_key=? ON CONFLICT(object_key) DO UPDATE SET size=excluded.size,updated_at=excluded.updated_at,content_type=excluded.content_type,encoding=excluded.encoding,crc32=excluded.crc32,data=excluded.data RETURNING object_key",
                vec![destination.as_ref().into(), chrono::Utc::now().to_rfc3339().into(), content_type_for_key(destination).map(JsValue::from_str).unwrap_or(JsValue::NULL), source.as_ref().into()])?,
            self.prepare("INSERT INTO object_chunks(object_key,seq,data) SELECT ?,seq,data FROM object_chunks WHERE object_key=?", vec![destination.as_ref().into(), source.as_ref().into()])?,
        ];
        if remove {
            statements.push(self.prepare("DELETE FROM object_chunks WHERE object_key=?", vec![source.as_ref().into()])?);
            statements.push(self.prepare("DELETE FROM objects WHERE object_key=?", vec![source.as_ref().into()])?);
        }
        let results = self.db.batch(statements).await.map_err(worker_error)?;
        crate::d1_repository::ensure_batch_success(&results)?;
        let copied = results[1].results::<Copied>().map_err(worker_error)?;
        if !copied.iter().any(|row| row.object_key == destination.as_ref()) { return Err(worker_error("copy source missing")); }
        Ok(())
    }

    pub async fn collect_expired_stages(&self, limit: usize) -> Result<usize> {
        #[derive(Deserialize)] struct Expired { object_key: String }
        let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(STAGE_TTL_SECONDS)).to_rfc3339();
        let rows = self.prepare("SELECT object_key FROM objects WHERE object_key>=? AND object_key<? AND updated_at<? ORDER BY updated_at LIMIT ?",
            vec![".narou-staging/".into(), ".narou-staging0".into(), cutoff.as_str().into(), JsValue::from_f64(limit as f64)])?
            .all().await.map_err(worker_error)?.results::<Expired>().map_err(worker_error)?;
        let mut count = 0;
        for row in rows {
            let key = ObjectKey::try_new(row.object_key)?;
            let results = self.db.batch(vec![
                self.prepare("DELETE FROM object_chunks WHERE object_key=? AND EXISTS(SELECT 1 FROM objects WHERE object_key=? AND updated_at<?)",
                    vec![key.as_ref().into(), key.as_ref().into(), cutoff.as_str().into()])?,
                self.prepare("DELETE FROM objects WHERE object_key=? AND updated_at<? RETURNING object_key",
                    vec![key.as_ref().into(), cutoff.as_str().into()])?,
            ]).await.map_err(worker_error)?;
            crate::d1_repository::ensure_batch_success(&results)?;
            let removed = results[1].results::<Expired>().map_err(worker_error)?.len();
            if removed != 0 { self.stage_leases.forget(&key); }
            count += removed;
        }
        Ok(count)
    }
}
