//! In-memory mock implementations for tests.
//!
//! These let download/update/convert logic run without network, filesystem,
//! or real sleeping. They are compiled into the crate unconditionally (cheap,
//! no extra deps) so both unit tests and future integration tests can use them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use futures::StreamExt;

use crate::db::NovelRecord;
use crate::error::{NarouError, Result};
use crate::platform::{
    AssetStore, AssetStream, CookieStore, HttpClient, HttpMethod, HttpRequest, HttpResponse, LoginGroup,
    NovelFilter, NovelId, NovelMutation, NovelQuery, NovelRepository, ObjectKey, ObjectListPage,
    ObjectListRequest, ObjectMetadata, ObjectStore, PlatformFuture, RateLimitScope, RateLimiter,
    paginate_object_listing,
    ObjectUpdateResult, SmallObjectMutator,
};

/// HTTP client backed by a caller-provided responder closure.
///
/// Register canned responses by URL; any request without a canned response
/// fails with `NarouError::Http`. All requests are recorded so tests can
/// assert on what was fetched.
#[derive(Debug, Default)]
pub struct MockHttpClient {
    responses: Mutex<BTreeMap<String, HttpResponse>>,
    method_responses: Mutex<BTreeMap<(String, String), HttpResponse>>,
    requests: Mutex<Vec<String>>,
    /// Full request heads (url + headers) so tests can assert what was sent.
    sent: Mutex<Vec<RequestHead>>,
    /// Bodies of the sent requests, aligned with `sent`.
    bodies: Mutex<Vec<Option<Vec<u8>>>>,
}

/// One sent request head: `(url, headers)` in send order.
type RequestHead = (String, Vec<(String, String)>);

impl MockHttpClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a canned response for an exact URL.
    pub fn add_response(&self, url: impl Into<String>, response: HttpResponse) {
        self.responses.lock().insert(url.into(), response);
    }

    /// Register a response for one method without replacing other URL fixtures.
    pub fn add_method_response(&self, method: HttpMethod, url: impl Into<String>, response: HttpResponse) {
        self.method_responses.lock().insert((method_name(method).into(), url.into()), response);
    }

    /// Register a text response for an exact URL.
    pub fn add_text(&self, url: impl Into<String>, status: u16, body: impl Into<String>) {
        self.add_response(
            url,
            HttpResponse {
                status,
                headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
                body: body.into().into_bytes(),
            },
        );
    }

    /// All URLs requested so far, in order.
    pub fn requested_urls(&self) -> Vec<String> {
        self.requests.lock().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().len()
    }

    /// All request heads sent so far: `(url, headers)` in send order.
    pub fn sent_requests(&self) -> Vec<RequestHead> {
        self.sent.lock().clone()
    }

    /// Bodies of the requests sent so far, aligned with [`Self::sent_requests`].
    pub fn sent_bodies(&self) -> Vec<Option<Vec<u8>>> {
        self.bodies.lock().clone()
    }
}

impl HttpClient for MockHttpClient {
    fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
        Box::pin(async move {
            self.sent
                .lock()
                .push((request.url.clone(), request.headers.clone()));
            self.bodies.lock().push(request.body.as_deref().cloned());
            self.requests
                .lock()
                .push(format!("{} {}", method_name(request.method), request.url));
            if let Some(response) = self.method_responses.lock()
                .get(&(method_name(request.method).to_string(), request.url.clone())).cloned() {
                return Ok(response);
            }
            self.responses
                .lock()
                .get(&request.url)
                .cloned()
                .ok_or_else(|| NarouError::Http(format!("no canned response for {}", request.url)))
        })
    }

    fn send_stream<'a>(
        &'a self, request: HttpRequest,
    ) -> PlatformFuture<'a, Result<crate::platform::http::HttpStreamResponse>> {
        Box::pin(async move {
            let url = request.url.clone();
            let response = self.send(request).await?;
            Ok(crate::platform::http::HttpStreamResponse {
                status: response.status, url, headers: response.headers,
                body: crate::platform::staging::owned_bytes_stream(response.body),
            })
        })
    }
}

fn method_name(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Delete => "DELETE",
    }
}

/// In-memory object store. Keys are plain strings.
#[derive(Debug, Default)]
pub struct MemoryObjectStore {
    objects: Mutex<BTreeMap<String, std::sync::Arc<Vec<u8>>>>,
    stage: MemoryStage,
    gates: std::sync::Arc<Mutex<BTreeMap<String, MemoryGate>>>,
    verified:Mutex<BTreeMap<String,MemoryFileProof>>,
}

impl MemoryObjectStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Separate payload/stage storage with the same admission, epoch and reader authority.
    pub fn new_with_shared_authority(authority: &Self) -> Self {
        Self {
            objects: Mutex::default(),
            stage: MemoryStage::default(),
            gates: authority.gates.clone(),
            verified: Mutex::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.objects.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
#[derive(Debug)]
struct MemoryFileProof{sha256:String,source:String,bytes:std::sync::Weak<Vec<u8>>}

impl MemoryObjectStore{
    fn memory_writer(&self,key:&ObjectKey)->Result<Option<super::WriteFence>>{
        let gates=self.gate_map();let row=gates.iter().filter(|(prefix,_)|super::ObjectPrefix::new(prefix.as_str()).is_ok_and(|prefix|prefix.matches(key))).max_by_key(|(prefix,_)|prefix.len());
        match row{Some((prefix,gate))=>{if gate.state!=super::GateState::Open{return Err(super::access_fence::gate_closed_error(&super::ObjectPrefix::new(prefix.as_str())?));}Ok(Some(super::WriteFence::with_proof(super::ObjectPrefix::new(prefix.as_str())?,gate.epoch,std::sync::Arc::new(MemoryGateProof(self.gates.clone())))))},None=>Ok(None)}
    }
    fn remember_verified(&self,key:&ObjectKey,sha:String)->Result<()>{let objects=self.objects.lock();let bytes=objects.get(key.as_ref()).ok_or_else(||NarouError::NotFound(key.to_string()))?;self.verified.lock().insert(key.to_string(),MemoryFileProof{sha256:sha,source:key.to_string(),bytes:std::sync::Arc::downgrade(bytes)});Ok(())}
}

impl ObjectStore for MemoryObjectStore {
    fn delete_closed<'a>(&'a self,prefix:&'a super::ObjectPrefix,owner:&'a str,epoch:u64,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{super::access_fence::parse_removal_owner(owner)?;let mut gates=self.gate_map();let gate=gates.get_mut(&gate_key_of(prefix)).ok_or_else(||super::access_fence::gate_closed_error(prefix))?;gate.prune(super::transfer_stage::now_secs());
        if !prefix.matches(key)||gate.state!=super::GateState::Closed||gate.owner.as_deref()!=Some(owner)||gate.epoch!=epoch{return Err(super::access_fence::gate_closed_error(prefix));}
        if !gate.readers.is_empty(){return Err(super::access_fence::novel_busy_error(prefix));}
        if self.objects.lock().remove(key.as_ref()).is_some(){gate.content_revision+=1;}Ok(())
    })}
    fn retire_legacy_guarded<'a>(&'a self,fence:&'a super::WriteFence,logical:&'a ObjectKey,pool:&'a ObjectKey,size:u64,sha:&'a str)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        use sha2::Digest;if super::transfer_stage::illustration_pool_key(logical)?!=*pool{return Err(NarouError::Platform("retirement pool identity mismatch".into()));}
        let mut gates=self.gate_map();let gate=gates.get_mut(&gate_key_of(&fence.prefix)).ok_or_else(||super::access_fence::gate_closed_error(&fence.prefix))?;
        if !fence.prefix.matches(logical)||gate.state!=super::GateState::Open||gate.epoch!=fence.epoch{return Err(super::access_fence::gate_closed_error(&fence.prefix));}
        let mut objects=self.objects.lock();let bytes=objects.get(pool.as_ref()).cloned().ok_or_else(||NarouError::NotFound(pool.to_string()))?;
        if bytes.len() as u64!=size||!hex::encode(sha2::Sha256::digest(bytes.as_slice())).eq_ignore_ascii_case(sha){return Err(NarouError::Platform("pool size/SHA mismatch".into()));}
        if let Some(current)=objects.get(logical.as_ref())&&(!hex::encode(sha2::Sha256::digest(current.as_slice())).eq_ignore_ascii_case(sha)||current.len() as u64!=size){return Err(NarouError::Platform("legacy source changed before retirement".into()));}
        let proof=MemoryFileProof{sha256:sha.to_ascii_lowercase(),source:pool.to_string(),bytes:std::sync::Arc::downgrade(&bytes)};objects.insert(logical.to_string(),bytes);self.verified.lock().insert(logical.to_string(),proof);gate.content_revision+=1;Ok(())
    })}
    fn update_small_guarded<'a,'data:'a>(&'a self,fence:&'a super::WriteFence,key:&'a ObjectKey,mutate:&'a mut SmallObjectMutator<'data>)->PlatformFuture<'a,Result<ObjectUpdateResult>>{Box::pin(async move{
        let mut gates=self.gate_map();let gate=gates.get_mut(&gate_key_of(&fence.prefix)).ok_or_else(||super::access_fence::gate_closed_error(&fence.prefix))?;
        if !fence.prefix.matches(key)||gate.state!=super::GateState::Open||gate.epoch!=fence.epoch{return Err(super::access_fence::gate_closed_error(&fence.prefix));}
        let mut objects=self.objects.lock();if let Some(next)=mutate(objects.get(key.as_ref()).map(|bytes|bytes.as_slice()))?{objects.insert(key.to_string(),std::sync::Arc::new(next.into_owned()));gate.content_revision+=1;}Ok(ObjectUpdateResult::Committed)
    })}
    fn access_fence(&self)->Option<&dyn super::NovelAccessFence>{Some(self)}
    fn read_stream_leased<'a>(&'a self,prefix:&'a super::ObjectPrefix,lease:&'a super::GateLease,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(async move{
        use super::NovelAccessFence;if !prefix.matches(key){return Err(NarouError::Platform("reader outside novel".into()));}self.renew_reader(prefix,lease,super::READER_LEASE_SECS).await?;self.read_stream_range(key,range).await
    })}
    fn write_small_guarded<'a>(&'a self,fence:&'a super::WriteFence,key:&'a ObjectKey,data:Vec<u8>)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        let mut gates=self.gate_map();let gate=gates.get_mut(&gate_key_of(&fence.prefix)).ok_or_else(||super::access_fence::gate_closed_error(&fence.prefix))?;
        if !fence.prefix.matches(key)||gate.state!=super::GateState::Open||gate.epoch!=fence.epoch{return Err(super::access_fence::gate_closed_error(&fence.prefix));}self.objects.lock().insert(key.to_string(),std::sync::Arc::new(data));gate.content_revision+=1;Ok(())
    })}
    fn delete_guarded<'a>(&'a self,fence:&'a super::WriteFence,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        let mut gates=self.gate_map();let gate=gates.get_mut(&gate_key_of(&fence.prefix)).ok_or_else(||super::access_fence::gate_closed_error(&fence.prefix))?;
        if !fence.prefix.matches(key)||gate.state!=super::GateState::Open||gate.epoch!=fence.epoch{return Err(super::access_fence::gate_closed_error(&fence.prefix));}
        if self.objects.lock().remove(key.as_ref()).is_some(){gate.content_revision+=1;}Ok(())
    })}

    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        Box::pin(async move {
            let objects = self.objects.lock();
            Ok(objects.get(key.as_ref()).map(|data| ObjectMetadata {
                key: key.clone(),
                size: data.len() as u64,
                etag: None,
                content_type: None,
                last_modified: None,
            }))
        })
    }

    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move { Ok(self.objects.lock().get(key.as_ref()).map(|data| data.as_ref().clone())) })
    }

    fn write_small<'a>(
        &'a self,
        key: &'a ObjectKey,
        data: Vec<u8>,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(fence)=self.memory_writer(key)?{return self.write_small_guarded(&fence,key,data).await;}
            self.objects.lock().insert(key.as_ref().to_string(), std::sync::Arc::new(data));
            Ok(())
        })
    }

    fn update_small<'a, 'data: 'a>(
        &'a self,
        key: &'a ObjectKey,
        mutate: &'a mut SmallObjectMutator<'data>,
    ) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        Box::pin(async move {
            if let Some(fence)=self.memory_writer(key)?{return self.update_small_guarded(&fence,key,mutate).await;}
            let mut objects = self.objects.lock();
            if let Some(data) = mutate(objects.get(key.as_ref()).map(|data| data.as_slice()))? {
                objects.insert(key.as_ref().to_string(), std::sync::Arc::new(data.into_owned()));
            }
            Ok(ObjectUpdateResult::Committed)
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(fence)=self.memory_writer(key)?{return self.delete_guarded(&fence,key).await;}
            self.objects.lock().remove(key.as_ref());
            Ok(())
        })
    }

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>> {
        Box::pin(async move {
            let objects = self.objects.lock();
            let mut items = Vec::new();
            for (key, data) in objects.iter() {
                let object_key = ObjectKey::try_new(key.clone())?;
                if request.prefix.matches(&object_key) {
                    items.push(ObjectMetadata {
                        key: object_key,
                        size: data.len() as u64,
                        etag: None,
                        content_type: None,
                        last_modified: None,
                    });
                }
            }
            let (page, next_cursor) = paginate_object_listing(
                items,
                request.cursor.as_deref(),
                request.limit.get(),
                |item| item.key.as_ref(),
            );
            Ok(ObjectListPage {
                objects: page,
                next_cursor,
            })
        })
    }
}

impl AssetStore for MemoryObjectStore {
    fn stage_stream<'a>(&'a self,stream:AssetStream)->PlatformFuture<'a,Result<super::StagedAsset>>{Box::pin(async move{let stage=super::staging::stage(self,stream).await?;self.remember_verified(&stage.key,stage.sha256.clone())?;Ok(stage)})}
    fn write_stream_guarded<'a>(&'a self,fence:&'a super::WriteFence,key:&'a ObjectKey,mut stream:AssetStream)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        let mut bytes=Vec::new();while let Some(chunk)=stream.next().await{bytes.extend_from_slice(&chunk?);}self.write_small_guarded(fence,key,bytes).await
    })}

    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        ObjectStore::stat(self, key)
    }

    fn read_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<crate::platform::AssetStream>>> {
        Box::pin(async move {
            let Some(bytes) = self.objects.lock().get(key.as_ref()).cloned() else {
                return Ok(None);
            };
            let stream: crate::platform::AssetStream = Box::pin(futures::stream::unfold(
                (bytes, 0usize), |(bytes, offset)| async move {
                    if offset == bytes.len() { return None; }
                    let end = (offset + crate::platform::staging::ASSET_CHUNK_BYTES).min(bytes.len());
                    Some((Ok(bytes[offset..end].to_vec()), (bytes, end)))
                },
            ));
            Ok(Some(stream))
        })
    }

    fn read_range<'a>(
        &'a self, key: &'a ObjectKey, range: std::ops::Range<u64>,
    ) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            crate::platform::staging::validate_range(&range)?;
            let objects = self.objects.lock();
            let Some(bytes) = objects.get(key.as_ref()) else { return Ok(None); };
            let start = usize::try_from(range.start).map_err(|_| NarouError::Platform("range overflow".into()))?;
            let end = usize::try_from(range.end).map_err(|_| NarouError::Platform("range overflow".into()))?;
            Ok(Some(bytes.get(start..end).ok_or_else(|| NarouError::Platform("asset range exceeds object".into()))?.to_vec()))
        })
    }

    fn read_stream_range<'a>(
        &'a self,
        key: &'a ObjectKey,
        range: std::ops::Range<u64>,
    ) -> PlatformFuture<'a, Result<Option<crate::platform::AssetStream>>> {
        Box::pin(async move {
            let objects = self.objects.lock();
            let Some(bytes) = objects.get(key.as_ref()).cloned() else {
                return Ok(None);
            };
            drop(objects);
            let end = if range.end == u64::MAX { bytes.len() as u64 } else { range.end };
            if end > bytes.len() as u64 || range.start > end {
                return Err(NarouError::Platform("asset range exceeds object".into()));
            }
            let start = usize::try_from(range.start)
                .map_err(|_| NarouError::Platform("range overflow".into()))?;
            let end = usize::try_from(end)
                .map_err(|_| NarouError::Platform("range overflow".into()))?;
            let bytes = std::sync::Arc::new(bytes[start..end].to_vec());
            let stream: crate::platform::AssetStream = Box::pin(futures::stream::unfold(
                (bytes, 0usize),
                |(bytes, offset)| async move {
                    if offset == bytes.len() { return None; }
                    let next = (offset + crate::platform::staging::ASSET_CHUNK_BYTES).min(bytes.len());
                    Some((Ok(bytes[offset..next].to_vec()), (bytes, next)))
                },
            ));
            Ok(Some(stream))
        })
    }

    fn write_stream_once<'a>(
        &'a self,
        key: &'a ObjectKey,
        mut stream: crate::platform::AssetStream,
        expected_size: u64,
        expected_sha256: &'a str,
    ) -> PlatformFuture<'a, Result<crate::platform::transfer_stage::WriteOnce>> {
        Box::pin(async move {
            use futures::StreamExt;
            use sha2::Digest;
            let mut data = Vec::new();
            while let Some(chunk) = stream.next().await {
                data.extend(chunk?);
            }
            if data.len() as u64 != expected_size {
                return Err(NarouError::Platform(format!(
                    "write-once size mismatch: {} != {expected_size}",
                    data.len()
                )));
            }
            let digest = hex::encode(sha2::Sha256::digest(&data));
            if !digest.eq_ignore_ascii_case(expected_sha256) {
                return Err(NarouError::Platform(format!(
                    "write-once sha256 mismatch: {digest} != {expected_sha256}"
                )));
            }
            let mut objects = self.objects.lock();
            match objects.get(key.as_ref()) {
                Some(existing) if existing.as_slice() == data.as_slice() => {
                    Ok(crate::platform::transfer_stage::WriteOnce::Reused)
                }
                Some(_) => Err(NarouError::Platform(format!(
                    "write-once conflict: {key} exists with different bytes"
                ))),
                None => {
                    objects.insert(key.as_ref().to_string(), std::sync::Arc::new(data));
                    Ok(crate::platform::transfer_stage::WriteOnce::Written)
                }
            }
        })
    }

    fn write_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        mut stream: crate::platform::AssetStream,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            use futures::StreamExt;
            if let Some(fence)=self.memory_writer(key)?{return self.write_stream_guarded(&fence,key,stream).await;}
            let mut data = Vec::new();
            while let Some(chunk) = stream.next().await {
                data.extend(chunk?);
            }
            if self.memory_writer(key)?.is_some(){return Err(NarouError::Platform("novel_epoch_conflict: admission changed during stream".into()));}
            self.objects.lock().insert(key.as_ref().to_string(), std::sync::Arc::new(data));
            Ok(())
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        ObjectStore::delete(self, key)
    }

    fn copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(data) = self.objects.lock().get(source.as_ref()).cloned() else {
                return Ok(());
            };
            self.objects
                .lock()
                .insert(destination.as_ref().to_string(), data);
            Ok(())
        })
    }

    fn move_or_copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let data = self.objects.lock().remove(source.as_ref());
            if let Some(data) = data {
                self.objects
                    .lock()
                    .insert(destination.as_ref().to_string(), data);
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Transfer stage + access fence mocks
// ---------------------------------------------------------------------------

/// In-memory durable stage. Nested byte/ledger locks always take bytes first.
#[derive(Debug,Default)]
struct MemoryStage {
    ledgers: Mutex<BTreeMap<String, super::transfer_stage::StageLedgerRecord>>,
    checkpoints:Mutex<BTreeMap<(String,u64),super::StageCursor>>,
}

impl MemoryObjectStore {
    fn stage(&self) -> &MemoryStage {
        &self.stage
    }

    fn memory_append(
        &self,
        stage_id: &str,
        owner: &str,
        epoch: u64,
        offset: u64,
        payload: &[u8],
        final_chunk: bool,
    ) -> Result<super::transfer_stage::StageCursor> {
        let data_key = super::transfer_stage::stage_data_key(stage_id)?;
        let mut objects = self.objects.lock();
        let mut ledgers = self.stage().ledgers.lock();
        let mut record = ledgers
            .get(stage_id)
            .cloned()
            .ok_or_else(|| NarouError::Platform(format!("stage not open: {stage_id}")))?;
        record.append(owner,epoch,offset,payload,final_chunk)?;
        let bytes=objects.entry(data_key.to_string()).or_insert_with(||std::sync::Arc::new(Vec::new()));
        std::sync::Arc::make_mut(bytes).extend_from_slice(payload);
        let cursor = record.cursor();
        self.stage().checkpoints.lock().insert((stage_id.to_string(),cursor.confirmed_bytes),cursor.clone());
        ledgers.insert(stage_id.to_string(), record);
        Ok(cursor)
    }

    fn gate_map(&self) -> parking_lot::MutexGuard<'_, BTreeMap<String, MemoryGate>> {
        self.gates.lock()
    }
}

#[derive(Clone, Debug)]
struct MemoryGate {
    origin_url_fold:String,
    content_revision:u64,
    state: super::access_fence::GateState,
    epoch: u64,
    owner: Option<String>,
    updated_at: i64,
    /// reader_id -> (admission epoch, expiry); an old release cannot evict a
    /// new lease reusing the same reader ID after publication.
    readers: BTreeMap<String, (u64,i64)>,
}

impl MemoryGate {
    fn open() -> Self {
        Self {
            origin_url_fold:String::new(),
            content_revision:0,
            state: super::access_fence::GateState::Open,
            epoch: 0,
            owner: None,
            updated_at: 0,
            readers: BTreeMap::new(),
        }
    }

    fn prune(&mut self, now: i64) {
        self.readers.retain(|_, (_,expiry)| *expiry > now);
    }
}

fn gate_key_of(prefix: &super::ObjectPrefix) -> String {
    prefix.as_ref().trim_end_matches('/').to_string()
}

impl super::transfer_stage::TransferStageStore for MemoryObjectStore {
    fn stage_open<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64,ttl:i64)->PlatformFuture<'a,Result<super::StageCursor>>{Box::pin(async move{
        use super::transfer_stage as ts;let key=ts::stage_data_key(id)?;let mut objects=self.objects.lock();let mut ledgers=self.stage().ledgers.lock();
        if let Some(record)=ledgers.get(id){ts::require_owner(record,owner,epoch)?;if record.lease.state==ts::StageLeaseState::Aborting{return Err(NarouError::Platform("stage_aborted".into()));}return Ok(record.cursor());}
        let record=ts::StageLedgerRecord::new(id,owner,epoch,ttl)?;objects.insert(key.to_string(),std::sync::Arc::new(Vec::new()));let cursor=record.cursor();ledgers.insert(id.into(),record);Ok(cursor)
    })}

    fn stage_cursor<'a>(
        &'a self,
        stage_id: &'a str,
    ) -> PlatformFuture<'a, Result<Option<super::transfer_stage::StageCursor>>> {
        Box::pin(async move {
            Ok(self
                .stage()
                .ledgers
                .lock()
                .get(stage_id)
                .map(|record| record.cursor()))
        })
    }

    fn stage_append<'a>(
        &'a self,
        stage_id: &'a str,
        owner: &'a str,
        epoch: u64,
        offset: u64,
        payload: &'a [u8],
        final_chunk: bool,
    ) -> PlatformFuture<'a, Result<super::transfer_stage::StageCursor>> {
        Box::pin(async move { self.memory_append(stage_id, owner, epoch, offset, payload, final_chunk) })
    }

    fn stage_rewind<'a>(
        &'a self,
        stage_id: &'a str,
        owner: &'a str,
        epoch: u64,
        checkpoint: &'a super::transfer_stage::StageCursor,
    ) -> PlatformFuture<'a, Result<super::transfer_stage::StageCursor>> {
        Box::pin(async move {
            use super::transfer_stage as ts;
            let data_key = ts::stage_data_key(stage_id)?;
            let mut objects = self.objects.lock();
            let mut ledgers = self.stage().ledgers.lock();
            let mut record = ledgers
                .get(stage_id)
                .cloned()
                .ok_or_else(|| NarouError::Platform(format!("stage not open: {stage_id}")))?;
            let trusted=if checkpoint.confirmed_bytes==0{ts::StageLedgerRecord::new(stage_id,owner,epoch,1)?.cursor()}else{
                let stored=self.stage().checkpoints.lock().get(&(stage_id.to_string(),checkpoint.confirmed_bytes)).cloned().ok_or_else(||NarouError::Platform("rewind checkpoint missing".into()))?;
                if stored.prefix_sha256()!=checkpoint.prefix_sha256(){return Err(NarouError::Platform("rewind checkpoint SHA conflict".into()));}stored
            };
            record.rewind(owner,epoch,&trusted)?;
            let bytes=objects.get_mut(data_key.as_ref()).ok_or_else(||NarouError::NotFound(data_key.to_string()))?;std::sync::Arc::make_mut(bytes).truncate(trusted.confirmed_bytes as usize);
            let cursor = record.cursor();
            ledgers.insert(stage_id.to_string(), record);
            Ok(cursor)
        })
    }

    fn stage_read<'a>(
        &'a self,
        stage_id: &'a str,
        range: std::ops::Range<u64>,
    ) -> PlatformFuture<'a, Result<Option<crate::platform::AssetStream>>> {
        Box::pin(async move {
            use super::transfer_stage as ts;
            let Some(record) = self.stage().ledgers.lock().get(stage_id).cloned() else {
                return Ok(None);
            };
            if record.lease.state != ts::StageLeaseState::Sealed {
                return Ok(None);
            }
            let key = ts::stage_data_key(stage_id)?;
            self.read_stream_range(&key, range).await
        })
    }

    fn stage_touch<'a>(
        &'a self,
        stage_id: &'a str,
        owner: &'a str,
        epoch: u64,
        lease_secs: i64,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            use super::transfer_stage as ts;
            let now = ts::now_secs();
            let mut ledgers = self.stage().ledgers.lock();
            let record = ledgers
                .get_mut(stage_id)
                .ok_or_else(|| NarouError::Platform(format!("stage not open: {stage_id}")))?;
            ts::require_owner(record, owner, epoch)?;
            record.lease.expires_at = now + lease_secs.max(1);
            Ok(())
        })
    }

    fn stage_seal<'a>(
        &'a self,
        stage_id: &'a str,
        owner: &'a str,
        epoch: u64,
        expected_size: u64,
        expected_sha256: &'a str,
    ) -> PlatformFuture<'a, Result<super::transfer_stage::SealedAsset>> {
        Box::pin(async move {
            use super::transfer_stage as ts;let key=ts::stage_data_key(stage_id)?;
            let objects=self.objects.lock();let bytes=objects.get(key.as_ref()).ok_or_else(||NarouError::NotFound(key.to_string()))?;
            let mut ledgers=self.stage().ledgers.lock();let mut record=ledgers.get(stage_id).cloned().ok_or_else(||NarouError::Platform("stage not open".into()))?;
            let sealed=record.verify_seal(owner,epoch,expected_size,expected_sha256)?;if bytes.len() as u64!=expected_size{return Err(NarouError::Platform("stage physical size mismatch".into()));}
            record.lease.state=ts::StageLeaseState::Sealed;ledgers.insert(stage_id.to_string(),record);
            self.verified.lock().insert(key.to_string(),MemoryFileProof{sha256:sealed.sha256.clone(),source:key.to_string(),bytes:std::sync::Arc::downgrade(bytes)});Ok(sealed)
        })
    }

    fn stage_abort<'a>(
        &'a self,
        stage_id: &'a str,
        owner: &'a str,
        epoch: u64,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            use super::transfer_stage as ts;
            let data_key = ts::stage_data_key(stage_id)?;
            let mut objects = self.objects.lock();
            let mut ledgers = self.stage().ledgers.lock();
            if let Some(record) = ledgers.get(stage_id) {
                ts::require_owner(record, owner, epoch)?;
            }
            ledgers.remove(stage_id);
            objects.remove(data_key.as_ref());
            Ok(())
        })
    }

    fn stage_lease_page<'a>(
        &'a self,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<(Vec<super::transfer_stage::StageLease>, Option<String>)>> {
        Box::pin(async move {
            let ledgers = self.stage().ledgers.lock();
            let mut items: Vec<(String, super::transfer_stage::StageLease)> = ledgers
                .iter()
                .map(|(id, record)| (id.clone(), record.lease()))
                .collect();
            items.sort_by(|a, b| a.0.cmp(&b.0));
            let (page, next) = super::object_store::paginate_object_listing(
                items,
                cursor,
                limit.max(1),
                |item| item.0.as_str(),
            );
            Ok((page.into_iter().map(|(_, lease)| lease).collect(), next))
        })
    }

    fn stage_reclaim<'a>(
        &'a self,
        stage_id: &'a str,
        expected_owner: &'a str,
        expected_epoch: u64,
    ) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async move {
            let data_key = super::transfer_stage::stage_data_key(stage_id)?;
            let mut objects = self.objects.lock();
            let mut ledgers = self.stage().ledgers.lock();
            let Some(record) = ledgers.get(stage_id) else {
                return Ok(true);
            };
            if record.lease.owner != expected_owner || record.lease.epoch != expected_epoch || record.lease.expires_at>super::transfer_stage::now_secs() {
                return Ok(false);
            }
            ledgers.remove(stage_id);
            objects.remove(data_key.as_ref());
            Ok(true)
        })
    }
}

#[derive(Debug)]
struct MemoryGateProof(std::sync::Arc<Mutex<BTreeMap<String,MemoryGate>>>);
impl super::access_fence::WriteFenceProof for MemoryGateProof{
    fn source_id(&self)->usize{std::sync::Arc::as_ptr(&self.0) as usize}
    fn run_many(&self,fences:&[super::WriteFence],invalidate:bool,operation:&mut dyn FnMut()->Result<()>)->Result<()>{
        let mut gates=self.0.lock();let source=std::sync::Arc::as_ptr(&self.0) as usize;
        for fence in fences{if fence.proof_source_id()!=Some(source){return Err(NarouError::Platform("mixed library writer fences".into()));}let gate=gates.get(&gate_key_of(&fence.prefix)).ok_or_else(||super::access_fence::gate_closed_error(&fence.prefix))?;if gate.state!=super::GateState::Open||gate.epoch!=fence.epoch{return Err(super::access_fence::gate_closed_error(&fence.prefix));}
            if invalidate&&gate.epoch==u64::MAX{return Err(NarouError::Platform("gate epoch overflow".into()));}}
        operation()?;if invalidate{for fence in fences{let gate=gates.get_mut(&gate_key_of(&fence.prefix)).expect("validated gate");if gate.epoch==fence.epoch{gate.epoch+=1;}}}Ok(())
    }
}
impl super::access_fence::NovelAccessFence for MemoryObjectStore {
    fn retire_writer<'a>(&'a self,fence:&'a super::WriteFence,owner:&'a str)->PlatformFuture<'a,Result<u64>>{Box::pin(async move{super::access_fence::parse_removal_owner(owner)?;let next=fence.epoch.checked_add(1).ok_or_else(||NarouError::Platform("gate epoch overflow".into()))?;let mut gates=self.gate_map();let gate=gates.entry(gate_key_of(&fence.prefix)).or_insert_with(MemoryGate::open);
        if fence.proof_source_id()!=Some(std::sync::Arc::as_ptr(&self.gates) as usize){return Err(NarouError::Platform("mixed library writer fence".into()));}
        if gate.state==super::GateState::Closed&&gate.owner.as_deref()==Some(owner)&&gate.epoch==next{return Ok(next);}
        if gate.state!=super::GateState::Open||gate.epoch!=fence.epoch{return Err(super::access_fence::gate_closed_error(&fence.prefix));}gate.epoch=next;gate.state=super::GateState::Closed;gate.owner=Some(owner.into());gate.updated_at=super::transfer_stage::now_secs();Ok(next)
    })}
    fn pending_removals<'a>(&'a self,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,Result<super::access_fence::PendingRemovalPage>>{Box::pin(async move{let cap=limit.clamp(1,100);let gates=self.gate_map();let mut out=Vec::new();let mut last=None;
        for(index,(prefix,gate))in gates.range::<str,_>((std::ops::Bound::Excluded(cursor.unwrap_or("")),std::ops::Bound::Unbounded)).enumerate(){if index==cap{return Ok((out,last));}last=Some(prefix.clone());if gate.state==super::GateState::Closed&&let Some(owner)=gate.owner.as_ref().filter(|owner|owner.starts_with("remove-v1:")){super::access_fence::parse_removal_owner(owner)?;out.push((super::ObjectPrefix::new(prefix.as_str())?,owner.clone(),gate.epoch));}}Ok((out,None))
    })}
    fn capture_writer<'a>(&'a self,prefix:&'a super::ObjectPrefix)->PlatformFuture<'a,Result<super::WriteFence>>{Box::pin(async move{
        let mut gates=self.gate_map();let gate=gates.entry(gate_key_of(prefix)).or_insert_with(MemoryGate::open);if gate.state!=super::GateState::Open{return Err(super::access_fence::gate_closed_error(prefix));}
        Ok(super::WriteFence::with_proof(prefix.clone(),gate.epoch,std::sync::Arc::new(MemoryGateProof(self.gates.clone()))))
    })}
    fn rebind_writer<'a>(&'a self,prefix:&'a super::ObjectPrefix,expected_epoch:u64)->PlatformFuture<'a,Result<super::WriteFence>>{Box::pin(async move{let mut gates=self.gate_map();let gate=gates.entry(gate_key_of(prefix)).or_insert_with(MemoryGate::open);if gate.state!=super::GateState::Open||gate.epoch!=expected_epoch{return Err(super::access_fence::gate_epoch_error(prefix,expected_epoch,gate.epoch));}Ok(super::WriteFence::with_proof(prefix.clone(),expected_epoch,std::sync::Arc::new(MemoryGateProof(self.gates.clone()))))})}

    fn admit_reader<'a>(
        &'a self,
        prefix: &'a super::ObjectPrefix,
        reader_id: &'a str,
        lease_secs: i64,
    ) -> PlatformFuture<'a, Result<super::access_fence::GateLease>> {
        Box::pin(async move {
            use super::access_fence as af;
            let now = super::transfer_stage::now_secs();
            let mut gates = self.gate_map();
            let gate = gates.entry(gate_key_of(prefix)).or_insert_with(MemoryGate::open);
            gate.prune(now);
            if gate.state != af::GateState::Open {
                return Err(af::gate_closed_error(prefix));
            }
            let expires = now.saturating_add(lease_secs.clamp(1,af::READER_LEASE_SECS));
            gate.readers.insert(reader_id.to_string(), (gate.epoch,expires));
            Ok(af::GateLease {
                reader_id: reader_id.to_string(),
                epoch: gate.epoch,
                expires_at: expires,
            })
        })
    }

    fn renew_reader<'a>(
        &'a self,
        prefix: &'a super::ObjectPrefix,
        lease: &'a super::access_fence::GateLease,
        lease_secs: i64,
    ) -> PlatformFuture<'a, Result<super::access_fence::GateLease>> {
        Box::pin(async move {
            use super::access_fence as af;
            let now = super::transfer_stage::now_secs();
            let mut gates = self.gate_map();
            let gate = gates.entry(gate_key_of(prefix)).or_insert_with(MemoryGate::open);
            gate.prune(now);
            if gate.state != af::GateState::Open || gate.epoch != lease.epoch {
                return Err(af::gate_closed_error(prefix));
            }
            // `prune` already dropped expired rows, so a missing id is a dead
            // admission; a same-id row at another epoch is a stale token.
            let Some(current) = gate.readers.get_mut(&lease.reader_id) else {
                return Err(af::lease_expired_error(prefix));
            };
            if current.0 != lease.epoch { return Err(af::gate_closed_error(prefix)); }
            current.1 = now.saturating_add(lease_secs.clamp(1, af::READER_LEASE_SECS));
            let expires_at = current.1;
            Ok(af::GateLease {
                reader_id: lease.reader_id.clone(),
                epoch: gate.epoch,
                expires_at,
            })
        })
    }

    fn release_reader<'a>(
        &'a self,
        prefix: &'a super::ObjectPrefix,
        lease: &'a super::access_fence::GateLease,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut gates = self.gate_map();
            if let Some(gate) = gates.get_mut(&gate_key_of(prefix)) {
                gate.prune(super::transfer_stage::now_secs());
                if gate.readers.get(&lease.reader_id).is_some_and(|(epoch,_)|*epoch==lease.epoch) {
                    gate.readers.remove(&lease.reader_id);
                }
            }
            Ok(())
        })
    }

    fn gate_info<'a>(
        &'a self,
        prefix: &'a super::ObjectPrefix,
    ) -> PlatformFuture<'a, Result<super::access_fence::GateInfo>> {
        Box::pin(async move {
            let now = super::transfer_stage::now_secs();
            let mut gates = self.gate_map();
            let gate = gates.entry(gate_key_of(prefix)).or_insert_with(MemoryGate::open);
            gate.prune(now);
            Ok(super::access_fence::GateInfo {
                epoch: gate.epoch,
                content_revision:gate.content_revision,
                state: gate.state,
                pending_readers: gate.readers.len() as u32,
                owner: gate.owner.clone(),
                updated_at: gate.updated_at,
            })
        })
    }

    fn close_gate<'a>(
        &'a self,
        prefix: &'a super::ObjectPrefix,
        owner: &'a str,
    ) -> PlatformFuture<'a, Result<u64>> {
        Box::pin(async move {
            use super::access_fence as af;
            let now = super::transfer_stage::now_secs();
            let mut gates = self.gate_map();
            let gate = gates.entry(gate_key_of(prefix)).or_insert_with(MemoryGate::open);
            gate.prune(now);
            match gate.state {
                af::GateState::Closed => {
                    if gate.owner.as_deref() == Some(owner) {
                        return Ok(gate.epoch);
                    }
                    return Err(af::gate_owner_error(
                        prefix,
                        gate.owner.as_deref().unwrap_or("<unknown>"),
                    ));
                }
                af::GateState::Open => {}
            }
            gate.state = af::GateState::Closed;
            gate.epoch = gate.epoch.saturating_add(1);
            gate.owner = Some(owner.to_string());
            gate.updated_at = now;
            Ok(gate.epoch)
        })
    }

    fn renew_gate<'a>(
        &'a self,
        prefix: &'a super::ObjectPrefix,
        owner: &'a str,
        epoch: u64,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            use super::access_fence as af;
            let now = super::transfer_stage::now_secs();
            let mut gates = self.gate_map();
            let gate = gates
                .get_mut(&gate_key_of(prefix))
                .ok_or_else(|| af::gate_closed_error(prefix))?;
            if gate.owner.as_deref() != Some(owner) {
                return Err(af::gate_owner_error(prefix, gate.owner.as_deref().unwrap_or("<unknown>")));
            }
            if gate.epoch != epoch {
                return Err(af::gate_epoch_error(prefix, epoch, gate.epoch));
            }
            gate.updated_at = now;
            Ok(())
        })
    }

    fn open_gate<'a>(
        &'a self,
        prefix: &'a super::ObjectPrefix,
        owner: &'a str,
        epoch: u64,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            use super::access_fence as af;
            let now = super::transfer_stage::now_secs();
            let mut gates = self.gate_map();
            let gate = gates
                .get_mut(&gate_key_of(prefix))
                .ok_or_else(|| af::gate_closed_error(prefix))?;
            if gate.owner.as_deref() != Some(owner) || gate.epoch != epoch {
                return Err(af::gate_epoch_error(prefix, epoch, gate.epoch));
            }
            if owner.starts_with("remove-v1:")&&super::access_fence::parse_removal_owner(owner)?.1{gate.origin_url_fold.clear();}
            gate.state = af::GateState::Open;
            gate.owner = None;
            gate.updated_at = now;
            Ok(())
        })
    }

    fn commit_staged<'a>(
        &'a self,
        prefix: &'a super::ObjectPrefix,
        owner: &'a str,
        epoch: u64,
        stage: &'a ObjectKey,
        canonical: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            use super::access_fence as af;
            let mut gates = self.gate_map();
            let gate = gates
                .get_mut(&gate_key_of(prefix))
                .ok_or_else(|| af::gate_closed_error(prefix))?;
            if gate.state != af::GateState::Closed
                || gate.owner.as_deref() != Some(owner)
                || gate.epoch != epoch
            {
                return Err(af::gate_epoch_error(prefix, epoch, gate.epoch));
            }
            if !prefix.matches(canonical)||!super::is_reserved_key(stage){return Err(NarouError::Platform("invalid install namespace".into()));}
            gate.prune(super::transfer_stage::now_secs());if !gate.readers.is_empty(){return Err(af::novel_busy_error(prefix));}
            let mut objects=self.objects.lock();let mut proofs=self.verified.lock();
            if !objects.contains_key(stage.as_ref()){
                let proof=proofs.get(canonical.as_ref()).filter(|proof|proof.source==stage.as_ref()).ok_or_else(||NarouError::NotFound(stage.to_string()))?;
                let expected=proof.bytes.upgrade().ok_or_else(||NarouError::Platform("installed generation proof expired".into()))?;let current=objects.get(canonical.as_ref()).ok_or_else(||NarouError::NotFound(canonical.to_string()))?;
                if !std::sync::Arc::ptr_eq(current,&expected){return Err(NarouError::Platform("installed generation changed".into()));}return Ok(());
            }
            let value=objects.get(stage.as_ref()).expect("stage present");let proof=proofs.get(stage.as_ref()).ok_or_else(||NarouError::Platform("stage seal proof missing".into()))?;
            let expected=proof.bytes.upgrade().ok_or_else(||NarouError::Platform("stage generation expired".into()))?;if !std::sync::Arc::ptr_eq(value,&expected){return Err(NarouError::Platform("stage generation changed".into()));}
            let proof=MemoryFileProof{sha256:proof.sha256.clone(),source:stage.to_string(),bytes:std::sync::Arc::downgrade(value)};
            let value=objects.remove(stage.as_ref()).expect("stage present");objects.insert(canonical.to_string(),value);proofs.insert(canonical.to_string(),proof);gate.content_revision+=1;Ok(())
        })
    }
    fn remove_committed<'a>(&'a self,prefix:&'a super::ObjectPrefix,owner:&'a str,epoch:u64,key:&'a ObjectKey,sha:&'a str)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        let mut gates=self.gate_map();let gate=gates.get_mut(&gate_key_of(prefix)).ok_or_else(||super::access_fence::gate_closed_error(prefix))?;gate.prune(super::transfer_stage::now_secs());
        if !prefix.matches(key)||gate.owner.as_deref()!=Some(owner)||gate.epoch!=epoch||!gate.readers.is_empty(){return Err(super::access_fence::gate_closed_error(prefix));}
        let mut objects=self.objects.lock();if let Some(bytes)=objects.get(key.as_ref()){let proofs=self.verified.lock();let proof=proofs.get(key.as_ref()).ok_or_else(||NarouError::Platform("recovery seal proof missing".into()))?;
            let expected=proof.bytes.upgrade().ok_or_else(||NarouError::Platform("recovery generation expired".into()))?;if !proof.sha256.eq_ignore_ascii_case(sha)||!std::sync::Arc::ptr_eq(bytes,&expected){return Err(NarouError::Platform("recovery generation fingerprint conflict".into()));}objects.remove(key.as_ref());gate.content_revision+=1;}Ok(())
    })}
}

/// In-memory novel repository seeded from a map of records.
///
/// Implements the full [`NovelRepository`] contract with no filesystem,
/// network, or sleeping — the basis for downloader/update integration tests.
#[derive(Debug, Default)]
pub struct MemoryNovelRepository {
    records: Mutex<BTreeMap<i64, NovelRecord>>,
    id_allocator: Mutex<super::id_allocator::IdAllocator>,
    objects:Option<std::sync::Arc<MemoryObjectStore>>,
    creation:Mutex<creation::CreationState>,
}

impl MemoryNovelRepository {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_objects(mut self,objects:std::sync::Arc<MemoryObjectStore>)->Self{self.objects=Some(objects);self}

    pub fn from_records(records: Vec<NovelRecord>) -> Self {
        let mut stored = BTreeMap::new();
        for record in records {
            let id = record.id;
            stored.insert(id, record);
        }
        let next_id = stored.last_key_value().map_or(0, |(id, _)| id.saturating_add(1));
        let id_allocator = super::id_allocator::IdAllocator::with_next_id(next_id);
        Self { records: Mutex::new(stored), id_allocator: Mutex::new(id_allocator),objects:None,creation:Mutex::default() }
    }

    pub fn len(&self) -> usize {
        self.records.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[path="memory_creation.rs"]
mod creation;

impl NovelRepository for MemoryNovelRepository {
    fn resume_id(&self,id:NovelId,generation:i64)->PlatformFuture<'_,Result<super::NovelIdReservation>>{Box::pin(async move{let reservation=super::NovelIdReservation::new(id,generation);if self.id_allocator.lock().owns(&reservation)||self.creation.lock().has_receipt(id,generation){Ok(reservation)}else{Err(NarouError::Platform("numeric reservation resume lost its exact generation".into()))}})}
    fn reserve_create<'a>(&'a self,fence:&'a super::WriteFence,reservation:&'a super::NovelIdReservation,toc_url:&'a str)->PlatformFuture<'a,Result<()>>{Box::pin(async move{self.reserve_memory_creation(fence,reservation,toc_url)})}
    fn apply_create_guarded<'a>(&'a self,fence:&'a super::WriteFence,reservation:&'a super::NovelIdReservation,record:NovelRecord)->PlatformFuture<'a,Result<()>>{Box::pin(async move{self.commit_memory_creation(fence,reservation,record)})}
    fn remove_closed<'a>(&'a self,prefix:&'a super::ObjectPrefix,owner:&'a str,epoch:u64,id:NovelId)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if super::access_fence::parse_removal_owner(owner)?.0!=id{return Err(NarouError::Platform("removal owner ID mismatch".into()));}let objects=self.objects.as_ref().ok_or_else(||NarouError::Platform("Memory repository has no bound object-store gate authority".into()))?;let mut gates=objects.gates.lock();let gate=gates.get_mut(&gate_key_of(prefix)).ok_or_else(||super::access_fence::gate_closed_error(prefix))?;gate.prune(super::transfer_stage::now_secs());
        if gate.state!=super::GateState::Closed||gate.owner.as_deref()!=Some(owner)||gate.epoch!=epoch{return Err(super::access_fence::gate_closed_error(prefix));}
        if !gate.readers.is_empty(){return Err(super::access_fence::novel_busy_error(prefix));}
        let mut records=self.records.lock();if let Some(record)=records.get(&id.0){let keys=super::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;if keys.prefix().as_ref()!=prefix.as_ref().trim_end_matches('/'){return Err(NarouError::Platform("removal record identity changed".into()));}let url=crate::db::novel_codec::fold(&record.toc_url);if !gate.origin_url_fold.is_empty()&&gate.origin_url_fold!=url{return Err(NarouError::Platform("retained record origin changed".into()));}gate.origin_url_fold=url;}records.remove(&id.0);Ok(())
    })}
    fn get_current<'a>(&'a self,id:NovelId)->PlatformFuture<'a,Result<Option<NovelRecord>>>{self.get(id)}
    fn get_many_current<'a>(&'a self,ids:&'a [NovelId])->PlatformFuture<'a,Result<Vec<Option<NovelRecord>>>>{Box::pin(async move{let records=self.records.lock();Ok(ids.iter().map(|id|records.get(&id.0).cloned()).collect())})}
    fn query_current<'a>(&'a self,query:&'a NovelQuery)->PlatformFuture<'a,Result<Vec<NovelRecord>>>{self.query(query)}
    fn find_by_toc_url_current<'a>(&'a self,url:&'a str)->PlatformFuture<'a,Result<Option<NovelRecord>>>{self.find_by_toc_url(url)}
    fn find_by_storage_prefix_current<'a>(&'a self,prefix:&'a ObjectKey)->PlatformFuture<'a,Result<Option<NovelRecord>>>{Box::pin(async move{for record in self.records.lock().values(){let keys=super::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;if keys.prefix()==prefix{return Ok(Some(record.clone()));}}Ok(None)})}
    fn apply_batch_guarded_many<'a>(&'a self,fences:&'a [super::WriteFence],mut mutations:Vec<NovelMutation>)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        let invalidate=mutations.iter().any(|mutation|matches!(mutation,NovelMutation::Remove(_)));
        super::WriteFence::run_many_proven(fences,invalidate,&mut||{let mut records=self.records.lock();let mut allocator=self.id_allocator.lock();
            self.creation.lock().validate_mutations(&records,&mutations)?;
            for mutation in &mutations{let record=match mutation{NovelMutation::Upsert(record)=>Some(record),NovelMutation::Remove(id)=>records.get(&id.0)};if let Some(record)=record{let keys=super::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;if !fences.iter().any(|fence|keys.prefix().as_ref()==fence.prefix.as_ref().trim_end_matches('/')){return Err(NarouError::Platform("record outside captured fence".into()));}}}
            for mutation in &mutations{let id=match mutation{NovelMutation::Upsert(record)=>record.id,NovelMutation::Remove(id)=>id.0};if let Some(previous)=records.get(&id){let keys=super::NovelObjectKeys::new(&previous.sitename,&previous.file_title,previous.use_subdirectory)?;if !fences.iter().any(|fence|keys.prefix().as_ref()==fence.prefix.as_ref().trim_end_matches('/')){return Err(NarouError::Platform("previous record origin outside captured fence".into()));}}}
            for mutation in mutations.drain(..){match mutation{NovelMutation::Upsert(record)=>{allocator.commit(record.id);records.insert(record.id,record);},NovelMutation::Remove(id)=>{records.remove(&id.0);}}}Ok(())
        })
    })}

    fn get<'a>(&'a self, id: NovelId) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        Box::pin(async move { Ok(self.records.lock().get(&id.0).cloned()) })
    }

    fn find_by_toc_url<'a>(
        &'a self,
        url: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let needle = url.trim().to_lowercase();
        Box::pin(async move {
            Ok(self
                .records
                .lock()
                .values()
                .find(|r| r.toc_url.trim().to_lowercase() == needle)
                .cloned())
        })
    }

    fn find_by_title<'a>(
        &'a self,
        title: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let needle = title.trim().to_lowercase();
        Box::pin(async move {
            let records = self.records.lock();
            // Exact (index) lookup first, then case-insensitive fallback,
            // matching the native `Database::find_by_title` semantics.
            Ok(records
                .values()
                .find(|r| r.title.trim().to_lowercase() == needle)
                .cloned())
        })
    }

    fn find_by_ncode<'a>(
        &'a self,
        ncode: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let ncode = ncode.to_lowercase();
        Box::pin(async move {
            Ok(self
                .records
                .lock()
                .values()
                .find(|r| {
                    r.ncode
                        .as_deref()
                        .is_some_and(|nc| nc.eq_ignore_ascii_case(&ncode))
                        || r.toc_url
                            .to_lowercase()
                            .trim_end_matches('/')
                            .ends_with(&format!("/{ncode}"))
                })
                .cloned())
        })
    }

    fn count<'a>(&'a self, filter: &'a NovelFilter) -> PlatformFuture<'a, Result<u64>> {
        let filter = filter.clone();
        Box::pin(async move {
            Ok(self
                .records
                .lock()
                .values()
                .filter(|r| crate::platform::repository::record_matches_filter(r, &filter))
                .count() as u64)
        })
    }

    fn query<'a>(&'a self, query: &'a NovelQuery) -> PlatformFuture<'a, Result<Vec<NovelRecord>>> {
        Box::pin(async move {
            Ok(crate::platform::repository::query_records(self.records.lock().values(), query))
        })
    }

    fn scan_ids<'a>(
        &'a self,
        filter: &'a NovelFilter,
        after_id: Option<NovelId>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<NovelId>>> {
        let filter = filter.clone();
        let after_id = after_id.map(|id| id.0).unwrap_or(i64::MIN);
        Box::pin(async move {
            let ids: Vec<NovelId> = self
                .records
                .lock()
                .values()
                .filter(|r| {
                    r.id > after_id
                        && crate::platform::repository::record_matches_filter(r, &filter)
                })
                .take(limit)
                .map(|r| NovelId::from(r.id))
                .collect();
            Ok(ids)
        })
    }

    fn allocate_id(&self) -> PlatformFuture<'_, Result<super::NovelIdReservation>> {
        Box::pin(async move {
            let _records = self.records.lock();
            self.id_allocator.lock().allocate()
        })
    }

    fn release_id(&self, reservation: super::NovelIdReservation) -> PlatformFuture<'_, Result<()>> {
        Box::pin(async move {self.release_memory_creation(reservation)})
    }

    fn apply_batch<'a>(&'a self, mutations: Vec<NovelMutation>) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut records = self.records.lock();
            let mut id_allocator = self.id_allocator.lock();
            self.creation.lock().validate_mutations(&records,&mutations)?;
            for mutation in mutations {
                match mutation {
                    NovelMutation::Upsert(record) => {
                        let id = record.id;
                        id_allocator.commit(id);
                        records.insert(id, record);
                    }
                    NovelMutation::Remove(id) => {
                        records.remove(&id.0);
                    }
                }
            }
            Ok(())
        })
    }
}

/// A rate limiter that never waits. Used by tests to run downloads without
/// sleeping; request counts can be observed if needed.
#[derive(Debug, Default)]
pub struct FakeRateLimiter {
    acquisitions: AtomicU64,
}

impl FakeRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn acquisition_count(&self) -> u64 {
        self.acquisitions.load(Ordering::SeqCst)
    }
}

impl RateLimiter for FakeRateLimiter {
    fn acquire<'a>(&'a self, _scope: &'a RateLimitScope) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            self.acquisitions.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

/// In-memory [`CookieStore`] for tests and the Worker runtime.
#[derive(Debug, Default, Clone)]
pub struct MemoryCookieStore {
    groups: std::sync::Arc<
        parking_lot::Mutex<std::collections::BTreeMap<String, Vec<LoginGroup>>>,
    >,
}

impl MemoryCookieStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.groups.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl CookieStore for MemoryCookieStore {
    fn load_groups<'a>(&'a self, site: &'a str) -> PlatformFuture<'a, Result<Vec<LoginGroup>>> {
        let site = site.trim().to_ascii_lowercase();
        Box::pin(async move {
            Ok(self
                .groups
                .lock()
                .get(&site)
                .cloned()
                .unwrap_or_default())
        })
    }

    fn save_groups<'a>(
        &'a self,
        site: &'a str,
        groups: &'a [LoginGroup],
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            crate::platform::cookie_store::save_site_groups(
                &mut self.groups.lock(),
                site,
                groups,
            );
            Ok(())
        })
    }

    fn compare_save_groups<'a>(
        &'a self,
        site: &'a str,
        expected: &'a [LoginGroup],
        replacement: &'a [LoginGroup],
    ) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async move {
            Ok(crate::platform::cookie_store::compare_save_site_groups(
                &mut self.groups.lock(),
                site,
                expected,
                replacement,
            ))
        })
    }

    fn clear<'a>(&'a self, site: &'a str) -> PlatformFuture<'a, Result<bool>> {
        let site = site.trim().to_ascii_lowercase();
        Box::pin(async move { Ok(self.groups.lock().remove(&site).is_some()) })
    }

    fn list_groups(&self) -> PlatformFuture<'_, Result<std::collections::BTreeMap<String, Vec<LoginGroup>>>> {
        Box::pin(async move { Ok(self.groups.lock().clone()) })
    }

    fn save_all_groups<'a>(
        &'a self,
        groups: &'a BTreeMap<String, Vec<LoginGroup>>,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            *self.groups.lock() =
                crate::platform::cookie_store::replace_site_groups(groups);
            Ok(())
        })
    }

    fn merge_all_groups<'a>(
        &'a self,
        groups: &'a BTreeMap<String, Vec<LoginGroup>>,
    ) -> PlatformFuture<'a, Result<usize>> {
        Box::pin(async move {
            Ok(crate::platform::cookie_store::merge_site_groups(
                &mut self.groups.lock(),
                groups,
            ))
        })
    }

    fn renew_response_cookies<'a>(
        &'a self,
        sent_cookie: &'a str,
        set_cookie: &'a [String],
    ) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async move {
            Ok(crate::platform::cookie_store::renew_sent_login(
                &mut self.groups.lock(),
                sent_cookie,
                set_cookie,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::SystemClock;
    use crate::platform::clock::Clock;
    use crate::platform::{NovelSort, NovelSortKey};

    fn sample_record(id: i64) -> NovelRecord {
        NovelRecord {
            id,
            author: "作者".into(),
            title: "テスト小説".into(),
            file_title: "test".into(),
            toc_url: format!("https://example.com/novel/{id}"),
            sitename: "example".into(),
            novel_type: 1,
            end: false,
            last_update: chrono::Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn mock_http_serves_canned_and_records() {
        let client = MockHttpClient::new();
        client.add_text("https://example.com/toc", 200, "<html>toc</html>");

        let body = futures::executor::block_on(async {
            let resp = client
                .send(HttpRequest::get("https://example.com/toc"))
                .await
                .unwrap();
            String::from_utf8_lossy(&resp.body).into_owned()
        });
        assert_eq!(body, "<html>toc</html>");

        let err = futures::executor::block_on(async {
            client
                .send(HttpRequest::get("https://example.com/missing"))
                .await
                .unwrap_err()
        });
        assert!(err.to_string().contains("no canned response"));

        let urls = client.requested_urls();
        assert_eq!(urls.len(), 2);
        assert!(urls[0].starts_with("GET https://example.com/toc"));
    }

    #[test]
    fn memory_store_roundtrip() {
        let store = MemoryObjectStore::new();
        let key = ObjectKey::try_new("novel/1/toc.yaml").unwrap();
        futures::executor::block_on(async {
            assert!(!store.exists(&key).await.unwrap());
            assert!(store.read_small(&key).await.unwrap().is_none());

            store.write_small(&key, b"data".to_vec()).await.unwrap();
            assert!(store.exists(&key).await.unwrap());
            assert_eq!(store.read_small(&key).await.unwrap().unwrap(), b"data");

            let prefix = crate::platform::ObjectPrefix::new("novel/1").unwrap();
            let request = ObjectListRequest::new(prefix, std::num::NonZeroUsize::new(10).unwrap());
            let listed = store.list_page(&request).await.unwrap().objects;
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].key, key);
            assert_eq!(listed[0].size, 4);

            ObjectStore::delete(&store, &key).await.unwrap();
            assert!(!store.exists(&key).await.unwrap());
        });
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn memory_asset_stream_preserves_chunk_boundaries_and_bytes() {
        let store = MemoryObjectStore::new();
        let key = ObjectKey::try_new("assets/large.bin").unwrap();
        let input: Vec<u8> = (0..(64 * 1024 + 17))
            .map(|value| (value % 251) as u8)
            .collect();
        futures::executor::block_on(async {
            let stream: crate::platform::AssetStream =
                Box::pin(futures::stream::iter(vec![Ok(input.clone())]));
            AssetStore::write_stream(&store, &key, stream)
                .await
                .unwrap();
            let mut stream = AssetStore::read_stream(&store, &key)
                .await
                .unwrap()
                .unwrap();
            use futures::StreamExt;
            let mut output = Vec::new();
            let mut chunks = 0;
            while let Some(chunk) = stream.next().await {
                chunks += 1;
                output.extend(chunk.unwrap());
            }
            assert!(chunks >= 2);
            assert_eq!(output, input);
        });
    }

    #[test]
    fn memory_repository_crud_and_query() {
        let repo = MemoryNovelRepository::new();
        let mut record = sample_record(1);
        record.author = "作者".into();
        record.title = "テスト小説".into();
        let mut record2 = sample_record(2);
        record2.author = "別作者".into();
        record2.title = "なろう作品".into();

        futures::executor::block_on(repo.apply_batch(vec![
            NovelMutation::Upsert(record.clone()),
            NovelMutation::Upsert(record2.clone()),
        ]))
        .unwrap();
        assert_eq!(repo.len(), 2);

        let found = futures::executor::block_on(repo.get(NovelId::from(1)))
            .unwrap()
            .unwrap();
        assert_eq!(found.title, "テスト小説");

        let mut updated = record.clone();
        updated.title = "更新後タイトル".into();
        futures::executor::block_on(repo.apply_batch(vec![NovelMutation::Upsert(updated)]))
            .unwrap();
        assert_eq!(
            futures::executor::block_on(repo.get(NovelId::from(1)))
                .unwrap()
                .unwrap()
                .title,
            "更新後タイトル"
        );

        let query = NovelQuery::page(
            NovelFilter {
                keyword: Some("なろう".into()),
                ..Default::default()
            },
            NovelSort::by(NovelSortKey::Id),
            0,
            10,
        );
        let results = futures::executor::block_on(repo.query(&query)).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 2);

        futures::executor::block_on(
            repo.apply_batch(vec![NovelMutation::Remove(NovelId::from(1))]),
        )
        .unwrap();
        assert_eq!(repo.len(), 1);
    }

    #[test]
    fn imported_record_ids_are_excluded_from_new_reservations() {
        let repo = MemoryNovelRepository::from_records(vec![sample_record(5)]);
        let reservation = futures::executor::block_on(repo.allocate_id()).unwrap();
        assert_eq!(reservation.id(),NovelId(6));
    }


    #[test]
    fn memory_repository_scan_ids_uses_keyset_pagination() {
        let repo = MemoryNovelRepository::from_records(vec![
            sample_record(1),
            sample_record(2),
            sample_record(3),
            sample_record(4),
            sample_record(5),
        ]);
        let filter = NovelFilter::all();

        let page1 = futures::executor::block_on(repo.scan_ids(&filter, None, 2)).unwrap();
        assert_eq!(page1, vec![NovelId::from(1), NovelId::from(2)]);

        let page2 =
            futures::executor::block_on(repo.scan_ids(&filter, page1.last().copied(), 2)).unwrap();
        assert_eq!(page2, vec![NovelId::from(3), NovelId::from(4)]);

        let page3 =
            futures::executor::block_on(repo.scan_ids(&filter, page2.last().copied(), 2)).unwrap();
        assert_eq!(page3, vec![NovelId::from(5)]);
    }

    #[test]
    fn memory_repository_count_and_find_by_ncode() {
        let mut record = sample_record(1);
        record.ncode = Some("n1234ab".into());
        record.toc_url = "https://ncode.syosetu.com/n1234ab/".into();
        let repo = MemoryNovelRepository::from_records(vec![record]);

        let filter = NovelFilter {
            is_narou: Some(true),
            ..Default::default()
        };
        assert_eq!(futures::executor::block_on(repo.count(&filter)).unwrap(), 0);

        let found = futures::executor::block_on(repo.find_by_ncode("N1234AB"))
            .unwrap()
            .unwrap();
        assert_eq!(found.id, 1);
    }

    #[test]
    fn fake_rate_limiter_counts_acquisitions() {
        let limiter = FakeRateLimiter::new();
        let scope = RateLimitScope::site("example.com");
        futures::executor::block_on(async {
            limiter.acquire(&scope).await.unwrap();
            limiter.acquire(&scope).await.unwrap();
        });
        assert_eq!(limiter.acquisition_count(), 2);
    }

    #[test]
    fn system_clock_is_a_clock() {
        let clock = SystemClock;
        let _ = clock.now_utc();
        let _ = clock.now_unix_secs();
    }
}
