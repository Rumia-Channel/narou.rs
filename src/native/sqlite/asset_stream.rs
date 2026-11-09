//! Bounded SQLite asset reads and atomic chunked writes.

use super::*;
use std::io::{Read, Seek, SeekFrom, Write};
use rusqlite::OptionalExtension;
use crate::platform::{asset_codec::{decode_stream, range_from_stream}, staging::{ASSET_CHUNK_BYTES, STAGE_TTL_SECONDS, new_staging_key, validate_range}};

#[derive(Clone)]
struct Header { size: u64, encoding: String, crc32: u32, updated_at: String, inline: bool, stored_size: u64 }

fn db_error(error: rusqlite::Error) -> NarouError { super::super::sqlite_error(error) }

fn nonnegative_column(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(column)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(column, value))
}

impl SqliteObjectStore {
    pub(super) fn renew_stage_progress(&self, key: &ObjectKey) -> Result<()> {
        let now = chrono::Utc::now();
        if !crate::platform::staging::is_staging_key(key)
            || !self.stage_leases.needs_renewal(key, now.timestamp()) { return Ok(()); }
        let changed = self.conn.lock().execute("UPDATE objects SET updated_at=? WHERE object_key=?",
            rusqlite::params![now.to_rfc3339(), key.as_ref()]).map_err(db_error)?;
        if changed == 0 { return Err(NarouError::Platform("stage expired or was deleted".into())); }
        self.stage_leases.renewed(key, now.timestamp());
        Ok(())
    }

    fn asset_header(&self, key: &ObjectKey) -> Result<Option<Header>> {
        let header = self.conn.lock().query_row(
            "SELECT size,encoding,crc32,updated_at,data IS NOT NULL,COALESCE(length(data),(SELECT SUM(length(data)) FROM object_chunks WHERE object_key=objects.object_key),0) FROM objects WHERE object_key=?",
            [key.as_ref()], |row| Ok(Header { size: nonnegative_column(row, 0)?, encoding: row.get(1)?, crc32: row.get(2)?,
                updated_at: row.get(3)?, inline: row.get(4)?, stored_size: nonnegative_column(row, 5)? }))
            .optional().map_err(db_error)?;
        if header.is_some() { self.renew_stage_progress(key)?; }
        Ok(header)
    }

    fn raw_slice(&self, key: &ObjectKey, header: &Header, seq: u64, offset: u64, length: usize) -> Result<(Vec<u8>, u64)> {
        self.renew_stage_progress(key)?;
        let conn = self.conn.lock();
        let result = if header.inline {
            conn.query_row("SELECT substr(data,?,?),length(data) FROM objects WHERE object_key=? AND (updated_at=? OR object_key GLOB '.narou-staging/*')",
                rusqlite::params![(offset + 1) as i64, length as i64, key.as_ref(), header.updated_at], |row| Ok((row.get(0)?, nonnegative_column(row, 1)?))).optional()
        } else {
            conn.query_row("SELECT substr(c.data,?,?),length(c.data) FROM object_chunks c JOIN objects o ON o.object_key=c.object_key WHERE c.object_key=? AND (o.updated_at=? OR o.object_key GLOB '.narou-staging/*') AND c.seq=?",
                rusqlite::params![(offset + 1) as i64, length as i64, key.as_ref(), header.updated_at, seq as i64], |row| Ok((row.get(0)?, nonnegative_column(row, 1)?))).optional()
        }.map_err(db_error)?;
        result.ok_or_else(|| NarouError::Platform("asset changed or stored chunk missing".into()))
    }

    pub(super) async fn read_asset(&self, key: &ObjectKey) -> Result<Option<AssetStream>> {
        let staged = crate::platform::staging::is_staging_key(key);
        if self.prefer_existing_files && !staged
            && let Some(stream) = self.mirror.read_stream(key).await? { return Ok(Some(stream)); }
        let header = run_blocking({ let this = self.clone(); let key = key.clone(); move || this.asset_header(&key) }).await?;
        let Some(header) = header else { return if staged { Ok(None) } else { self.mirror.read_stream(key).await }; };
        let encoding = ObjectEncoding::parse(&header.encoding)?;
        let (size, crc32) = (header.size, header.crc32);
        let raw: AssetStream = Box::pin(futures::stream::try_unfold((self.clone(), key.clone(), header, 0u64, 0u64, 0u64),
            |(store, key, header, seq, offset, total)| async move {
                if total == header.stored_size { return Ok(None); }
                let (bytes, length) = run_blocking({
                    let store = store.clone(); let key = key.clone(); let header = header.clone();
                    move || store.raw_slice(&key, &header, seq, offset, ASSET_CHUNK_BYTES)
                }).await?;
                if bytes.is_empty() || bytes.len() as u64 > header.stored_size - total {
                    return Err(NarouError::Platform("asset stored length mismatch".into()));
                }
                let next = total + bytes.len() as u64;
                let offset = offset + bytes.len() as u64;
                let (seq, offset) = if offset == length { (seq + 1, 0) } else { (seq, offset) };
                Ok(Some((bytes, (store, key, header, seq, offset, next))))
            }));
        Ok(Some(decode_stream(raw, encoding, size, crc32)))
    }

    pub(super) async fn read_asset_range(&self, key: &ObjectKey, range: std::ops::Range<u64>) -> Result<Option<Vec<u8>>> {
        let length = validate_range(&range)?;
        let staged = crate::platform::staging::is_staging_key(key);
        if self.prefer_existing_files && !staged
            && let Some(bytes) = self.mirror.read_range(key, range.clone()).await? { return Ok(Some(bytes)); }
        let header = run_blocking({ let this = self.clone(); let key = key.clone(); move || this.asset_header(&key) }).await?;
        let Some(header) = header else { return if staged { Ok(None) } else { self.mirror.read_range(key, range).await }; };
        if range.end > header.size { return Err(NarouError::Platform("asset range exceeds object".into())); }
        if header.encoding != "none" {
            return range_from_stream(self.read_asset(key).await?.ok_or_else(|| NarouError::NotFound(key.to_string()))?, range).await.map(Some);
        }
        let this = self.clone(); let key = key.clone();
        run_blocking(move || {
            this.renew_stage_progress(&key)?;
            let mut bytes = Vec::with_capacity(length);
            let mut position = range.start;
            while position < range.end {
                let (seq, offset) = if header.inline { (0, position) }
                    else { (position / OBJECT_CHUNK_BYTES as u64, position % OBJECT_CHUNK_BYTES as u64) };
                let wanted = (range.end - position).min(OBJECT_CHUNK_BYTES as u64 - offset) as usize;
                let (chunk, _) = this.raw_slice(&key, &header, seq, offset, wanted)?;
                if chunk.len() != wanted { return Err(NarouError::Platform("asset range truncated".into())); }
                position += chunk.len() as u64;
                bytes.extend_from_slice(&chunk);
            }
            Ok(Some(bytes))
        }).await
    }

    pub(super) async fn write_asset(&self, key: &ObjectKey, mut stream: AssetStream) -> Result<()> {
        let mut temporary = run_blocking({ let this = self.clone(); move || {
            let path = this.mirror.resolve_path(&new_staging_key()?)?;
            this.mirror.ensure_parent_dir(&path)?;
            let temporary = tempfile::Builder::new().prefix(".narou-asset-").tempfile_in(path.parent().expect("stage parent"))?;
            fs2::FileExt::lock_exclusive(temporary.as_file())?;
            Ok(temporary)
        }}).await?;
        let mut size = 0u64;
        let mut crc = crc32fast::Hasher::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            size = size.checked_add(chunk.len() as u64).filter(|&size| size <= i64::MAX as u64)
                .ok_or_else(|| NarouError::Platform("asset size overflow".into()))?;
            crc.update(&chunk);
            temporary = run_blocking(move || {
                temporary.as_file_mut().write_all(&chunk)?;
                Ok(temporary)
            }).await?;
        }
        let crc = crc.finalize();
        let this = self.clone(); let key = key.clone();
        run_blocking(move || {
            temporary.as_file().sync_all()?;
            let mut input = temporary.as_file_mut();
            input.seek(SeekFrom::Start(0))?;
            let mut conn = this.conn.lock();
            let tx = conn.transaction().map_err(db_error)?;
            tx.execute("INSERT INTO objects(object_key,size,updated_at,content_type,encoding,crc32,data) VALUES (?,?,?,?,'none',?,NULL) ON CONFLICT(object_key) DO UPDATE SET size=excluded.size,updated_at=excluded.updated_at,content_type=excluded.content_type,encoding='none',crc32=excluded.crc32,data=NULL",
                rusqlite::params![key.as_ref(), size as i64, Self::now_rfc3339(), content_type_for_path(Path::new(key.as_ref())), crc]).map_err(db_error)?;
            tx.execute("DELETE FROM object_chunks WHERE object_key=?", [key.as_ref()]).map_err(db_error)?;
            let mut buffer = vec![0; OBJECT_CHUNK_BYTES];
            let mut remaining = size;
            let mut seq = 0;
            while remaining > 0 {
                let length = remaining.min(OBJECT_CHUNK_BYTES as u64) as usize;
                input.read_exact(&mut buffer[..length])?;
                tx.execute("INSERT INTO object_chunks(object_key,seq,data) VALUES (?,?,?)",
                    rusqlite::params![key.as_ref(), seq, &buffer[..length]]).map_err(db_error)?;
                remaining -= length as u64;
                seq += 1;
            }
            tx.commit().map_err(db_error)?;
            drop(conn);
            if this.mirror_files {
                let path = this.mirror.resolve_path(&key)?;
                this.mirror.ensure_parent_dir(&path)?;
                let mut mirror = tempfile::NamedTempFile::new_in(path.parent().expect("asset parent"))?;
                input.seek(SeekFrom::Start(0))?;
                std::io::copy(&mut input, mirror.as_file_mut())?;
                mirror.as_file().sync_all()?;
                mirror.persist(path).map_err(|error| NarouError::Io(error.error))?;
            }
            Ok(())
        }).await
    }

    pub(super) async fn cleanup_asset_stages(&self) -> Result<()> {
        let this = self.clone();
        run_blocking(move || {
            crate::native::object_store::collect_expired_stage_files(this.mirror.root())?;
            let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(STAGE_TTL_SECONDS)).to_rfc3339();
            let keys: Vec<String> = {
                let conn = this.conn.lock();
                let mut statement = conn.prepare("SELECT object_key FROM objects WHERE object_key>=? AND object_key<? AND updated_at<? ORDER BY updated_at LIMIT 100").map_err(db_error)?;
                statement.query_map(rusqlite::params![".narou-staging/", ".narou-staging0", cutoff], |row| row.get(0)).map_err(db_error)?
                    .collect::<std::result::Result<_, _>>().map_err(db_error)?
            };
            for key in keys {
                let key = ObjectKey::try_new(key)?;
                let mut conn = this.conn.lock();
                let tx = conn.transaction().map_err(db_error)?;
                tx.execute("DELETE FROM object_chunks WHERE object_key=? AND EXISTS(SELECT 1 FROM objects WHERE object_key=? AND updated_at<?)",
                    rusqlite::params![key.as_ref(), key.as_ref(), cutoff]).map_err(db_error)?;
                let removed = tx.execute("DELETE FROM objects WHERE object_key=? AND updated_at<?",
                    rusqlite::params![key.as_ref(), cutoff]).map_err(db_error)?;
                tx.commit().map_err(db_error)?;
                drop(conn);
                if removed != 0 {
                    this.stage_leases.forget(&key);
                    this.remove_file_copy(&key)?;
                }
            }
            Ok(())
        }).await
    }
}
