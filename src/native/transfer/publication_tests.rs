use super::*;
use futures::executor::block_on;

fn scope() -> AuthScope { AuthScope { library_id: "library".into(), principal: "admin".into() } }

fn released_session(id: &str) -> SessionRow {
    let now = Utc::now();
    SessionRow {
        session_id: SessionId(id.into()), scope: scope(), secret_hash: SecretHash("0".repeat(64)),
        direction: TransferDirection::Export, state: SessionState::Released, resume_phase: None,
        revision: 1, conflict_policy: ConflictPolicy::Skip, profile: TransferProfile::Runtime,
        extra_roles: Vec::new(), policy_version: 1, plan_revision: None, catalog_revision: 1,
        chunk_bytes: CHUNK_BYTES, confirmed_bytes: 0, published_novels: 0,
        next_action: NextAction::Advance, operation_id: Some(OperationId("op_gc".into())),
        last_operation_id: None, last_operation_result: None, gc_cursor: None, error: None,
        created_at: now, updated_at: now, expires_at: now + chrono::Duration::seconds(600),
        ext: SessionExt::Export(ExportSessionExt::default()),
    }
}

// Regression for the durable-stage live-lock: a terminal session's stage leases
// are still inside their `SESSION_IDLE_TTL_SECS` lease, so reclaim without the
// owner-driven abort refused every attempt and `advance` retried the same
// cursor forever (sessions observed at revision >20k in `state=released`).
#[test]
fn released_export_reclaims_unexpired_stages_instead_of_retrying_forever() {
    let _guard = crate::test_support::global_state_guard();
    block_on(async {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-libraries");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new().prefix("transfer-gc-").tempdir_in(base).unwrap();
        let journal = Arc::new(NativeTransferJournal::open(root.path()).unwrap());
        let backend = NativeTransferBackend::open(root.path().to_path_buf(), journal.clone()).unwrap();
        let row = released_session("sess_gc");
        journal.create_session(&row).await.unwrap();

        let handles = ["stage_dead_owner_sealed", "stage_dead_owner_open", "story_dead_owner_empty"];
        backend.primary.stage_open(handles[0], &row.session_id.0, 1, SESSION_IDLE_TTL_SECS).await.unwrap();
        let cursor = backend.primary.stage_append(handles[0], &row.session_id.0, 1, 0, b"payload", true).await.unwrap();
        let sha = cursor.prefix_sha256().unwrap();
        backend.primary.stage_seal(handles[0], &row.session_id.0, 1, cursor.confirmed_bytes, &sha).await.unwrap();
        for handle in &handles[1..] {
            backend.primary.stage_open(handle, &row.session_id.0, 1, SESSION_IDLE_TTL_SECS).await.unwrap();
        }
        assert_eq!(backend.primary.stage_lease_page(None, 100).await.unwrap().0.len(), 3, "fresh unexpired leases must exist to exercise the refusal path");

        let mut gc: Option<String> = None;
        let mut done = false;
        for _ in 0..16 {
            let step = backend.release_session_resources(&row, gc.as_deref()).await.unwrap();
            if step.done { done = true; break; }
            gc = step.cursor;
        }
        assert!(done, "terminal session GC must finish; a dead owner's stages must not wait out their 24h lease");
        assert!(backend.primary.stage_lease_page(None, 100).await.unwrap().0.is_empty(), "stage ledgers must be gone after collection");
    })
}
