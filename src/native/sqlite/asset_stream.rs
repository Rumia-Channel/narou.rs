//! Bounded SQLite asset reads and atomic chunked writes.

use super::*;
use std::io::{Read, Seek, SeekFrom, Write};
use rusqlite::OptionalExtension;
use crate::platform::{asset_codec::{decode_stream, range_from_stream}, staging::{ASSET_CHUNK_BYTES, new_staging_key, validate_range}};
#[cfg(test)]
use crate::platform::staging::STAGE_TTL_SECONDS;

#[derive(Clone)]
struct Header { size: u64, encoding: String, crc32: u32, generation: String, physical:ObjectKey, inline: bool, stored_size: u64 }

fn db_error(error: rusqlite::Error) -> NarouError { super::super::sqlite_error(error) }

fn nonnegative_column(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(column)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(column, value))
}

impl SqliteObjectStore {
    pub(super) fn renew_stage_progress(&self, key: &ObjectKey) -> Result<()> {
        // Stage payload keys only: `.narou-staging/` uploads and transfer
        // `.narou-transfer/data/` payloads alike — a deleted stage must fail
        // renewal even when the throttle cache says it was just renewed,
        // and a throttle-hit still needs the existence probe because the
        // UPDATE below only runs on a real renewal. Other reserved keys
        // (proofs, checkpoints, journals) are filesystem-owned: a missing
        // row there is a legitimate `None`, never a renewal error.
        let stage_payload=crate::platform::staging::is_staging_key(key)||key.as_ref().starts_with(ts::DATA_PREFIX);
        if !stage_payload { return Ok(()); }
        let now = chrono::Utc::now();
        let conn = self.conn.lock();
        let physical = resolve_payload_key(&conn, key)?;
        if physical == *key {
            let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM objects WHERE object_key=?)",
                [key.as_ref()], |row| row.get(0)).map_err(db_error)?;
            if !exists { return Err(NarouError::Platform("stage expired or was deleted".into())); }
        }
        if !self.stage_leases.needs_renewal(key, now.timestamp()) { return Ok(()); }
        let changed = conn.execute("UPDATE objects SET updated_at=? WHERE object_key=?",
            rusqlite::params![now.to_rfc3339(), physical.as_ref()]).map_err(db_error)?;
        if changed == 0 { return Err(NarouError::Platform("stage expired or was deleted".into())); }
        self.stage_leases.renewed(key, now.timestamp());
        Ok(())
    }

    fn header_in_connection(conn:&Connection,key:&ObjectKey)->Result<Option<Header>>{
        let physical=resolve_payload_key(conn,key)?;
        conn.query_row("SELECT size,encoding,crc32,generation,data IS NOT NULL,CASE WHEN data IS NOT NULL THEN length(data) WHEN encoding='none' THEN size ELSE COALESCE((SELECT SUM(length(data)) FROM object_chunks WHERE object_key=objects.object_key),0) END FROM objects WHERE object_key=?",[physical.as_ref()],|row|Ok(Header{size:nonnegative_column(row,0)?,encoding:row.get(1)?,crc32:row.get(2)?,generation:row.get(3)?,physical:physical.clone(),inline:row.get(4)?,stored_size:nonnegative_column(row,5)?})).optional().map_err(db_error)
    }
    pub(super) fn open_asset_in_connection(&self,conn:&Connection,key:&ObjectKey,range:std::ops::Range<u64>)->Result<Option<(ObjectMetadata,AssetStream)>>{
        let referenced=Self::has_promoted_reference(conn,key)?;let private=crate::platform::is_reserved_key(key);
        let Some(header)=Self::header_in_connection(conn,key)?else{
            if referenced{return Err(NarouError::Platform("canonical reference payload missing".into()));}
            if private{return Ok(None);}
            return self.mirror.open_range(key,range)?.map(|(file,length)|Ok((crate::native::object_store::NativeObjectStore::metadata_for_open_file(key,&file)?,crate::native::object_store::native_range_stream(file,length)))).transpose();
        };
        let metadata=ObjectMetadata{key:key.clone(),size:header.size,etag:Some(header.generation.clone()),content_type:crate::platform::content_type_for_key(key).map(str::to_owned),last_modified:None};
        Ok(Some((metadata,self.stream_from_pinned_header(key.clone(),header,range)?)))
    }

    /// Local-only variant of `open_asset_in_connection`: a key backed by any
    /// canonical reference is skipped instead of resolving remote/stale bytes.
    /// Callers hold the same connection/lock boundary as the ref check.
    pub(super) fn open_local_asset_in_connection(&self,conn:&Connection,key:&ObjectKey,range:std::ops::Range<u64>)->Result<Option<(ObjectMetadata,AssetStream)>>{
        let referenced=Self::has_promoted_reference(conn,key)?;let private=crate::platform::is_reserved_key(key);
        if referenced{return Ok(None);}
        let Some(header)=Self::header_in_connection(conn,key)?else{
            if private{return Ok(None);}
            return self.mirror.open_range(key,range)?.map(|(file,length)|Ok((crate::native::object_store::NativeObjectStore::metadata_for_open_file(key,&file)?,crate::native::object_store::native_range_stream(file,length)))).transpose();
        };
        let metadata=ObjectMetadata{key:key.clone(),size:header.size,etag:Some(header.generation.clone()),content_type:crate::platform::content_type_for_key(key).map(str::to_owned),last_modified:None};
        Ok(Some((metadata,self.stream_from_pinned_header(key.clone(),header,range)?)))
    }
    fn stream_from_pinned_header(&self,key:ObjectKey,header:Header,range:std::ops::Range<u64>)->Result<AssetStream>{
        let end=if range.end==u64::MAX{header.size}else{range.end};if range.start>end||end>header.size{return Err(NarouError::Platform("asset range exceeds object".into()));}
        if range.start==end&&header.size!=0{return Ok(Box::pin(futures::stream::empty()));}
        let size=header.size;let crc=header.crc32;let full=range.start==0&&end==size;
        if header.encoding!="none"{
            let encoding=ObjectEncoding::parse(&header.encoding)?;
            let raw:AssetStream=Box::pin(futures::stream::try_unfold((self.clone(),key,header,0u64,0u64,0u64),|(store,key,header,seq,offset,total)|async move{
                if total==header.stored_size{return Ok(None);}let(bytes,length)=run_blocking({let store=store.clone();let key=key.clone();let header=header.clone();move||store.raw_slice(&key,&header,seq,offset,ASSET_CHUNK_BYTES)}).await?;
                if bytes.is_empty()||bytes.len() as u64>header.stored_size-total{return Err(NarouError::Platform("asset stored length mismatch".into()));}
                let next=total+bytes.len() as u64;let offset=offset+bytes.len() as u64;let(seq,offset)=if offset==length{(seq+1,0)}else{(seq,offset)};Ok(Some((bytes,(store,key,header,seq,offset,next))))
            }));let decoded=decode_stream(raw,encoding,size,crc);return Ok(if full{decoded}else{crate::platform::asset_codec::slice_stream(decoded,range.start..end)});
        }
        let raw:AssetStream=Box::pin(futures::stream::try_unfold((self.clone(),key,header,range.start),move|(store,key,header,position)|async move{
            if position==end{return Ok(None);}let(seq,offset)=if header.inline{(0,position)}else{(position/OBJECT_CHUNK_BYTES as u64,position%OBJECT_CHUNK_BYTES as u64)};
            let wanted=(end-position).min(ASSET_CHUNK_BYTES as u64).min(OBJECT_CHUNK_BYTES as u64-offset) as usize;let(bytes,_)=run_blocking({let store=store.clone();let key=key.clone();let header=header.clone();move||store.raw_slice(&key,&header,seq,offset,wanted)}).await?;
            if bytes.len()!=wanted{return Err(NarouError::Platform("asset range truncated".into()));}Ok(Some((bytes,(store,key,header,position+wanted as u64))))
        }));Ok(if full{decode_stream(raw,ObjectEncoding::None,size,crc)}else{raw})
    }

    fn raw_slice(&self, key: &ObjectKey, header: &Header, seq: u64, offset: u64, length: usize) -> Result<(Vec<u8>, u64)> {
        self.renew_stage_progress(key)?;
        let conn = self.conn.lock();
        if resolve_payload_key(&conn,key)?!=header.physical{return Err(NarouError::Platform("canonical reference changed during read".into()));}
        let result = if header.inline {
            conn.query_row("SELECT substr(data,?,?),length(data) FROM objects WHERE object_key=? AND generation=?",
                rusqlite::params![(offset + 1) as i64, length as i64, header.physical.as_ref(), header.generation], |row| Ok((row.get(0)?, nonnegative_column(row, 1)?))).optional()
        } else {
            conn.query_row("SELECT substr(c.data,?,?),length(c.data) FROM object_chunks c JOIN objects o ON o.object_key=c.object_key WHERE c.object_key=? AND o.generation=? AND c.seq=?",
                rusqlite::params![(offset + 1) as i64, length as i64, header.physical.as_ref(), header.generation, seq as i64], |row| Ok((row.get(0)?, nonnegative_column(row, 1)?))).optional()
        }.map_err(db_error)?;
        result.ok_or_else(|| NarouError::Platform("asset changed or stored chunk missing".into()))
    }

    pub(super) async fn read_asset(&self,key:&ObjectKey)->Result<Option<AssetStream>>{self.read_asset_stream_range(key,0..u64::MAX).await}
    pub(super) async fn read_asset_range(&self,key:&ObjectKey,range:std::ops::Range<u64>)->Result<Option<Vec<u8>>>{
        let length=validate_range(&range)?;let Some(stream)=self.read_asset_stream_range(key,range.clone()).await?else{return Ok(None);};range_from_stream(stream,0..length as u64).await.map(Some)
    }
    pub(super) async fn read_asset_stream_range(&self,key:&ObjectKey,range:std::ops::Range<u64>)->Result<Option<AssetStream>>{
        let this=self.clone();let key=key.clone();run_blocking(move||{this.renew_stage_progress(&key)?;let conn=this.conn.lock();Ok(this.open_asset_in_connection(&conn,&key,range)?.map(|(_,stream)|stream))}).await
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

    pub(super) async fn stage_local_stream(
        &self,
        mut stream: AssetStream,
    ) -> Result<crate::platform::StagedAsset> {
        let id = format!("ordinary-{}", uuid::Uuid::new_v4().simple());
        let key = ts::stage_data_key(&id)?;
        let mut temporary = run_blocking({
            let this = self.clone();
            move || {
                let path = this.mirror.resolve_path(&new_staging_key()?)?;
                this.mirror.ensure_parent_dir(&path)?;
                let temporary = tempfile::Builder::new()
                    .prefix(".narou-asset-")
                    .tempfile_in(path.parent().expect("stage parent"))?;
                fs2::FileExt::lock_exclusive(temporary.as_file())?;
                Ok(temporary)
            }
        })
        .await?;
        let mut size = 0u64;
        let mut crc = crc32fast::Hasher::new();
        let mut sha = sha2::Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            size = size
                .checked_add(chunk.len() as u64)
                .filter(|&size| size <= i64::MAX as u64)
                .ok_or_else(|| NarouError::Platform("asset size overflow".into()))?;
            crc.update(&chunk);
            sha.update(&chunk);
            temporary = run_blocking(move || {
                temporary.as_file_mut().write_all(&chunk)?;
                Ok(temporary)
            })
            .await?;
        }
        let crc = crc.finalize();
        let sha256 = hex::encode(sha.finalize());
        let this = self.clone();
        let staged_key = key.clone();
        run_blocking(move || {
            let mut input = temporary.as_file_mut();
            input.seek(SeekFrom::Start(0))?;
            let now = Self::now_rfc3339();
            let mut conn = this.conn.lock();
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(db_error)?;
            if size <= OBJECT_CHUNK_BYTES as u64 {
                let mut inline = vec![0; size as usize];
                input.read_exact(&mut inline)?;
                tx.execute(
                    "INSERT INTO objects(object_key,size,updated_at,content_type,encoding,crc32,data) VALUES (?,?,?,NULL,'none',?,?)",
                    rusqlite::params![staged_key.as_ref(), size as i64, now, crc, inline],
                )
                .map_err(db_error)?;
            } else {
                tx.execute(
                    "INSERT INTO objects(object_key,size,updated_at,content_type,encoding,crc32,data) VALUES (?,?,?,NULL,'none',?,NULL)",
                    rusqlite::params![staged_key.as_ref(), size as i64, now, crc],
                )
                .map_err(db_error)?;
                let mut buffer = vec![0; OBJECT_CHUNK_BYTES];
                let mut remaining = size;
                let mut seq = 0i64;
                while remaining > 0 {
                    let length = remaining.min(OBJECT_CHUNK_BYTES as u64) as usize;
                    input.read_exact(&mut buffer[..length])?;
                    tx.execute(
                        "INSERT INTO object_chunks(object_key,seq,data) VALUES (?,?,?)",
                        rusqlite::params![staged_key.as_ref(), seq, &buffer[..length]],
                    )
                    .map_err(db_error)?;
                    remaining -= length as u64;
                    seq += 1;
                }
            }
            Self::record_verified_object(&tx, &staged_key, size, &sha256)?;
            tx.commit().map_err(db_error)?;
            drop(conn);
            if this.mirror_files {
                let path = this.mirror.resolve_path(&staged_key)?;
                this.mirror.ensure_parent_dir(&path)?;
                let mut mirror = tempfile::NamedTempFile::new_in(path.parent().expect("asset parent"))?;
                input.seek(SeekFrom::Start(0))?;
                std::io::copy(&mut input, mirror.as_file_mut())?;
                mirror.as_file().sync_all()?;
                mirror.persist(path).map_err(|error| NarouError::Io(error.error))?;
                this.mirror.save_verified_file(&staged_key, size, &sha256)?;
            }
            Ok(crate::platform::StagedAsset {
                key: staged_key,
                size,
                sha256,
            })
        })
        .await
    }

    #[cfg(test)]
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
