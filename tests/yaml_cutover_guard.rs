//! SQLite→YAML in-place cutover regressions.
//!
//! The guarded finalization must preserve imported immutable canonical bytes
//! (`transfer_object_refs`) and their remote/tombstone semantics through a
//! real mode switch and a reopen, and a write committed inside the cutover
//! window must abort the flip instead of being silently lost.
//!
//! SQLite mode is process-wide here (the cwd-based library lookup, the
//! `db.sqlite` handle cache and the `Database` slot all belong to the
//! process), so this file keeps its tests serialized and points the cwd at an
//! isolated `target/test-libraries` directory first.

use std::future::Future;
use std::path::{Path, PathBuf};

use narou_rs::native::object_store::NativeObjectStore;
use narou_rs::native::sqlite::object_store::SqliteObjectStore;
use narou_rs::native::sqlite::state::{self, StateDb, StorageMode};
use narou_rs::platform::{AssetStore, AssetStream, ObjectKey, ObjectStore};

/// One lock for the whole binary: the library root is the process cwd.
static LIBRARY_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Restores cwd and the Database slot when the test ends.
struct LibraryGuard {
    cwd: PathBuf,
    database: Option<narou_rs::db::Database>,
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl LibraryGuard {
    fn enter() -> Self {
        let libraries =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-libraries");
        std::fs::create_dir_all(&libraries).unwrap();
        let temp = tempfile::Builder::new()
            .prefix("yaml-cutover-")
            .tempdir_in(libraries)
            .unwrap();
        let root = temp.path().to_path_buf();
        std::fs::create_dir_all(root.join(".narousetting")).unwrap();
        let guard = Self {
            cwd: std::env::current_dir().expect("cwd is readable"),
            database: narou_rs::db::DATABASE.lock().take(),
            _temp: temp,
            root,
        };
        std::env::set_current_dir(&guard.root).expect("temp library is reachable");
        guard
    }
}

impl Drop for LibraryGuard {
    fn drop(&mut self) {
        *narou_rs::db::DATABASE.lock() = self.database.take();
        let _ = std::env::set_current_dir(&self.cwd);
    }
}

/// SQLite mode with the mirror disabled: every byte lives in `db.sqlite`.
fn db_only_store(root: &Path) -> (StateDb, SqliteObjectStore) {
    let narou_dir = root.join(".narou");
    state::write_mode(&narou_dir, StorageMode::Sqlite).unwrap();
    narou_rs::db::init_database().unwrap();

    let state = state::active_for(&narou_dir).expect("sqlite mode is active");
    let mirror =
        NativeObjectStore::from_root(root.join(narou_rs::downloader::ARCHIVE_ROOT_DIR)).unwrap();
    let store = SqliteObjectStore::with_mirror_files(state.conn_ref().clone(), mirror, false);
    (state, store)
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    futures::executor::block_on(future)
}

/// Deterministic >16 MiB opaque payload (nothing about it is compressible or
/// known to the object store).
fn opaque_bytes(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = 0x9e3779b97f4a7c15u64;
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

fn byte_stream(bytes: Vec<u8>) -> AssetStream {
    Box::pin(futures::stream::unfold((bytes, 0usize), |(bytes, offset)| async move {
        if offset == bytes.len() { return None; }
        let end = (offset + 64 * 1024).min(bytes.len());
        let chunk = bytes[offset..end].to_vec();
        Some((Ok(chunk), (bytes, end)))
    }))
}

/// Register `blob` as the immutable canonical object behind `canonical`,
/// exactly like a completed remote reference publication leaves it.
fn promote_reference(state: &StateDb, canonical: &str, blob: &str) {
    let generation: String = {
        let conn = state.conn_ref();
        conn.lock()
            .query_row(
                "SELECT generation FROM objects WHERE object_key=?",
                [blob],
                |row| row.get(0),
            )
            .unwrap()
    };
    let conn = state.conn_ref();
    conn.lock()
        .execute(
            "INSERT INTO transfer_object_refs(object_key,blob_key,size,sha256,backend,updated_at,blob_generation) VALUES(?,?,?,?,'d1',?,?)",
            rusqlite::params![canonical, blob, 0i64, "00", "2026-01-01T00:00:00Z", generation],
        )
        .unwrap();
}

/// The durable FS gate file the SQL→FS adoption writes for `prefix`:
/// `.objects/.narou-transfer/.gate/<sha256(prefix)>` under the archive root.
fn fs_gate_path(archive: &Path, prefix: &str) -> PathBuf {
    use sha2::Digest;
    archive
        .join(".objects/.narou-transfer/.gate")
        .join(hex::encode(sha2::Sha256::digest(prefix.as_bytes())))
}

/// An opaque >16 MiB immutable blob promoted behind a canonical reference
/// survives the in-place switch byte-for-byte, the marker flips only after
/// every byte is on disk, and a reopened handle still sees the canonical
/// reference semantics (SQL residue is never rewritten).
#[test]
fn in_place_switch_preserves_immutable_canonical_bytes() {
    let _guard = LIBRARY_LOCK.lock();
    let library = LibraryGuard::enter();
    let root = library.root.clone();
    let (state, store) = db_only_store(&root);

    let blob_key = ObjectKey::try_new(".narou-transfer/.data/immutable-seed").unwrap();
    let canonical_key = ObjectKey::try_new("novels/site/title/payload.bin").unwrap();
    let payload = opaque_bytes(17 * 1024 * 1024 + 12345);
    let expected_sha = sha256_hex(&payload);
    block_on(async {
        let stream = byte_stream(payload);
        AssetStore::write_stream(&store, &blob_key, stream)
            .await
            .unwrap();
    });
    promote_reference(&state, canonical_key.as_ref(), blob_key.as_ref());
    {
        let conn = state.conn_ref();
        conn.lock()
            .execute(
                "INSERT INTO novel_access_gates(prefix,epoch,closed,owner,updated_at,content_revision,origin_url_fold) VALUES('novels/site/title',5,0,NULL,1,2,'')",
                [],
            )
            .unwrap();
    }

    // Remote-reference markers (.narou-transfer/.refs/<logical>) are the one
    // reserved namespace the FS/YAML backend reads verbatim: a non-hash S3
    // alias (physical is not derivable from the logical key) and a deletion
    // tombstone. They must cross the cutover untouched while sessions,
    // staged blobs and locks stay behind.
    let alias_key =
        ObjectKey::try_new(".narou-transfer/.refs/novels/site/title/挿絵/remote.png").unwrap();
    let remote_image = opaque_bytes(4096);
    let alias_marker = serde_json::json!({
        "physical": "content/7f9c2ab4-pool",
        "size": 4096,
        "sha256": sha256_hex(&remote_image),
        "logical": "novels/site/title/挿絵/remote.png",
        "prefix": "novels/site/title",
        "etag": "\"etag-pool-1\""
    });
    let tomb_key =
        ObjectKey::try_new(".narou-transfer/.refs/novels/site/title/挿絵/deleted.png").unwrap();
    let tomb_marker = serde_json::json!({
        "physical": "",
        "size": 0,
        "sha256": "",
        "logical": "novels/site/title/挿絵/deleted.png",
        "prefix": "novels/site/title"
    });
    block_on(async {
        store.write_small(&alias_key, serde_json::to_vec(&alias_marker).unwrap()).await.unwrap();
        store.write_small(&tomb_key, serde_json::to_vec(&tomb_marker).unwrap()).await.unwrap();
    });
    let archive = root.join(narou_rs::downloader::ARCHIVE_ROOT_DIR);
    let target = archive.join("site/title/payload.bin");
    assert!(!target.exists(), "mirror-files=false keeps the archive empty");

    narou_rs::native::sqlite::export_yaml::export_yaml(None, true).unwrap();
    assert_eq!(
        state::read_mode(&root.join(".narou")),
        StorageMode::Yaml,
        "the marker flips only after all content is on disk"
    );
    assert_eq!(
        sha256_hex(&std::fs::read(&target).unwrap()),
        expected_sha,
        "the canonical reference resolves to the exact immutable bytes"
    );

    // The blob's reserved namespace must never land in the export: the FS
    // gate machinery owns `.objects/.narou-transfer/` now.
    assert!(
        !archive
            .join(".objects/.narou-transfer/.data/immutable-seed")
            .exists()
    );

    // Both `.refs` markers crossed verbatim to the resolved `.objects` paths;
    // the FS backend's marker lookup reads exactly these files.
    let refs_root = archive.join(".objects/.narou-transfer/.refs");
    let written_alias: serde_json::Value = serde_json::from_slice(
        &std::fs::read(refs_root.join("novels/site/title/挿絵/remote.png")).unwrap(),
    )
    .unwrap();
    assert_eq!(written_alias["physical"].as_str(), Some("content/7f9c2ab4-pool"));
    assert_eq!(written_alias["etag"].as_str(), Some("\"etag-pool-1\""));
    assert_eq!(written_alias["logical"].as_str(), Some("novels/site/title/挿絵/remote.png"));
    let written_tomb: serde_json::Value = serde_json::from_slice(
        &std::fs::read(refs_root.join("novels/site/title/挿絵/deleted.png")).unwrap(),
    )
    .unwrap();
    assert_eq!(written_tomb["physical"].as_str(), Some(""), "deletion tombstone preserved");

    // The adopted namespace authority is a live FS gate with provenance; the
    // SQL row is retired so a stale token cannot revive it.
    {
        let conn = state.conn_ref();
        let conn = conn.lock();
        let sql_gate: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix='novels/site/title')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql_gate, "the SQL gate row is retired");
        let handoff: Option<(i64, i64)> = conn
            .query_row(
                "SELECT sql_epoch,fs_epoch FROM fs_gate_handoffs WHERE prefix='novels/site/title'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        assert_eq!(handoff, Some((5, 6)), "the recorded handoff advances the epoch");
    }
    let gate_file = fs_gate_path(&archive, "novels/site/title");
    let gate: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&gate_file).unwrap()).unwrap();
    assert_eq!(gate["epoch"].as_u64(), Some(6));
    assert_eq!(gate["content_revision"].as_u64(), Some(2));

    // Restart/readback: the YAML mode marker makes `active_for` return None,
    // while the durable reference rows keep their remote semantics intact
    // for the next SQLite session.
    assert!(state::active_for(&root.join(".narou")).is_none());
    let reopened = rusqlite::Connection::open(root.join(".narou/db.sqlite")).unwrap();
    let reference: Option<String> = reopened
        .query_row(
            "SELECT blob_key FROM transfer_object_refs WHERE object_key=?",
            [canonical_key.as_ref()],
            |row| row.get(0),
        )
        .ok();
    assert_eq!(reference.as_deref(), Some(blob_key.as_ref()));

    // Reopen through the real leased S3 consumer, not just the marker codec.
    // The non-hash image must resolve its immutable physical key; the valid
    // deletion marker must never fall back to an old remote logical object.
    use std::sync::Arc;
    use narou_rs::application::novel_access::NovelReadScope;
    use narou_rs::native::object_store::{NativeStore, NativeStores};
    use narou_rs::platform::mocks::MockHttpClient;
    use narou_rs::platform::s3_store::{S3Store, S3StoreConfig};
    use narou_rs::platform::{HttpResponse, ObjectPrefix, SystemClock};
    let http = Arc::new(MockHttpClient::new());
    let physical_url = "https://storage.example/bucket/content/7f9c2ab4-pool";
    http.add_response(physical_url, HttpResponse {
        status: 206,
        headers: vec![("content-range".into(), "bytes 0-4095/4096".into()),
            ("etag".into(), "\"etag-pool-1\"".into())],
        body: remote_image.clone(),
    });
    let remote = Arc::new(S3Store::new(S3StoreConfig {
        endpoint: "https://storage.example".into(), bucket: "bucket".into(),
        region: "us-east-1".into(), prefix: String::new(), illustration_dedup: false,
        access_key_id: "fixture-key".into(), secret_access_key: "fixture-secret".into(),
    }, http.clone(), Arc::new(SystemClock)).unwrap());
    let primary = Arc::new(NativeStore::for_narou_root(&root).unwrap());
    let objects = NativeStores::remote_objects(primary, remote);
    block_on(async {
        let mut scope = NovelReadScope::new(&objects, "cutover-consumer");
        let prefix = ObjectPrefix::new("novels/site/title").unwrap();
        scope.lease_for(&prefix).await.unwrap();
        let image = ObjectKey::try_new("novels/site/title/挿絵/remote.png").unwrap();
        let deleted = ObjectKey::try_new("novels/site/title/挿絵/deleted.png").unwrap();
        let mut stream = scope.open_key(&image, 0..4096).await.unwrap().unwrap();
        let mut actual = Vec::new();
        while let Some(chunk) = futures::StreamExt::next(&mut stream).await { actual.extend(chunk.unwrap()); }
        drop(stream);
        assert_eq!(actual, remote_image);
        assert!(scope.open_key(&deleted, 0..u64::MAX).await.unwrap().is_none());
        scope.release_all().await.unwrap();
    });
    assert_eq!(http.requested_urls(), [format!("GET {physical_url}")]);
    assert!(http.sent_requests()[0].1.iter().any(|(name, value)|
        name.eq_ignore_ascii_case("if-match") && value == "\"etag-pool-1\""));

    // narou.rb sees the same management files it would have produced itself.
    assert!(root.join(".narou/database.yaml").is_file());
    assert!(root.join(".narou/database_index.yaml").is_file());
}

/// A canonical reference whose blob no longer matches the recorded
/// generation is a tombstone: the export must fail before the marker flips
/// and leave SQLite fully authoritative.
#[test]
fn tombstoned_reference_aborts_before_any_flip() {
    let _guard = LIBRARY_LOCK.lock();
    let library = LibraryGuard::enter();
    let root = library.root.clone();
    let (state, store) = db_only_store(&root);

    let blob_key = ObjectKey::try_new(".narou-transfer/.data/gone").unwrap();
    let canonical_key = ObjectKey::try_new("novels/site/title/payload.bin").unwrap();
    block_on(async {
        store
            .write_small(&blob_key, b"still-here".to_vec())
            .await
            .unwrap();
    });
    promote_reference(&state, canonical_key.as_ref(), blob_key.as_ref());
    {
        let conn = state.conn_ref();
        conn.lock()
            .execute(
                "UPDATE objects SET generation='tombstoned' WHERE object_key=?",
                [blob_key.as_ref()],
            )
            .unwrap();
    }

    let error = narou_rs::native::sqlite::export_yaml::export_yaml(None, true).unwrap_err();
    assert!(
        error.to_string().contains("generation") || error.to_string().contains("payload.bin"),
        "{error}"
    );
    assert_eq!(
        state::read_mode(&root.join(".narou")),
        StorageMode::Sqlite,
        "a failed export never partially selects YAML authority"
    );
    let archive = root.join(narou_rs::downloader::ARCHIVE_ROOT_DIR);
    assert!(
        !archive.join("site/title/payload.bin").exists(),
        "staged bodies are never committed by an aborted cutover"
    );
}

/// A write racing the cutover window either commits before the snapshot (and
/// is exported) or trips the fingerprint compare and keeps SQLite
/// authoritative — it is never silently lost after a completed flip.
#[test]
fn concurrent_writes_never_leak_past_a_completed_flip() {
    let _guard = LIBRARY_LOCK.lock();
    let library = LibraryGuard::enter();
    let root = library.root.clone();
    let (_state, store) = db_only_store(&root);

    let key = ObjectKey::try_new("novels/site/title/raced.yaml").unwrap();
    let filler = ObjectKey::try_new("novels/site/title/filler.bin").unwrap();
    block_on(async {
        store.write_small(&key, b"v0".to_vec()).await.unwrap();
        // Give the export a real staging window to race inside.
        let stream = byte_stream(opaque_bytes(24 * 1024 * 1024));
        AssetStore::write_stream(&store, &filler, stream)
            .await
            .unwrap();
    });

    let writer_store = store.clone();
    let writer_key = key.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_stop = stop.clone();
    let writer = std::thread::spawn(move || {
        let mut revision = 1u64;
        while !writer_stop.load(std::sync::atomic::Ordering::Relaxed) {
            let bytes = format!("v{revision}").into_bytes();
            block_on(async {
                let _ = writer_store.write_small(&writer_key, bytes).await;
            });
            revision += 1;
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    });

    let outcome = narou_rs::native::sqlite::export_yaml::export_yaml(None, true);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();

    let archive = root.join(narou_rs::downloader::ARCHIVE_ROOT_DIR);
    let raced_file = archive.join("site/title/raced.yaml");
    match outcome {
        Ok(_) => {
            assert_eq!(state::read_mode(&root.join(".narou")), StorageMode::Yaml);
            // A completed flip publishes one committed revision, and the
            // filler body arrived intact.
            let on_disk = std::fs::read(&raced_file).unwrap();
            let text = String::from_utf8(on_disk).unwrap();
            assert!(text.starts_with('v'), "{text}");
            assert_eq!(
                std::fs::metadata(archive.join("site/title/filler.bin"))
                    .unwrap()
                    .len() as usize,
                24 * 1024 * 1024
            );
        }
        Err(error) => {
            assert_eq!(
                state::read_mode(&root.join(".narou")),
                StorageMode::Sqlite,
                "the aborted cutover keeps the original mode ({error})"
            );
            // The newest committed revision is still readable through SQLite.
            let latest = block_on(store.read_small(&key)).unwrap().unwrap();
            assert!(latest.starts_with(b"v"));
        }
    }
}

/// Crash-boundary proof: a flip that is killed after `write_mode` fsyncs the
/// yaml marker but before its barrier tx commits must never leave "FS chosen
/// by marker while SQL gates/authority are still alive". The durable intent
/// file + committed `cutover_meta.yaml_pending` row make the two halves
/// distinguishable on restart, in both zero-gate and preexisting-gate
/// libraries.
#[test]
fn crashed_flip_restart_recovers_a_single_authority() {
    let _guard = LIBRARY_LOCK.lock();
    let library = LibraryGuard::enter();
    let root = library.root.clone();
    let (_state, store) = db_only_store(&root);
    let narou_dir = root.join(".narou");
    let key = ObjectKey::try_new("novels/site/title/toc.yaml").unwrap();
    block_on(async {
        store.write_small(&key, b"toc-v1".to_vec()).await.unwrap();
    });

    // Simulate a crash right after the fsync'd marker write inside the
    // barrier tx: marker=yaml, durable intent file present, tx never
    // committed (no yaml_pending row, SQL gate rows intact).
    let intent = narou_dir.join(".cutover-yaml-intent");
    std::fs::write(&intent, "sqlite->yaml cutover in flight\n").unwrap();
    state::write_mode(&narou_dir, StorageMode::Yaml).unwrap();

    // "Restart": the same public entrypoint a caller hits after a crash.
    // reconcile must restore the SQLite selector before export re-runs, and
    // the re-run itself then completes the real flip end to end.
    narou_rs::native::sqlite::export_yaml::export_yaml(None, true).unwrap();
    assert_eq!(state::read_mode(&narou_dir), StorageMode::Yaml);
    assert!(!intent.exists(), "converged flip leaves no intent evidence");
    let archive = root.join(narou_rs::downloader::ARCHIVE_ROOT_DIR);
    assert_eq!(
        std::fs::read(archive.join("site/title/toc.yaml")).unwrap(),
        b"toc-v1",
        "the recovered library carries the committed content"
    );

    // Second crash shape: committed flip (pending row + handoff + pending
    // gate journal) killed before FS canonicalization. Re-entry finalizes
    // the journal instead of reverting the selector.
    let intent2 = narou_dir.join(".cutover-yaml-intent");
    std::fs::write(&intent2, "sqlite->yaml cutover in flight\n").unwrap();
    {
        let conn = rusqlite::Connection::open(narou_dir.join("db.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cutover_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL) WITHOUT ROWID;\
             INSERT OR REPLACE INTO cutover_meta(key,value) VALUES('yaml_pending','1');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO novel_access_gates(prefix,epoch,closed,owner,updated_at,content_revision,origin_url_fold) VALUES('novels/site/title',9,0,NULL,1,4,'')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO fs_gate_handoffs(prefix,sql_epoch,fs_epoch,fs_origin_fold,recorded_at) VALUES('novels/site/title',9,10,'',1)",
            [],
        )
        .unwrap();
        // The committed adoption deleted the SQL row inside the tx.
        conn.execute(
            "DELETE FROM novel_access_gates WHERE prefix='novels/site/title'",
            [],
        )
        .unwrap();
    }
    let pending_dir = archive.join(".objects/.narou-transfer/.gate.pending");
    std::fs::create_dir_all(&pending_dir).unwrap();
    let pending_gate = serde_json::json!({
        "prefix": "novels/site/title",
        "origin_url_fold": "",
        "content_revision": 4,
        "epoch": 10,
        "owner": null,
        "updated_at": 1,
        "migrated": false,
        "readers": {}
    });
    std::fs::write(
        pending_dir.join(sha256_hex(b"novels/site/title")),
        serde_json::to_vec(&pending_gate).unwrap(),
    )
    .unwrap();

    // Marker already names yaml (the committed selector); the restart path
    // is NativeStore::for_narou_root — it reconciles intent before choosing
    // the store variant, exactly like a real process restart.
    let restarted = narou_rs::native::object_store::NativeStore::for_narou_root(&root).unwrap();
    assert!(
        matches!(restarted, narou_rs::native::object_store::NativeStore::Fs(_)),
        "a converged yaml marker selects the filesystem store"
    );

    assert_eq!(state::read_mode(&narou_dir), StorageMode::Yaml);
    assert!(!intent2.exists(), "intent discharged after journal finalization");
    assert!(
        !pending_dir.join(sha256_hex(b"novels/site/title")).exists(),
        "the pending journal was canonicalized and retired"
    );
    let gate_file = fs_gate_path(&archive, "novels/site/title");
    let gate: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&gate_file).unwrap()).unwrap();
    assert_eq!(gate["epoch"].as_u64(), Some(10));
    assert_eq!(gate["content_revision"].as_u64(), Some(4));
}

#[test]
fn cutover_succeeds_with_over_1100_novel_access_gates() {
    let _guard = LIBRARY_LOCK.lock();
    let library = LibraryGuard::enter();
    let root = library.root.clone();
    let (state, _store) = db_only_store(&root);
    {
        let mut conn = state.conn_ref().lock();
        let tx = conn.transaction().unwrap();
        for index in 0..1200usize {
            let prefix = format!("novels/site/book_{index:04}");
            tx.execute(
                "INSERT INTO novel_access_gates(prefix,epoch,closed,updated_at) VALUES(?1,1,0,1)",
                [&prefix],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    narou_rs::native::sqlite::export_yaml::export_yaml(None, true).unwrap();
    assert_eq!(state::read_mode(&root.join(".narou")), StorageMode::Yaml);
    let archive = root.join(narou_rs::downloader::ARCHIVE_ROOT_DIR);
    assert!(fs_gate_path(&archive, "novels/site/book_0000").is_file());
    assert!(fs_gate_path(&archive, "novels/site/book_1199").is_file());
    // A completed batch must leave the scan-invalidation seqlock even: an
    // odd value degrades every later gate scan to the uncached walk (issue #58).
    let revision: u64 = std::fs::read_to_string(archive.join(".objects/.narou-transfer/gate-revision"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(revision % 2, 0, "the adoption batch seals the seqlock even");
}

#[test]
fn cutover_skips_rewriting_identical_mirrored_files() {
    let _guard = LIBRARY_LOCK.lock();
    let library = LibraryGuard::enter();
    let root = library.root.clone();
    let narou_dir = root.join(".narou");
    state::write_mode(&narou_dir, StorageMode::Sqlite).unwrap();
    narou_rs::db::init_database().unwrap();
    let state = state::active_for(&narou_dir).unwrap();
    let archive = root.join(narou_rs::downloader::ARCHIVE_ROOT_DIR);
    let mirror = NativeObjectStore::from_root(archive.clone()).unwrap();
    let store = SqliteObjectStore::with_mirror_files(state.conn_ref().clone(), mirror, true);

    let key_same = ObjectKey::try_new("novels/site/book/toc.yaml").unwrap();
    let key_missing = ObjectKey::try_new("novels/site/book/setting.ini").unwrap();
    block_on(store.write_small(&key_same, b"title: same\n".to_vec())).unwrap();
    block_on(store.write_small(&key_missing, b"setting: value\n".to_vec())).unwrap();

    let mirrored_same = archive.join("site/book/toc.yaml");
    let mirrored_missing = archive.join("site/book/setting.ini");
    std::fs::remove_file(&mirrored_missing).unwrap();

    let fixed_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&mirrored_same)
        .unwrap()
        .set_modified(fixed_time)
        .unwrap();
    let mtime_before = std::fs::metadata(&mirrored_same).unwrap().modified().unwrap();

    narou_rs::native::sqlite::export_yaml::export_yaml(None, true).unwrap();
    assert_eq!(state::read_mode(&narou_dir), StorageMode::Yaml);
    let mtime_after = std::fs::metadata(&mirrored_same).unwrap().modified().unwrap();
    assert_eq!(mtime_after, mtime_before, "identical mirrored file must not be rewritten");
    assert_eq!(std::fs::read(&mirrored_missing).unwrap(), b"setting: value\n");
    assert!(!archive.join("site/book/toc.yaml.lock").exists());
}
