#![cfg(feature = "native-runtime")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use narou_rs::application::transfer::{sha256_hex, TransferService};
use narou_rs::transfer::ArchiveManifest;
use narou_rs::transfer::projection::{FileRole, ProjectionDigest, TransferProfile};
use narou_rs::transfer::protocol::{self, BatchHeader, FrameDecoder, FrameEncoder, WireRecord, WireHeader, WireNovel, WireText, WireFile, WireEnd, NovelKind};
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde_json::{json, Value};

struct Server { child: Child, base: String, token: String }
impl Drop for Server {
    fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
}

fn start(library: &Path) -> Server {
    std::fs::create_dir_all(library.join(".narou")).unwrap();
    std::fs::create_dir_all(library.join(".narousetting")).unwrap();
    let global = library.join(".narousetting/global_setting.yaml");
    if !global.exists() {
        std::fs::write(global, "server-ws-port: 0\n").unwrap();
    }
    // Keep SQLite auxiliary scratch files inside the owned fixture rather
    // than depending on the user's ambient temporary-directory setup.
    let temp = library.join(".narou/tmp");
    std::fs::create_dir_all(&temp).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let log_path = library.join("server-test.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_narou_rs"))
        .args(["web", "--port", &port.to_string(), "--no-browser"])
        .current_dir(library).env("HOME", library).env("USERPROFILE", library)
        .env("TMP", &temp).env("TEMP", &temp)
        .env_remove("NAROU_RS_LEGACY_YAML").env_remove("NAROU_RS_MIRROR_FILES")
        .stdin(Stdio::null()).stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap();
    let mut server = Server { child, base: format!("http://127.0.0.1:{port}"), token: String::new() };
    let client = Client::builder().timeout(Duration::from_secs(2)).build().unwrap();
    for _ in 0..100 {
        if client.get(&server.base).send().is_ok() {
            let pid = std::fs::read_to_string(library.join(".narou/server.pid")).unwrap();
            server.token = pid.split_whitespace().nth(1).unwrap().to_string();
            return server;
        }
        assert!(server.child.try_wait().unwrap().is_none(), "{}", std::fs::read_to_string(&log_path).unwrap());
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("native web did not start: {}", std::fs::read_to_string(log_path).unwrap());
}

fn authenticated(client: &Client, server: &Server, method: reqwest::Method, path: &str, secret: Option<&str>) -> RequestBuilder {
    let request = client.request(method, format!("{}{path}", server.base))
        .header("x-narou-internal-token", &server.token);
    match secret { Some(secret) => request.header("x-narou-transfer-secret", secret), None => request }
}

fn with_json(request: RequestBuilder, body: &Value) -> RequestBuilder {
    request.header("content-type", "application/json").body(serde_json::to_vec(body).unwrap())
}
fn decode(response: Response) -> Value {
    let status = response.status();
    let text = response.text().unwrap();
    assert!(status.is_success(), "HTTP {status}: {text}");
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{error}: {text}"))
}
fn get(client: &Client, server: &Server, path: &str, secret: Option<&str>) -> Value {
    decode(authenticated(client, server, reqwest::Method::GET, path, secret).send().unwrap())
}
fn revision(status: &Value) -> &str { status["revision"].as_str().unwrap() }
fn session_id(status: &Value) -> &str { status["session_id"].as_str().unwrap() }
fn control(client: &Client, server: &Server, direction: &str, secret: &str, status: &mut Value, action: &str, mut body: Value) {
    let operation = uuid::Uuid::new_v4().to_string();
    body["operation_id"] = json!(operation);
    let path = format!("/api/transfer/v2/{direction}/{}/{action}", session_id(status));
    let etag = format!("\"{}\"", revision(status));
    let send = || with_json(authenticated(client, server, reqwest::Method::POST, &path, Some(secret))
        .header("if-match", &etag), &body).send().unwrap();
    let result = decode(send());
    let replay = decode(send());
    assert_eq!(result, replay, "control ACK replay must recover the same durable receipt");
    *status = result;
}
fn advance(client: &Client, server: &Server, direction: &str, secret: &str, status: &mut Value) {
    for _ in 0..30_000 {
        if !matches!(status["next_action"].as_str(), Some("advance" | "finalize_file")) { return; }
        let body = json!({"operation_id":status["operation_id"]});
        let path = format!("/api/transfer/v2/{direction}/{}/advance", session_id(status));
        let response = with_json(authenticated(client, server, reqwest::Method::POST, &path, Some(secret)), &body).send().unwrap();
        if response.status().as_u16() == 423 || response.status().as_u16() == 503 {
            let retry = response.headers().get("retry-after").and_then(|v|v.to_str().ok()).and_then(|v|v.parse::<u64>().ok()).unwrap_or(1);
            let value: Value = serde_json::from_str(&response.text().unwrap()).unwrap();
            assert_eq!(value["error"]["retryable"], true, "{value}");
            std::thread::sleep(Duration::from_secs(retry.min(2)));
            continue;
        }
        let result = decode(response);
        assert!(revision(&result).parse::<u64>().unwrap() > revision(status).parse::<u64>().unwrap());
        let replay = decode(with_json(authenticated(client, server, reqwest::Method::POST, &path, Some(secret)), &body).send().unwrap());
        assert_eq!(result, replay, "one advance identity must not execute a second unit");
        assert_ne!(result["state"], "failed", "{result}");
        *status = result;
    }
    panic!("bounded transfer did not terminate: {status}");
}
fn page_items(client: &Client, server: &Server, path: &str, secret: &str) -> Vec<Value> {
    let mut cursor: Option<String> = None;
    let mut all = Vec::new();
    loop {
        let mut url = reqwest::Url::parse(&format!("{}{path}", server.base)).unwrap();
        if let Some(cursor) = &cursor { url.query_pairs_mut().append_pair("cursor", cursor); }
        let request = authenticated(client, server, reqwest::Method::GET, &url[url::Position::BeforePath..], Some(secret));
        let response = request.send().unwrap();
        let status = response.status();
        let bytes = response.bytes().unwrap();
        assert!(status.is_success(), "{}", String::from_utf8_lossy(&bytes));
        assert!(bytes.len() <= protocol::PAGE_MAX_BYTES);
        let page: Value = serde_json::from_slice(&bytes).unwrap();
        let items = page["items"].as_array().unwrap();
        assert!(items.len() <= protocol::PAGE_MAX_ITEMS);
        all.extend(items.iter().cloned());
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() { return all; }
    }
}

fn test_root() -> tempfile::TempDir {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-libraries");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new().prefix("transfer-web-").tempdir_in(base).unwrap()
}

fn seed_novels(library: &Path, count: i64) -> Vec<i64> {
    let mut records = serde_yaml::Mapping::new();
    let mut ids = Vec::new();
    for index in 0..count {
        let id = index + 100;
        ids.push(id);
        let title = format!("作品 {index}");
        let url = format!("https://example.test/bulk/{index}/");
        let record = json!({"id":id,"author":"作者","title":title,"file_title":format!("bulk-{index}"),
            "toc_url":url,"sitename":"TransferTest","novel_type":1,"end":false,
            "last_update":"2026-11-02T00:00:00Z","use_subdirectory":false,"tags":["検証"],
            "ncode":format!("bulk-{index}"),"domain":"example.test","general_all_no":1,"is_narou":false});
        records.insert(serde_yaml::to_value(id).unwrap(), serde_yaml::to_value(record).unwrap());
        let directory = library.join("小説データ/TransferTest").join(format!("bulk-{index}"));
        std::fs::create_dir_all(directory.join("本文")).unwrap();
        std::fs::write(directory.join("toc.yaml"), format!("title: {title}\nauthor: 作者\ntoc_url: {url}\nsubtitles: []\nnovel_type: 1\n")).unwrap();
        std::fs::write(directory.join("本文/1 話.yaml"), format!("body: 本文 {index}\n")).unwrap();
    }
    let first = library.join("小説データ/TransferTest/bulk-0");
    std::fs::create_dir_all(first.join("raw")).unwrap();
    std::fs::create_dir_all(first.join("本文/cache")).unwrap();
    std::fs::create_dir_all(first.join("挿絵")).unwrap();
    std::fs::write(first.join("raw/download.html"), vec![b'R'; 3 * 1024 * 1024]).unwrap();
    std::fs::write(first.join("本文/cache/old.yaml"), "body: 過去本文\n").unwrap();
    std::fs::write(first.join("setting.ini"), "[General]\nforce-convert=true\n").unwrap();
    std::fs::write(first.join("replace.txt"), "before => after\n").unwrap();
    std::fs::write(first.join("converter.yaml"), "user_setting: keep\n").unwrap();
    std::fs::write(first.join("novel.txt"), "手動で作成した出力。拡張子だけで捨てない。\n").unwrap();
    std::fs::write(first.join("user-notes.epub"), "unknown user file\n").unwrap();
    std::fs::write(first.join("挿絵/unchanged.apng"), [1,2,3,4,5,6,7]).unwrap();
    std::fs::write(first.join(".illustration_cache.yaml"), serde_yaml::to_string(&narou_rs::illustration_store::IllustrationIndex::default()).unwrap()).unwrap();
    std::fs::write(first.join("empty-user-note.txt"), []).unwrap();
    std::fs::write(first.join("db.sqlite"), b"internal runtime state").unwrap();
    std::fs::create_dir_all(library.join(".narou")).unwrap();
    std::fs::write(library.join(".narou/database.yaml"), serde_yaml::to_string(&records).unwrap()).unwrap();
    ids
}

fn create(client:&Client,server:&Server,direction:&str,secret:&str,mut body:Value)->Value{
    let caps=get(client,server,"/api/transfer/v2/capabilities",None);
    assert_eq!(caps["protocol_version"],2);
    assert!(caps["build"]["wasm_build_id"].as_str().is_some_and(|id|id.len()==64),"tests require the actual compiled transfer WASM distribution: {caps}");
    assert_eq!(caps["storage"][direction.trim_end_matches('s')]["allowed"],true,"{caps}");
    body["request_id"]=json!(uuid::Uuid::new_v4().to_string());
    body["creation_ticket"]=caps["creation_ticket"].clone();
    let path=format!("/api/transfer/v2/{direction}");
    let status=decode(with_json(authenticated(client,server,reqwest::Method::POST,&path,Some(secret)), &body).send().unwrap());
    let replay=decode(with_json(authenticated(client,server,reqwest::Method::POST,&path,Some(secret)), &body).send().unwrap());
    assert_eq!(status["session_id"],replay["session_id"]);
    assert_eq!(replay["replayed"],true);
    let conflict=with_json(authenticated(client,server,reqwest::Method::POST,&path,Some(&hex::encode([99u8;32]))), &body).send().unwrap();
    assert_eq!(conflict.status().as_u16(),409);
    status
}

fn export_profile(client:&Client,server:&Server,library:&Path,ids:&[i64],profile:TransferProfile)->(ArchiveManifest,BTreeMap<u32,Vec<u8>>){
    let secret=hex::encode([7u8;32]);
    let mut status=create(client,server,"exports",&secret,json!({"ids":ids,"profile":profile,"extra_roles":[]}));
    let path=format!("/api/transfer/v2/exports/{}/manifest/chunks/0",session_id(&status));
    assert_eq!(authenticated(client,server,reqwest::Method::GET,&path,Some(&secret)).send().unwrap().status().as_u16(),409);
    advance(client,server,"exports",&secret,&mut status);
    assert_eq!(status["state"],"selecting");
    let root=format!("/api/transfer/v2/exports/{}",session_id(&status));
    assert_eq!(authenticated(client,server,reqwest::Method::GET,&root,Some(&hex::encode([9u8;32]))).send().unwrap().status().as_u16(),403);
    assert_eq!(authenticated(client,server,reqwest::Method::GET,&format!("/api/transfer/v2/imports/{}",session_id(&status)),Some(&secret)).send().unwrap().status().as_u16(),404);
    let missing=with_json(authenticated(client,server,reqwest::Method::POST,&format!("{root}/pause"),Some(&secret)), &json!({"operation_id":uuid::Uuid::new_v4().to_string()})).send().unwrap();assert_eq!(missing.status().as_u16(),428);
    let stale=with_json(authenticated(client,server,reqwest::Method::POST,&format!("{root}/pause"),Some(&secret)).header("if-match","\"0\""), &json!({"operation_id":uuid::Uuid::new_v4().to_string()})).send().unwrap();assert_eq!(stale.status().as_u16(),412);let stale:Value=serde_json::from_str(&stale.text().unwrap()).unwrap();assert_eq!(stale["error"]["session_id"],status["session_id"]);assert_eq!(stale["error"]["revision"],status["revision"]);
    let sessions=page_items(client,server,"/api/transfer/v2/sessions",&secret);assert!(sessions.iter().any(|item|item["session_id"]==status["session_id"]));
    control(client,server,"exports",&secret,&mut status,"pause",json!({}));assert_eq!(status["state"],"paused");control(client,server,"exports",&secret,&mut status,"resume",json!({}));assert_eq!(status["state"],"selecting");
    let stage_data=library.join("小説データ/.objects/.narou-transfer/data");
    if stage_data.exists(){for entry in std::fs::read_dir(&stage_data).unwrap(){assert_eq!(entry.unwrap().metadata().unwrap().len(),0,"only empty metadata story stages may exist before selection; no body/raw/image snapshot");}}
    let catalog=page_items(client,server,&format!("/api/transfer/v2/exports/{}/catalog",session_id(&status)),&secret);
    let raw=catalog.iter().find(|r|r["logical_path"]=="raw/download.html").unwrap();
    assert_eq!(raw["role"],"raw");assert_eq!(raw["selected"],profile==TransferProfile::Complete);assert_eq!(raw["runtime"]["default"],false);assert_eq!(raw["complete"]["default"],true);
    assert_eq!(catalog.iter().find(|r|r["logical_path"]=="本文/cache/old.yaml").unwrap()["selected"],profile==TransferProfile::Complete);
    for path in ["setting.ini","replace.txt","converter.yaml",".illustration_cache.yaml","novel.txt","user-notes.epub","挿絵/unchanged.apng"]{assert_eq!(catalog.iter().find(|r|r["logical_path"]==path).unwrap()["selected"],true,"{path}");}
    assert_eq!(catalog.iter().find(|r|r["logical_path"]=="user-notes.epub").unwrap()["role"],"unknown");
    let selected=catalog.iter().map(|r|(
        r["novel_ordinal"].as_u64().unwrap() as u32,r["logical_path"].as_str().unwrap().to_owned(),r["size"].as_str().unwrap().parse().unwrap(),FileRole::parse(r["role"].as_str().unwrap()).unwrap()
    )).collect::<Vec<_>>();
    let digest=TransferService::selection_digest(status["catalog_revision"].as_str().unwrap().parse().unwrap(),profile,&[],&selected);
    control(client,server,"exports",&secret,&mut status,"select",json!({"profile":profile,"extra_roles":[],"selection_digest":digest}));
    advance(client,server,"exports",&secret,&mut status);assert_eq!(status["state"],"ready");
    let mut manifest_bytes=Vec::new();let mut index=0;
    loop{let path=format!("/api/transfer/v2/exports/{}/manifest/chunks/{index}",session_id(&status));let response=authenticated(client,server,reqwest::Method::GET,&path,Some(&secret)).send().unwrap();assert!(response.status().is_success());let last=response.headers()["x-transfer-last"]=="true";let declared=response.headers()["x-transfer-body-bytes"].to_str().unwrap().parse::<usize>().unwrap();let total=response.headers()["x-transfer-manifest-bytes"].to_str().unwrap().parse::<usize>().unwrap();let bytes=response.bytes().unwrap();assert_eq!(bytes.len(),declared,"manifest chunk body must equal x-transfer-body-bytes");assert!(bytes.len() as u64<=protocol::CHUNK_BYTES);manifest_bytes.extend_from_slice(&bytes);if last{assert_eq!(manifest_bytes.len(),total);break;}index+=1;}
    let manifest:ArchiveManifest=serde_json::from_slice(&manifest_bytes).unwrap();assert_eq!(manifest.novels.len(),ids.len());
    assert!(!manifest.novels.iter().flat_map(|n|&n.files).any(|f|f.path=="db.sqlite"));if profile==TransferProfile::Runtime{assert!(!manifest.novels.iter().flat_map(|n|&n.files).any(|f|f.path.starts_with("raw/")||f.path.starts_with("本文/cache/")));}else{assert!(manifest.novels.iter().flat_map(|n|&n.files).any(|f|f.path=="raw/download.html"));}
    let mut payloads=BTreeMap::<u32,Vec<u8>>::new();let mut batch=0;
    loop{let response=authenticated(client,server,reqwest::Method::GET,&format!("/api/transfer/v2/exports/{}/batches/{batch}",session_id(&status)),Some(&secret)).send().unwrap();assert!(response.status().is_success());let last=response.headers()["x-transfer-last"]=="true";let expected=response.headers()["x-transfer-record-count"].to_str().unwrap().parse::<usize>().unwrap();let total=response.headers()["x-transfer-body-bytes"].to_str().unwrap().parse::<usize>().unwrap();let body=response.bytes().unwrap();assert_eq!(body.len(),total);assert!(body.len() as u64<=protocol::BINARY_BODY_MAX_BYTES);let mut decoder=FrameDecoder::new();let frames=decoder.feed(&body).unwrap();assert!(decoder.finish().unwrap().is_empty());assert_eq!(frames.len(),expected);for frame in frames{let bytes=payloads.entry(frame.header.file_id).or_default();assert_eq!(bytes.len() as u64,u64::from(frame.header.chunk_index)*protocol::CHUNK_BYTES);bytes.extend(frame.payload);if frame.header.is_final{let file=manifest.novels.iter().flat_map(|n|&n.files).nth(frame.header.file_id as usize).unwrap();assert_eq!(bytes.len() as u64,file.size.unwrap());assert_eq!(sha256_hex(bytes),file.sha256.as_deref().unwrap());}}if last{break;}batch+=1;}
    assert!(batch<10,"small files must use batches, not one GET per file");
    let direct=authenticated(client,server,reqwest::Method::GET,&format!("/api/transfer/v2/exports/{}/files/0/chunks/0",session_id(&status)),Some(&secret)).send().unwrap();assert!(direct.status().is_success());let declared=direct.headers()["x-transfer-body-bytes"].to_str().unwrap().parse::<usize>().unwrap();let bytes=direct.bytes().unwrap();assert_eq!(bytes.len(),declared,"file chunk body must equal x-transfer-body-bytes");assert_eq!(bytes.as_ref(),payloads[&0].as_slice());
    control(client,server,"exports",&secret,&mut status,"complete",json!({}));advance(client,server,"exports",&secret,&mut status);assert_eq!(status["state"],"released");
    (manifest,payloads)
}

fn text(records:&mut Vec<WireRecord>,texts:&mut u32,novel:u32,field:&str,item:Option<u32>,value:&str){let parts=protocol::text_fragments(value).collect::<Vec<_>>();for(part,value)in parts.iter().enumerate(){records.push(WireRecord::Text(WireText{novel,field:field.into(),item,part:part as u32,last:part+1==parts.len(),value:(*value).to_owned()}));*texts+=1;}}
fn wire_manifest(manifest:&ArchiveManifest,fingerprint:&str,policy:u32,profile:TransferProfile)->Vec<u8>{
    let mut records=Vec::new();let mut texts=0;let mut file_id=0;let mut digest=ProjectionDigest::new(policy,profile,&[],fingerprint).unwrap();
    for(index,n)in manifest.novels.iter().enumerate(){let index=index as u32;let kind=if n.virtual_members.is_some(){NovelKind::VirtualParent}else{NovelKind::Normal};digest.novel(index);records.push(WireRecord::Novel(WireNovel{index,kind,novel_type:n.novel_type,files:n.files.len() as u32}));
        for(field,value)in [("toc_url",Some(n.toc_url.as_str())),("title",Some(n.title.as_str())),("author",Some(n.author.as_str())),("sitename",Some(n.sitename.as_str())),("domain",n.domain.as_deref()),("attribute_tag",n.attribute_tag.as_deref()),("ncode",n.ncode.as_deref()),("subtitle",n.subtitle.as_deref()),("general_lastup",n.general_lastup.as_deref()),("last_update",n.last_update.as_deref())]{if let Some(value)=value{text(&mut records,&mut texts,index,field,None,value);}}
        for(item,value)in n.tags.iter().enumerate(){text(&mut records,&mut texts,index,"tag",Some(item as u32),value);}if let Some(members)=&n.virtual_members{for(item,value)in members.iter().enumerate(){text(&mut records,&mut texts,index,"member",Some(item as u32),value);}}
        for file in &n.files{let size=file.size.unwrap();let sha=file.sha256.clone().unwrap();digest.file(&file.path,size,&sha).unwrap();records.push(WireRecord::File(WireFile{file_id,novel:index,path:file.path.clone(),size,sha256:sha}));file_id+=1;}
    }
    records.insert(0,WireRecord::Header(WireHeader{format_version:manifest.format_version,protocol_version:protocol::PROTOCOL_VERSION,policy_version:policy,profile,extra_roles:vec![],fingerprint:fingerprint.into(),selection_digest:digest.finish(),novels:manifest.novels.len() as u32,files:file_id,texts}));
    records.push(WireRecord::End(WireEnd{novels:manifest.novels.len() as u32,files:file_id,texts}));records.iter().flat_map(|r|protocol::serialize_record(r).unwrap()).collect()
}
fn send_batch(client:&Client,server:&Server,path:&str,secret:&str,body:&[u8],count:usize)->Value{
    decode(authenticated(client,server,reqwest::Method::PUT,path,Some(secret)).header("content-type","application/octet-stream").header("x-transfer-record-count",count.to_string()).header("x-transfer-body-bytes",body.len().to_string()).body(body.to_vec()).send().unwrap())
}
fn frame(file:u32,chunk:u32,bytes:&[u8],is_final:bool,encoder:&mut FrameEncoder,body:&mut Vec<u8>){encoder.encode_into(&BatchHeader{file_id:file,chunk_index:chunk,payload_size:bytes.len() as u64,is_final,sha256:Some(sha256_hex(bytes))},bytes,body).unwrap();}
fn prepare_projected_import(client:&Client,server:&Server,manifest:&ArchiveManifest,fingerprint:&str,payloads:&BTreeMap<u32,Vec<u8>>,overwrite:bool,profile:TransferProfile)->(Value,String){
    let secret=hex::encode([17u8;32]);let caps=get(client,server,"/api/transfer/v2/capabilities",None);let policy=caps["storage"]["policy_version"].as_u64().unwrap() as u32;let wire=wire_manifest(manifest,fingerprint,policy,profile);let hash=sha256_hex(&wire);
    let mut status=create(client,server,"imports",&secret,json!({"source_fingerprint":fingerprint,"profile":profile,"extra_roles":[],"policy_version":policy,"conflict_policy":if overwrite{"overwrite"}else{"skip"},"manifest_size":wire.len().to_string(),"manifest_sha256":hash}));
    let manifest_path=format!("/api/transfer/v2/imports/{}/manifest/chunks/0",session_id(&status));
    let oversized=authenticated(client,server,reqwest::Method::PUT,&manifest_path,Some(&secret)).header("content-type","application/octet-stream").header("x-transfer-record-count","1").header("x-transfer-body-bytes",(protocol::BINARY_BODY_MAX_BYTES+1).to_string()).body(Vec::new()).send().unwrap();assert_eq!(oversized.status().as_u16(),413);
    let encoded=authenticated(client,server,reqwest::Method::PUT,&manifest_path,Some(&secret)).header("content-type","application/octet-stream").header("content-encoding","gzip").body(vec![0;16]).send().unwrap();assert_eq!(encoded.status().as_u16(),400);
    for(index,bytes)in wire.chunks(protocol::CHUNK_BYTES as usize).enumerate(){let mut body=Vec::new();frame(protocol::MANIFEST_FILE_ID,index as u32,bytes,(index+1)*protocol::CHUNK_BYTES as usize>=wire.len(),&mut FrameEncoder::new(),&mut body);let path=format!("/api/transfer/v2/imports/{}/manifest/chunks/{index}",session_id(&status));let result=send_batch(client,server,&path,&secret,&body,1);assert!(result["failure"].is_null(),"{result}");let duplicate=send_batch(client,server,&path,&secret,&body,1);assert_eq!(duplicate["receipts"][0]["outcome"],"duplicate");assert_eq!(result["receipts"][0]["confirmed_bytes"],duplicate["receipts"][0]["confirmed_bytes"]);status=duplicate;}
    control(client,server,"imports",&secret,&mut status,"manifest/seal",json!({"size":wire.len().to_string(),"sha256":hash}));advance(client,server,"imports",&secret,&mut status);assert_eq!(status["state"],"transferring");
    let files=page_items(client,server,&format!("/api/transfer/v2/imports/{}/files",session_id(&status)),&secret);let mut body=Vec::new();let mut encoder=FrameEncoder::new();let mut count=0;
    let send=|body:&mut Vec<u8>,count:&mut usize,status:&mut Value|{if *count==0{return;}let result=send_batch(client,server,&format!("/api/transfer/v2/imports/{}/chunks",session_id(status)),&secret,body,*count);assert!(result["failure"].is_null(),"{result}");assert_eq!(result["receipts"].as_array().unwrap().len(),*count);let replay=send_batch(client,server,&format!("/api/transfer/v2/imports/{}/chunks",session_id(status)),&secret,body,*count);assert!(replay["failure"].is_null(),"{replay}");assert_eq!(replay["confirmed_bytes"],result["confirmed_bytes"]);for (original,duplicate) in result["receipts"].as_array().unwrap().iter().zip(replay["receipts"].as_array().unwrap()){assert_eq!(duplicate["outcome"],"duplicate");assert_eq!(original["confirmed_bytes"],duplicate["confirmed_bytes"]);assert_eq!(original["confirmed_prefix_sha256"],duplicate["confirmed_prefix_sha256"]);}*status=replay;body.clear();*count=0;};
    for file in files.iter().filter(|f|f["upload_required"]==true){let id=file["file_id"].as_u64().unwrap() as u32;assert_eq!(file["manifest_file_index"],id);let bytes=&payloads[&id];for(index,chunk)in bytes.chunks(protocol::CHUNK_BYTES as usize).enumerate(){if count==protocol::BATCH_MAX_RECORDS||body.len()+chunk.len()+protocol::FRAME_HEADER_MAX_BYTES+4>protocol::BINARY_BODY_MAX_BYTES as usize{send(&mut body,&mut count,&mut status);encoder=FrameEncoder::new();}frame(id,index as u32,chunk,(index+1)*protocol::CHUNK_BYTES as usize>=bytes.len(),&mut encoder,&mut body);count+=1;}if bytes.is_empty(){if count==protocol::BATCH_MAX_RECORDS{send(&mut body,&mut count,&mut status);encoder=FrameEncoder::new();}frame(id,0,&[],true,&mut encoder,&mut body);count+=1;}}
    send(&mut body,&mut count,&mut status);advance(client,server,"imports",&secret,&mut status);
    let plan=page_items(client,server,&format!("/api/transfer/v2/imports/{}/plan",session_id(&status)),&secret);assert_eq!(plan.len(),manifest.novels.len());
    for entry in plan {
        let novel=&manifest.novels[entry["novel_ordinal"].as_u64().unwrap() as usize];
        let display_title:String=novel.title.chars().take(narou_rs::application::transfer::DISPLAY_TITLE_CHARS).collect();
        assert_eq!(entry["title"],display_title);
    }
    control(client,server,"imports",&secret,&mut status,"ready",json!({}));
    (status,secret)
}

fn import_projected(client:&Client,server:&Server,manifest:&ArchiveManifest,fingerprint:&str,payloads:&BTreeMap<u32,Vec<u8>>,overwrite:bool,profile:TransferProfile)->Value{
    let (mut status,secret)=prepare_projected_import(client,server,manifest,fingerprint,payloads,overwrite,profile);
    let plan_revision=status["plan_revision"].clone();control(client,server,"imports",&secret,&mut status,"publish",json!({"plan_revision":plan_revision}));advance(client,server,"imports",&secret,&mut status);assert_eq!(status["state"],"completed","{status}");
    let receipts=page_items(client,server,&format!("/api/transfer/v2/imports/{}/plan",session_id(&status)),&secret);assert!(receipts.iter().all(|p|!p["receipt"].is_null()),"{receipts:?}");status
}

#[test]
fn native_web_v2_roundtrips_158_novels_with_durable_receipts_and_runtime_projection(){
    let temp=test_root();let source:PathBuf=temp.path().join("source");let target:PathBuf=temp.path().join("target");let ids=seed_novels(&source,158);let source_server=start(&source);let target_server=start(&target);let client=Client::builder().timeout(Duration::from_secs(60)).build().unwrap();
    assert_eq!(authenticated(&client,&source_server,reqwest::Method::GET,"/api/transfer/export?ids=100",None).send().unwrap().status().as_u16(),404);
    assert_eq!(authenticated(&client,&target_server,reqwest::Method::POST,"/api/transfer/import",None).header("content-type","application/zip").body(vec![0x50,0x4b,3,4]).send().unwrap().status().as_u16(),404);
    let(mut manifest,payloads)=export_profile(&client,&source_server,&source,&ids,TransferProfile::Runtime);
    // The fixture uses the allowed local CLI/core codec, never a server ZIP route.
    let mut ordinal=0;for novel in &mut manifest.novels{for file in &mut novel.files{file.bytes=payloads[&ordinal].clone();ordinal+=1;}}
    let zip=narou_rs::transfer::build_archive(&manifest).unwrap();let parsed=narou_rs::transfer::parse_archive(&zip).unwrap();assert_eq!(parsed.novels.len(),158);
    let fingerprint=narou_rs::transfer::source_fingerprint(&parsed.manifest).unwrap();let mut ordered=BTreeMap::new();let mut ordinal=0;for(index,novel)in parsed.manifest.novels.iter().enumerate(){for file in &novel.files{let bytes=&parsed.novels[index].files.iter().find(|(path,_)|path==&file.path).unwrap().1;ordered.insert(ordinal,bytes.clone());ordinal+=1;}}
    let imported=import_projected(&client,&target_server,&parsed.manifest,&fingerprint,&ordered,false,TransferProfile::Runtime);assert_eq!(imported["published_novels"],158);
    let records: BTreeMap<i64,narou_rs::db::NovelRecord>=serde_yaml::from_str(&std::fs::read_to_string(target.join(".narou/database.yaml")).unwrap()).unwrap();assert_eq!(records.len(),158);
    for record in records.values(){let directory=narou_rs::db::novel_dir_for_record(&target.join("小説データ"),record);let index=record.toc_url.trim_end_matches('/').rsplit('/').next().unwrap();assert_eq!(std::fs::read(directory.join("本文/1 話.yaml")).unwrap(),format!("body: 本文 {index}\n").as_bytes());assert!(!directory.join("raw/download.html").exists());}
    let skipped=import_projected(&client,&target_server,&parsed.manifest,&fingerprint,&ordered,false,TransferProfile::Runtime);assert_eq!(skipped["published_novels"],0);
    let first=&parsed.manifest.novels[0];let record=records.values().find(|r|r.toc_url==first.toc_url).unwrap();let directory=narou_rs::db::novel_dir_for_record(&target.join("小説データ"),record);std::fs::write(directory.join("keep-user-note.txt"),b"keep exact extra bytes").unwrap();
    let one=ArchiveManifest{novels:vec![first.clone()],..parsed.manifest.clone()};let mut one_payloads=BTreeMap::new();for(index,file)in first.files.iter().enumerate(){one_payloads.insert(index as u32,parsed.novels[0].files.iter().find(|(path,_)|path==&file.path).unwrap().1.clone());}
    let overwritten=import_projected(&client,&target_server,&one,&fingerprint,&one_payloads,true,TransferProfile::Runtime);assert_eq!(overwritten["published_novels"],1);assert_eq!(std::fs::read(directory.join("keep-user-note.txt")).unwrap(),b"keep exact extra bytes");
    let (complete,complete_bytes)=export_profile(&client,&source_server,&source,&ids[..1],TransferProfile::Complete);let complete_fingerprint=narou_rs::transfer::source_fingerprint(&complete).unwrap();let imported=import_projected(&client,&target_server,&complete,&complete_fingerprint,&complete_bytes,true,TransferProfile::Complete);assert_eq!(imported["published_novels"],1);assert_eq!(std::fs::read(directory.join("raw/download.html")).unwrap(),vec![b'R';3*1024*1024]);assert_eq!(std::fs::read(directory.join("本文/cache/old.yaml")).unwrap(),"body: 過去本文\n".as_bytes());
    // The durable commit mark IS the immutable publication receipt: every
    // finished publication must carry the receipt bytes verbatim, match the
    // immutable expected receipt and resolve its metadata journal as committed.
    let mut marks=0;for session in std::fs::read_dir(target.join(".narou/transfer/publications")).unwrap(){for publication in std::fs::read_dir(session.unwrap().path()).unwrap(){let publication=publication.unwrap().path();if let Ok(committed)=std::fs::read(publication.join("committed")){assert_eq!(committed,std::fs::read(publication.join("expected-receipt")).unwrap(),"durable commit mark must be the immutable receipt");serde_json::from_slice::<Value>(&committed).unwrap();assert_eq!(std::fs::read(publication.join("metadata-resolved")).unwrap(),b"committed");marks+=1;}}}assert!(marks>=3,"{marks}");
    for admin in [false,true]{let secret=hex::encode([43u8;32]);let mut cancelled=create(&client,&source_server,"exports",&secret,json!({"ids":[ids[0]],"profile":"runtime","extra_roles":[]}));let id=session_id(&cancelled).to_owned();let path=format!("/api/transfer/v2/{}/{id}?operation_id={}",if admin{"sessions"}else{"exports"},uuid::Uuid::new_v4());let etag=format!("\"{}\"",revision(&cancelled));let send=||authenticated(&client,&source_server,reqwest::Method::DELETE,&path,(!admin).then_some(secret.as_str())).header("if-match",&etag).send().unwrap();let result=decode(send());assert_eq!(result,decode(send()));cancelled=result;advance(&client,&source_server,"exports",&secret,&mut cancelled);assert_eq!(cancelled["state"],"cancelled");}

}

#[test]
fn native_web_v2_roundtrips_six_mebibyte_title_without_prefix_replay(){
    // Exercise the wire scalar, not an unrelated startup rewrite for legacy
    // records that have never received their immutable attribute classification.
    let temp=test_root();let source=temp.path().join("source");let target=temp.path().join("target");let ids=seed_novels(&source,158);let path=source.join(".narou/database.yaml");let mut records:BTreeMap<i64,Value>=serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();for record in records.values_mut(){record["attribute_tag"]=json!("小説");}records.get_mut(&ids[0]).unwrap()["title"]=Value::String("\0".repeat(6*1024*1024));std::fs::write(&path,serde_yaml::to_string(&records).unwrap()).unwrap();drop(records);
    let source_server=start(&source);let target_server=start(&target);let client=Client::builder().timeout(Duration::from_secs(60)).build().unwrap();let(manifest,payloads)=export_profile(&client,&source_server,&source,&ids[..1],TransferProfile::Runtime);let title=&manifest.novels[0].title;assert_eq!(title.len(),6*1024*1024);assert!(title.bytes().all(|byte|byte==0));let fingerprint=narou_rs::transfer::source_fingerprint(&manifest).unwrap();let imported=import_projected(&client,&target_server,&manifest,&fingerprint,&payloads,false,TransferProfile::Runtime);assert_eq!(imported["published_novels"],1);
    let records:BTreeMap<i64,narou_rs::db::NovelRecord>=serde_yaml::from_str(&std::fs::read_to_string(target.join(".narou/database.yaml")).unwrap()).unwrap();assert_eq!(records.len(),1);assert_eq!(&records.values().next().unwrap().title,title);
}

#[test]
fn native_cli_and_web_v2_reject_selected_parent_with_unselected_child_before_catalog_or_payload(){
    let temp=test_root();for use_subdirectory in [false,true]{let source=temp.path().join(if use_subdirectory{"physical-flat-source"}else{"logical-parent-source"});let archive_root=source.join("小説データ");let child:narou_rs::db::NovelRecord=serde_json::from_value(json!({"id":1001,"author":"作者","title":"unselected child","file_title":"n1234ab","toc_url":"https://example.test/child/","sitename":"TransferTest","ncode":"n1234ab","novel_type":1,"general_all_no":1,"last_update":"2026-11-15T00:00:00Z","use_subdirectory":true})).unwrap();let child_directory=narou_rs::db::novel_dir_for_record(&archive_root,&child);let parent_directory=child_directory.parent().unwrap().to_path_buf();let mut parent=child.clone();parent.id=1000;parent.title="selected parent".into();parent.file_title=parent_directory.file_name().unwrap().to_string_lossy().into_owned();parent.use_subdirectory=use_subdirectory;parent.toc_url="https://example.test/parent/".into();parent.ncode=Some("parent".into());if !use_subdirectory{assert_eq!(narou_rs::db::novel_dir_for_record(&archive_root,&parent),parent_directory);}assert_ne!(child_directory,parent_directory);
    let child_marker="UNSELECTED_CHILD_MUST_NEVER_ENTER_PARENT_ARCHIVE_6e5740b7";for(record,directory)in [(&parent,&parent_directory),(&child,&child_directory)]{std::fs::create_dir_all(directory.join("本文")).unwrap();std::fs::write(directory.join("toc.yaml"),format!("title: {}\nauthor: 作者\ntoc_url: {}\nsubtitles: []\nnovel_type: 1\n",record.title,record.toc_url)).unwrap();std::fs::write(directory.join("本文/1.yaml"),format!("body: {}\n",if record.id==child.id{child_marker}else{"parent body"})).unwrap();}std::fs::create_dir_all(source.join(".narou")).unwrap();std::fs::write(source.join(".narou/database.yaml"),serde_yaml::to_string(&BTreeMap::from([(parent.id,parent.clone()),(child.id,child.clone())])).unwrap()).unwrap();
    let output=temp.path().join(if use_subdirectory{"physical-flat-parent-only.zip"}else{"logical-parent-only.zip"});let result=Command::new(env!("CARGO_BIN_EXE_narou_rs")).args(["export",&parent.id.to_string(),"--output"]).arg(&output).current_dir(&source).env("HOME",&source).env("USERPROFILE",&source).env_remove("NAROU_RS_LEGACY_YAML").env_remove("NAROU_RS_MIRROR_FILES").stdin(Stdio::null()).output().unwrap();let diagnostic=format!("{}{}",String::from_utf8_lossy(&result.stdout),String::from_utf8_lossy(&result.stderr));assert!(!result.status.success(),"{diagnostic}");assert!(diagnostic.contains("namespace")||diagnostic.contains("overlap")||diagnostic.contains("重な"),"export must fail at real namespace admission: {diagnostic}");assert!(!output.exists(),"a failed export must not publish an archive containing child bytes");
    let server=start(&source);let client=Client::builder().timeout(Duration::from_secs(60)).build().unwrap();let secret=hex::encode([31u8;32]);let mut status=create(&client,&server,"exports",&secret,json!({"ids":[parent.id],"profile":TransferProfile::Runtime,"extra_roles":[]}));let root=format!("/api/transfer/v2/exports/{}",session_id(&status));let mut rejected=false;for _ in 0..32{let response=with_json(authenticated(&client,&server,reqwest::Method::POST,&format!("{root}/advance"),Some(&secret)).header("if-match",revision(&status)), &json!({"operation_id":status["operation_id"]})).send().unwrap();let successful=response.status().is_success();let text=response.text().unwrap();if text.contains("ambiguous_source_namespace"){rejected=true;break;}assert!(successful,"unexpected source failure: {text}");status=serde_json::from_str(&text).unwrap();assert_eq!(status["next_action"],"advance","source must not become selectable before namespace admission: {status}");}assert!(rejected,"selected parent must reject the real unselected child namespace");assert!(page_items(&client,&server,&format!("{root}/catalog"),&secret).is_empty(),"no child paths may enter the parent catalog");let response=authenticated(&client,&server,reqwest::Method::GET,&format!("{root}/manifest/chunks/0"),Some(&secret)).send().unwrap();assert!(!response.status().is_success());assert!(!response.text().unwrap().contains(child_marker));assert!(std::fs::read_to_string(child_directory.join("本文/1.yaml")).unwrap().contains(child_marker));
    }
}

/// The DB-only backend (`storage-backend=sqlite`, `sqlite.mirror-files=false`)
/// performs the same authenticated v2 import and export without materializing
/// any novel file: canonical bytes live only in `.narou/db.sqlite`, while
/// private object-store control locks may remain on the filesystem. Receipts,
/// metadata-resolution recovery marks and payloads must match the YAML-backend
/// contract byte for byte. The legacy CLI import restriction is unaffected.
#[test]
fn native_web_v2_sqlite_db_only_imports_exports_and_recovers_without_mirror_files(){
    let temp=test_root();let source=temp.path().join("source");let target=temp.path().join("target");let ids=seed_novels(&source,1);
    std::fs::create_dir_all(target.join(".narou")).unwrap();
    std::fs::write(target.join(".narou/storage-backend"),"sqlite\n").unwrap();
    std::fs::write(target.join(".narou/local_setting.yaml"),"sqlite.mirror-files: false\n").unwrap();
    let source_server=start(&source);let target_server=start(&target);let client=Client::builder().timeout(Duration::from_secs(60)).build().unwrap();
    let (manifest,payloads)=export_profile(&client,&source_server,&source,&ids,TransferProfile::Runtime);
    let fingerprint=narou_rs::transfer::source_fingerprint(&manifest).unwrap();
    let imported=import_projected(&client,&target_server,&manifest,&fingerprint,&payloads,false,TransferProfile::Runtime);assert_eq!(imported["published_novels"],1);
    let archive=target.join("小説データ");
    let mut mirrored=Vec::new();let mut stack=vec![archive.clone()];
    while let Some(directory)=stack.pop(){if !directory.is_dir(){continue;}for entry in std::fs::read_dir(&directory).unwrap(){let entry=entry.unwrap();if directory==archive&&entry.file_name()==".objects"{continue;}else if entry.file_type().unwrap().is_dir(){stack.push(entry.path());}else{mirrored.push(entry.path());}}}
    assert!(mirrored.is_empty(),"DB-only import must not materialize novel files: {mirrored:?}");
    let database_connection=narou_rs::native::sqlite::open(&target.join(".narou/db.sqlite")).unwrap();
    let connection=database_connection.lock();
    let novel_id:i64=connection.query_row("SELECT id FROM novels WHERE toc_url=?",[&manifest.novels[0].toc_url],|row|row.get(0)).unwrap();
    let referenced:i64=connection.query_row("SELECT COUNT(*) FROM transfer_object_refs",[],|row|row.get(0)).unwrap();
    assert_eq!(referenced as usize,manifest.novels[0].files.len(),"every published file must reference a stored blob");
    let session=session_id(&imported);let publication_root=target.join(".narou/transfer/publications").join(session).join("0000000000");
    let expected=std::fs::read(publication_root.join("expected-receipt")).unwrap();
    let receipt=serde_json::from_slice::<Value>(&expected).unwrap();assert_eq!(receipt["assigned_id"],novel_id);
    let mark:Vec<u8>=connection.query_row("SELECT value FROM transfer_backend_marks WHERE key=?1",[format!("commit:{session}:0")],|row|row.get(0)).unwrap();
    assert_eq!(mark,expected,"SQL durable mark must be the immutable expected receipt");
    assert_eq!(std::fs::read(publication_root.join("metadata-resolved")).unwrap(),b"committed");
    drop(connection);
    // Export the same record back through v2: DB-only reads are served by the
    // object store and must reproduce the imported manifest and payload bytes.
    let secret=hex::encode([7u8;32]);
    let mut status=create(&client,&target_server,"exports",&secret,json!({"ids":[novel_id],"profile":TransferProfile::Runtime,"extra_roles":[]}));
    advance(&client,&target_server,"exports",&secret,&mut status);assert_eq!(status["state"],"selecting");
    let root=format!("/api/transfer/v2/exports/{}",session_id(&status));
    let catalog=page_items(&client,&target_server,&format!("{root}/catalog"),&secret);
    let selected=catalog.iter().map(|r|(r["novel_ordinal"].as_u64().unwrap() as u32,r["logical_path"].as_str().unwrap().to_owned(),r["size"].as_str().unwrap().parse().unwrap(),FileRole::parse(r["role"].as_str().unwrap()).unwrap())).collect::<Vec<_>>();
    let digest=TransferService::selection_digest(status["catalog_revision"].as_str().unwrap().parse().unwrap(),TransferProfile::Runtime,&[],&selected);
    control(&client,&target_server,"exports",&secret,&mut status,"select",json!({"profile":TransferProfile::Runtime,"extra_roles":[],"selection_digest":digest}));
    advance(&client,&target_server,"exports",&secret,&mut status);assert_eq!(status["state"],"ready");
    let mut bytes=Vec::new();let mut index=0u32;
    loop{let response=authenticated(&client,&target_server,reqwest::Method::GET,&format!("{root}/manifest/chunks/{index}"),Some(&secret)).send().unwrap();assert!(response.status().is_success());let last=response.headers()["x-transfer-last"]=="true";let declared=response.headers()["x-transfer-body-bytes"].to_str().unwrap().parse::<usize>().unwrap();let chunk=response.bytes().unwrap();assert_eq!(chunk.len(),declared,"manifest chunk body must equal x-transfer-body-bytes");bytes.extend_from_slice(&chunk);if last{break;}index+=1;}
    let exported:ArchiveManifest=serde_json::from_slice(&bytes).unwrap();
    assert_eq!(exported.novels.len(),1);let exported_files:Vec<_>=exported.novels[0].files.iter().map(|file|(file.path.clone(),file.size,file.sha256.clone())).collect();
    assert_eq!(exported_files,manifest.novels[0].files.iter().map(|file|(file.path.clone(),file.size,file.sha256.clone())).collect::<Vec<_>>(),"DB-only round trip must reproduce manifest identity");
    let mut re_payloads=BTreeMap::<u32,Vec<u8>>::new();let mut batch=0;
    loop{let response=authenticated(&client,&target_server,reqwest::Method::GET,&format!("{root}/batches/{batch}"),Some(&secret)).send().unwrap();assert!(response.status().is_success());let last=response.headers()["x-transfer-last"]=="true";let total=response.headers()["x-transfer-body-bytes"].to_str().unwrap().parse::<usize>().unwrap();let body=response.bytes().unwrap();assert_eq!(body.len(),total);let mut decoder=FrameDecoder::new();let frames=decoder.feed(&body).unwrap();assert!(decoder.finish().unwrap().is_empty());for frame in frames{let bytes=re_payloads.entry(frame.header.file_id).or_default();bytes.extend(frame.payload);}if last{break;}batch+=1;}
    for(index,payload)in payloads{assert_eq!(re_payloads.remove(&index).as_deref(),Some(payload.as_slice()),"file {index} payload must round trip byte-exact through DB-only");}
    control(&client,&target_server,"exports",&secret,&mut status,"complete",json!({}));advance(&client,&target_server,"exports",&secret,&mut status);assert_eq!(status["state"],"released");
}

/// A configured SQLite library must refuse transfer when its authority cannot
/// open, rather than upload on a YAML fallback and fail during publication.
#[test]
fn native_web_v2_refuses_transfer_when_configured_sqlite_is_unavailable(){
    let temp=test_root();let library:PathBuf=temp.path().join("broken");
    std::fs::create_dir_all(library.join(".narou")).unwrap();
    std::fs::write(library.join(".narou/storage-backend"),"sqlite\n").unwrap();
    std::fs::write(library.join(".narou/local_setting.yaml"),"sqlite.mirror-files: false\n").unwrap();
    // An existing but unopenable database: `configure` fails on every
    // `active_for` attempt, deterministically (no environment games).
    std::fs::write(library.join(".narou/db.sqlite"),b"not a sqlite database").unwrap();
    let server=start(&library);let client=Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    let caps=get(&client,&server,"/api/transfer/v2/capabilities",None);
    assert_eq!(caps["storage"]["methods"],json!(["sqlite"]),"{caps}");
    for direction in ["import","export"]{
        assert_eq!(caps["storage"][direction]["allowed"],false,"{caps}");
        assert!(caps["storage"][direction]["reason"].as_str().is_some_and(|r|!r.is_empty()),"{caps}");
    }
    let secret=hex::encode([23u8;32]);
    let body=json!({"request_id":uuid::Uuid::new_v4().to_string(),"creation_ticket":caps["creation_ticket"].clone(),
        "source_fingerprint":"a".repeat(64),"profile":TransferProfile::Runtime,"extra_roles":[],
        "policy_version":caps["storage"]["policy_version"].as_u64().unwrap(),"conflict_policy":"skip",
        "manifest_size":"1","manifest_sha256":"b".repeat(64)});
    let response=with_json(authenticated(&client,&server,reqwest::Method::POST,"/api/transfer/v2/imports",Some(&secret)),&body).send().unwrap();
    let status=response.status();
    let error:Value=serde_json::from_str(&response.text().unwrap()).unwrap();
    assert_eq!(status.as_u16(),501,"{error}");
    assert_eq!(error["error"]["code"],"import_unsupported","{error}");
    assert_eq!(error["error"]["retryable"],false,"{error}");
}

#[test]
fn native_web_v2_client_delete_cancels_with_query_operation_and_replays_receipt(){
    let temp=test_root();let client=Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    for sqlite in [false,true]{
        let library=temp.path().join(if sqlite{"sqlite"}else{"yaml"});
        std::fs::create_dir_all(library.join(".narou")).unwrap();
        if sqlite{std::fs::write(library.join(".narou/storage-backend"),"sqlite\n").unwrap();}
        let server=start(&library);let caps=get(&client,&server,"/api/transfer/v2/capabilities",None);
        assert_eq!(caps["storage"]["import"]["allowed"],true,"{caps}");
        let secret=hex::encode([29u8;32]);
        for admin in [false,true]{
            let body=json!({"request_id":uuid::Uuid::new_v4().to_string(),"creation_ticket":caps["creation_ticket"].clone(),
                "source_fingerprint":"c".repeat(64),"profile":TransferProfile::Runtime,"extra_roles":[],
                "policy_version":caps["storage"]["policy_version"].as_u64().unwrap(),"conflict_policy":"skip",
                "manifest_size":"1","manifest_sha256":"d".repeat(64)});
            let status=decode(with_json(authenticated(&client,&server,reqwest::Method::POST,"/api/transfer/v2/imports",Some(&secret)),&body).send().unwrap());
            let path=format!("/api/transfer/v2/{}/{}?operation_id={}",if admin{"sessions"}else{"imports"},session_id(&status),uuid::Uuid::new_v4());
            let etag=format!("\"{}\"",revision(&status));
            let send=||authenticated(&client,&server,reqwest::Method::DELETE,&path,if admin{None}else{Some(&secret)}).header("if-match",&etag).send().unwrap();
            let mut cancelled=decode(send());
            assert_eq!(cancelled,decode(send()),"a lost DELETE ACK must replay its immutable receipt");
            advance(&client,&server,"imports",&secret,&mut cancelled);
            assert_eq!(cancelled["state"],"cancelled","{cancelled}");
        }
    }
}

#[test]
fn native_web_v2_cancels_a_closed_publication_without_staged_record_metadata() {
    assert_metadata_loss_releases_publication_gate(true);
}

#[test]
fn native_web_v2_rolls_back_when_staged_record_metadata_disappears_before_commit() {
    assert_metadata_loss_releases_publication_gate(false);
}

fn assert_metadata_loss_releases_publication_gate(cancel:bool) {
    let temp=test_root();
    let source=temp.path().join("source");
    let target=temp.path().join("target");
    let ids=seed_novels(&source,1);
    std::fs::create_dir_all(target.join(".narou")).unwrap();
    std::fs::write(target.join(".narou/storage-backend"),"sqlite\n").unwrap();
    let source_server=start(&source);
    let target_server=start(&target);
    let client=Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    let profile=TransferProfile::Runtime;
    let (manifest,payloads)=export_profile(&client,&source_server,&source,&ids,profile);
    let fingerprint=narou_rs::transfer::source_fingerprint(&manifest).unwrap();
    let (mut status,secret)=prepare_projected_import(&client,&target_server,&manifest,&fingerprint,&payloads,false,profile);
    let plan_revision=status["plan_revision"].clone();
    control(&client,&target_server,"imports",&secret,&mut status,"publish",json!({"plan_revision":plan_revision}));
    let id=session_id(&status).to_owned();
    let path=format!("/api/transfer/v2/imports/{id}");

    let db=rusqlite::Connection::open(target.join(".narou/db.sqlite")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    let read_publication_state=||db.query_row(
        "SELECT json_extract(value,'$.state') FROM transfer_native_rows WHERE kind='publication' AND owner=?",
        [&id], |row|row.get::<_,String>(0),
    ).unwrap();
    let stopped_at=if cancel{"draining"}else{"committing"};
    for _ in 0..64 {
        if read_publication_state()==stopped_at {break;}
        status=decode(with_json(authenticated(&client,&target_server,reqwest::Method::POST,&format!("{path}/advance"),Some(&secret)),
            &json!({"operation_id":status["operation_id"]})).send().unwrap());
    }
    assert_eq!(read_publication_state(),stopped_at);
    let prefix:String=db.query_row(
        "SELECT c.prefix FROM native_transfer_target_claims c JOIN novel_access_gates g ON g.prefix=c.prefix
         WHERE c.session_id=? AND g.owner=c.owner AND g.closed=1",
        [&id], |row|row.get(0),
    ).expect("publication must hold a real closed gate before metadata loss");
    assert_eq!(db.execute(
        "DELETE FROM transfer_native_rows WHERE kind='blob' AND owner LIKE ? AND key='metadata:0:raw:2:0'",
        [format!("{id}:%")],
    ).unwrap(),1,"remove only the staged author fragment, not the durable publication identity");

    if cancel {
        status=decode(authenticated(&client,&target_server,reqwest::Method::DELETE,
            &format!("{path}?operation_id={}",uuid::Uuid::new_v4()),Some(&secret))
            .header("if-match",format!("\"{}\"",revision(&status))).send().unwrap());
    }
    advance(&client,&target_server,"imports",&secret,&mut status);
    assert_eq!(status["state"],if cancel{"cancelled"}else{"completed_with_errors"});
    assert_eq!(status["published_novels"],0);
    let plan=page_items(&client,&target_server,&format!("{path}/plan"),&secret);
    assert_eq!(plan.len(),1);
    assert_eq!(plan[0]["action"],"create");
    let receipt=&plan[0]["receipt"];
    assert_eq!(receipt["outcome"],"rolled_back");
    assert_eq!(receipt["files_installed"],0);
    assert!(receipt["assigned_id"].is_null());
    if cancel {
        assert!(receipt["error"].is_null());
    } else {
        assert_eq!(receipt["error"],"metadata_missing: metadata fragment missing");
    }
    let closed:i64=db.query_row("SELECT COUNT(*) FROM novel_access_gates WHERE prefix=? AND closed=1",[&prefix],|row|row.get(0)).unwrap();
    let claims:i64=db.query_row("SELECT COUNT(*) FROM native_transfer_target_claims WHERE session_id=?",[&id],|row|row.get(0)).unwrap();
    let novels:i64=db.query_row("SELECT COUNT(*) FROM novels",[],|row|row.get(0)).unwrap();
    let installed:i64=db.query_row(
        "SELECT COUNT(*) FROM transfer_object_refs WHERE substr(object_key,1,length(?1)+1)=?1||'/'",
        [&prefix],|row|row.get(0),
    ).unwrap();
    assert_eq!((closed,claims,novels,installed),(0,0,0,0));
}


/// Kept alive only while the real-browser CI controller owns the stop file.
/// The normal HTTP tests never start this fixture.
#[test]
#[ignore = "run by scripts/test-transfer-browser.py with a real Firefox"]
fn native_transfer_browser_fixture() {
    use std::io::Write;
    use sha2::{Digest, Sha256};

    let ready = PathBuf::from(std::env::var_os("NAROU_TRANSFER_BROWSER_READY").expect("browser controller ready path"));
    let root = test_root();
    let source = root.path().join("source");
    seed_novels(&source, 168);
    let first = source.join("小説データ/TransferTest/bulk-0");
    let large_size = 17 * 1024 * 1024 + 19;
    let mut large = std::fs::File::create(first.join("client-note.bin")).unwrap();
    let mut hash = Sha256::new();
    let mut state = 0x7155_029b_582c_4e37u64;
    let mut chunk = [0u8; 64 * 1024];
    let mut remaining = large_size;
    while remaining != 0 {
        for bytes in chunk.as_chunks_mut::<8>().0 {
            state ^= state << 13; state ^= state >> 7; state ^= state << 17;
            bytes.copy_from_slice(&state.to_le_bytes());
        }
        let count = remaining.min(chunk.len());
        large.write_all(&chunk[..count]).unwrap();
        hash.update(&chunk[..count]);
        remaining -= count;
    }
    drop(large);
    let large_sha = hex::encode(hash.finalize());
    let zero_size = 1024 * 1024 + 17;
    std::fs::File::create(first.join("zero-note.bin")).unwrap().set_len(zero_size).unwrap();

    let libraries = [source, root.path().join("yaml"), root.path().join("sqlite")];
    std::fs::create_dir_all(libraries[2].join(".narou")).unwrap();
    std::fs::write(libraries[2].join(".narou/storage-backend"), "sqlite\n").unwrap();
    std::fs::write(libraries[2].join(".narou/local_setting.yaml"), "sqlite.mirror-files: false\n").unwrap();
    // The real gateway preserves the browser's public Host/Origin, whose port
    // differs from the listener. Keep Host allowlists and UI cookie auth active.
    for library in &libraries {
        // These are already-initialized libraries, not a pre-0.4.0 upgrade.
        // Leave the feature tour unread, but don't queue the separate backup offer.
        std::fs::create_dir_all(library.join(".narou")).unwrap();
        std::fs::write(library.join(".narou/last-run-version"), "0.4.0\n").unwrap();
        std::fs::create_dir_all(library.join(".narousetting")).unwrap();
        std::fs::write(library.join(".narousetting/global_setting.yaml"),
            "server-ws-port: 0\nserver-reverse-proxy.enable: true\n").unwrap();
    }
    let servers = libraries.iter().map(|library| start(library)).collect::<Vec<_>>();
    let origins = servers.iter().zip(&libraries).map(|(server, library)| {
        json!({"base": server.base, "library": library,
            "ui_token": std::fs::read_to_string(library.join(".narou/ui_token")).unwrap().trim()})
    }).collect::<Vec<_>>();
    let stop = root.path().join("stop");
    let mut descriptor = tempfile::NamedTempFile::new_in(ready.parent().unwrap()).unwrap();
    serde_json::to_writer(&mut descriptor, &json!({
        "origins": origins, "stop": stop, "count": 168,
        "large_size": large_size, "large_sha256": large_sha, "zero_size": zero_size,
    })).unwrap();
    descriptor.flush().unwrap();
    descriptor.persist_noclobber(&ready).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20 * 60);
    while !stop.exists() {
        assert!(std::time::Instant::now() < deadline, "real-browser controller exceeded fixture lifetime");
        std::thread::sleep(Duration::from_millis(100));
    }
    // Server guards close every owned process before the TempDir is removed.
    drop(servers);
}
