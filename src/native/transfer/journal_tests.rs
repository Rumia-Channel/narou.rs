use super::*;
use futures::executor::block_on;

fn scope()->AuthScope{AuthScope{library_id:"library".into(),principal:"admin".into()}}
fn session(id:&str)->SessionRow{let now=Utc::now();SessionRow{session_id:SessionId(id.into()),scope:scope(),secret_hash:SecretHash("0".repeat(64)),direction:TransferDirection::Import,state:SessionState::Transferring,resume_phase:None,revision:1,conflict_policy:ConflictPolicy::Overwrite,profile:TransferProfile::Runtime,extra_roles:Vec::new(),policy_version:1,plan_revision:None,catalog_revision:0,chunk_bytes:CHUNK_BYTES,confirmed_bytes:0,published_novels:0,next_action:NextAction::Upload,operation_id:None,last_operation_id:None,last_operation_result:None,gc_cursor:None,error:None,created_at:now,updated_at:now,expires_at:now+chrono::Duration::seconds(600),ext:SessionExt::Import(ImportSessionExt::default())}}
fn file(id:&SessionId,index:u32)->FileRow{FileRow{session_id:id.clone(),file_id:index,novel_ordinal:0,logical_path:format!("body/{index}.yaml"),role:FileRole::Body,size:10,sha256:sha256_hex(&[0;10]),state:FileState::Pending,confirmed_bytes:0,next_chunk_index:0,confirmed_prefix_sha256:Some(sha256_hex(&[])),hash_checkpoint:None,stage:None,pin:None,finalize_operation:None}}
fn chunk(id:&SessionId,index:u32,size:u64)->ChunkRow{ChunkRow{session_id:id.clone(),file_id:index,chunk_index:0,payload_size:size,declared_sha256:Some(sha256_hex(&[0;4])),stored_sha256:sha256_hex(&[0;4]),prefix_sha256:sha256_hex(&[0;4]),hash_checkpoint:None,offset:0,is_final:false}}
fn operation(id:&SessionId,name:&str)->OperationRow{OperationRow{session_id:id.clone(),operation_id:OperationId(name.into()),kind:"advance".into(),request_fingerprint:name.into(),if_match_revision:None,result:None,completed:false,created_at:Utc::now()}}

#[test]
fn file_pages_are_complete_bounded_and_exclude_manifest(){block_on(async{
    let root=tempfile::tempdir().unwrap();let journal=NativeTransferJournal::open(root.path()).unwrap();let row=session("pages");journal.create_session(&row).await.unwrap();
    for index in 0..251{journal.put_file(&file(&row.session_id,index)).await.unwrap();}journal.put_file(&file(&row.session_id,MANIFEST_FILE_ID)).await.unwrap();
    let mut cursor=None;let mut ids=Vec::new();loop{let page=journal.files(&row.session_id,cursor.as_deref(),100).await.unwrap();assert!(page.items.len()<=100);assert!(serde_json::to_vec(&page).unwrap().len()<=MAX_PAGE_BYTES);ids.extend(page.items.into_iter().map(|file|file.file_id));cursor=page.next_cursor;if cursor.is_none(){break;}}assert_eq!(ids,(0..251).collect::<Vec<_>>());
})}

#[test]
fn reset_removes_only_that_unverified_files_receipts_and_counter(){block_on(async{
    let root=tempfile::tempdir().unwrap();let journal=NativeTransferJournal::open(root.path()).unwrap();let mut row=session("reset");row.confirmed_bytes=10;journal.create_session(&row).await.unwrap();
    let mut first=file(&row.session_id,0);first.state=FileState::Receiving;first.confirmed_bytes=4;first.next_chunk_index=1;first.stage=Some("old-stage".into());journal.put_file(&first).await.unwrap();let mut second=file(&row.session_id,1);second.state=FileState::Finalizing;second.confirmed_bytes=6;second.finalize_operation=Some(OperationId("final".into()));journal.put_file(&second).await.unwrap();
    journal.put("chunk",&row.session_id.0,&chunk_key(0,0),&chunk(&row.session_id,0,4),false).unwrap();journal.put("chunk",&row.session_id.0,&chunk_key(1,0),&chunk(&row.session_id,1,6),false).unwrap();journal.put("active_stage",&row.session_id.0,&number(0),&"old-stage",false).unwrap();
    journal.reset_file(&file(&row.session_id,0)).await.unwrap();assert!(journal.chunk(&row.session_id,0,0).await.unwrap().is_none());assert!(journal.chunk(&row.session_id,1,0).await.unwrap().is_some());let current=journal.session(&scope(),&row.session_id).await.unwrap().unwrap();assert_eq!(current.confirmed_bytes,6);assert_eq!(current.revision,2);assert!(journal.get::<String>("active_stage",&row.session_id.0,&number(0)).unwrap().is_none());assert!(journal.reset_file(&file(&row.session_id,1)).await.is_err());assert_eq!(journal.file(&row.session_id,1).await.unwrap().unwrap(),second);
    let mut manifest=file(&row.session_id,MANIFEST_FILE_ID);manifest.state=FileState::Finalizing;journal.put_file(&manifest).await.unwrap();assert_eq!(journal.finalizing_files(&row.session_id,100).await.unwrap().len(),2);
})}

#[test]
fn session_cursor_orders_nanoseconds_not_random_ids(){block_on(async{
    // Two active sessions exercise sub-microsecond ordering and a page boundary without exceeding admission.
    let root=tempfile::tempdir().unwrap();let journal=NativeTransferJournal::open(root.path()).unwrap();for(id,nanos)in[("a",2),("z",0)]{let mut row=session(id);row.updated_at=DateTime::from_timestamp(1_700_000_000,nanos).unwrap();journal.create_session(&row).await.unwrap();}
    let mut cursor=None;let mut ids=Vec::new();loop{let page=journal.list_sessions(&scope(),cursor.as_deref(),1).await.unwrap();ids.extend(page.items.into_iter().map(|row|row.session_id.0));cursor=page.next_cursor;if cursor.is_none(){break;}}assert_eq!(ids,["z","a"]);
})}

#[test]
fn actual_os_claim_cannot_be_stolen_or_released_by_previous_receipt(){block_on(async{
    let root=tempfile::tempdir().unwrap();let first=NativeTransferJournal::open(root.path()).unwrap();let second=NativeTransferJournal::open(root.path()).unwrap();let row=session("locks");first.create_session(&row).await.unwrap();let one=operation(&row.session_id,"one");let two=operation(&row.session_id,"two");let three=operation(&row.session_id,"three");assert!(first.begin_operation(&one).await.unwrap());assert!(matches!(second.begin_operation(&two).await,Err(TransferError::Busy{..})));first.finish_operation(&one).await.unwrap();assert!(second.begin_operation(&two).await.unwrap());second.finish_operation(&one).await.unwrap();assert!(matches!(first.begin_operation(&three).await,Err(TransferError::Busy{..})));second.finish_operation(&two).await.unwrap();assert!(first.begin_operation(&three).await.unwrap());first.finish_operation(&three).await.unwrap();
})}

#[test]
fn plan_receipt_join_stays_below_public_page_budget(){block_on(async{
    let root=tempfile::tempdir().unwrap();let journal=NativeTransferJournal::open(root.path()).unwrap();let row=session("plans");journal.create_session(&row).await.unwrap();for ordinal in 0..47{let entry=PlanEntry{novel_ordinal:ordinal,toc_url:format!("https://example.test/{ordinal}/{}","x".repeat(7_600)),title:"題".repeat(128),kind:NovelKind::Normal,action:PlanAction::Overwrite,reason:None,existing_id:Some(ordinal as i64),record_revision:Some("a".repeat(64)),content_revision:Some("0".into()),membership_digest:None,file_count:1,total_bytes:10,receipt:None};journal.put_plan_entries(&row.session_id,"plan",&[entry]).await.unwrap();let receipt=PublicationReceipt{novel_ordinal:ordinal,action:PlanAction::Overwrite,outcome:"overwritten".into(),assigned_id:Some(ordinal as i64),files_installed:1,error:None};journal.put_publication(&PublicationRow{session_id:row.session_id.clone(),publication_id:ordinal,state:PublicationState::Committed,spec:Vec::new(),phase_cursor:Some("finished".into()),assigned_id:Some(ordinal as i64),receipt:Some(serde_json::to_vec(&receipt).unwrap()),error:None}).await.unwrap();}
    let mut cursor=None;let mut ordinals=Vec::new();loop{let page=journal.plan_entries(&row.session_id,cursor.as_deref(),100).await.unwrap();assert!(serde_json::to_vec(&page).unwrap().len()<=MAX_PAGE_BYTES);assert!(page.items.iter().all(|entry|entry.receipt.is_some()));ordinals.extend(page.items.into_iter().map(|entry|entry.novel_ordinal));cursor=page.next_cursor;if cursor.is_none(){break;}}assert_eq!(ordinals,(0..47).collect::<Vec<_>>());
})}

#[test]
fn selection_digest_catalog_uses_all_rows_in_utf8_decimal_ordinal_order(){block_on(async{
    let root=tempfile::tempdir().unwrap();let journal=NativeTransferJournal::open(root.path()).unwrap();let row=session("selection");journal.create_session(&row).await.unwrap();for(ordinal,path)in[(2,"😀.txt"),(10,"a.txt"),(1,"z.txt"),(2,"\u{e000}.txt")]{let decision=CatalogDecision{send:"optional".into(),default:false,reason:Some("profile".into())};journal.put_catalog_entries(&row.session_id,1,&[CatalogItem{novel_ordinal:ordinal,existing_id:ordinal as i64,toc_url:format!("https://example.test/{ordinal}"),title:"title".into(),kind:NovelKind::Normal,logical_path:path.into(),role:FileRole::Unknown,size:1,selectable:true,selected:false,required:false,exclusion_reason:None,policy_version:1,runtime:decision.clone(),complete:decision}]).await.unwrap();}
    let page=journal.selection_catalog_entries(&row.session_id,None,100).await.unwrap();let order=page.items.into_iter().map(|entry|(entry.novel_ordinal,entry.logical_path)).collect::<Vec<_>>();assert_eq!(order,vec![(1,"z.txt".into()),(10,"a.txt".into()),(2,"\u{e000}.txt".into()),(2,"😀.txt".into())]);
})}

#[test]
fn copied_control_sanitizer_scrubs_sessions_but_keeps_generation_and_origin_authority(){block_on(async{
    let root=tempfile::tempdir().unwrap();let journal=NativeTransferJournal::open(root.path()).unwrap();let row=session("control-backup");journal.create_session(&row).await.unwrap();let marker="DO_NOT_BACKUP_TRANSFER_PRIVATE_TOKEN";journal.backend_put(&row.session_id.0,"private-token",&marker).unwrap();let image=root.path().join("control-copy.sqlite");
    {let connection=journal.control_connection();let connection=connection.lock();connection.execute_batch("CREATE TABLE native_yaml_id_sequence(id INTEGER PRIMARY KEY CHECK(id=1),next_id INTEGER NOT NULL,next_generation INTEGER NOT NULL); INSERT INTO native_yaml_id_sequence VALUES(1,45,99); CREATE TABLE native_yaml_id_claims(novel_id INTEGER PRIMARY KEY,generation INTEGER NOT NULL,available INTEGER NOT NULL CHECK(available IN(0,1)),retired INTEGER NOT NULL CHECK(retired IN(0,1))); INSERT INTO native_yaml_id_claims VALUES(12,71,0,1); CREATE TABLE native_id_liveness(allocator TEXT NOT NULL,novel_id INTEGER NOT NULL,generation INTEGER NOT NULL,liveness_key TEXT NOT NULL,committed INTEGER NOT NULL CHECK(committed IN(0,1)),PRIMARY KEY(allocator,novel_id)); INSERT INTO native_id_liveness VALUES('yaml',12,71,'12-71',1); CREATE TABLE native_transfer_id_owners(allocator TEXT NOT NULL,owner TEXT NOT NULL,novel_id INTEGER NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(allocator,owner),UNIQUE(allocator,novel_id)); INSERT INTO native_transfer_id_owners VALUES('yaml','old-session:0:plan',12,71); CREATE TABLE novel_access_gates(prefix TEXT PRIMARY KEY,closed INTEGER NOT NULL,owner TEXT,epoch INTEGER NOT NULL,origin_url_fold TEXT NOT NULL); INSERT INTO novel_access_gates VALUES('novels/Test/published',0,NULL,7,'https://example.test/published/');").unwrap();connection.execute("VACUUM INTO ?1",[image.to_str().unwrap()]).unwrap();}
    let mut copy=Connection::open(&image).unwrap();sanitize_control_snapshot(&mut copy).unwrap();assert_eq!(copy.query_row("SELECT COUNT(*) FROM transfer_native_rows",[],|row|row.get::<_,i64>(0)).unwrap(),0);assert_eq!(copy.query_row("SELECT COUNT(*) FROM native_transfer_id_owners",[],|row|row.get::<_,i64>(0)).unwrap(),0);assert_eq!(copy.query_row("SELECT next_id,next_generation FROM native_yaml_id_sequence WHERE id=1",[],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?))).unwrap(),(45,99));assert_eq!(copy.query_row("SELECT generation,retired FROM native_yaml_id_claims WHERE novel_id=12",[],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,bool>(1)?))).unwrap(),(71,true));assert_eq!(copy.query_row("SELECT generation,committed FROM native_id_liveness WHERE novel_id=12",[],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,bool>(1)?))).unwrap(),(71,true));assert_eq!(copy.query_row("SELECT origin_url_fold FROM novel_access_gates",[],|row|row.get::<_,String>(0)).unwrap(),"https://example.test/published/");assert!(!std::fs::read(&image).unwrap().windows(marker.len()).any(|window|window==marker.as_bytes()));assert!(journal.session(&scope(),&row.session_id).await.unwrap().is_some());assert_eq!(journal.backend_get::<String>(&row.session_id.0,"private-token").unwrap().as_deref(),Some(marker));
})}

#[test]
fn sqlite_marker_with_unopenable_db_fails_mutations_instead_of_yaml_fallback(){block_on(async{
    // Regression for the observed incident: a `storage-backend=sqlite`
    // library whose `db.sqlite` cannot be configured used to be accepted by
    // the transfer on silent YAML-file semantics; the publication step then
    // issued SQL against the unmigrated schema (`no such table
    // novel_access_gates`). Mutations must fail closed instead.
    let root=tempfile::tempdir().unwrap();let narou=root.path().join(".narou");std::fs::create_dir_all(&narou).unwrap();
    std::fs::write(narou.join(crate::native::sqlite::state::MARKER_FILE),"sqlite\n").unwrap();
    // `configure` fails on this path (a directory squats on db.sqlite), so
    // `active_for` is `None` while `configured_failure` records the cause.
    std::fs::create_dir_all(narou.join("db.sqlite")).unwrap();
    let journal=NativeTransferJournal::open(root.path()).unwrap();
    let row=session("gated");
    assert!(matches!(journal.create_session(&row).await,Err(TransferError::Unsupported{code,..}) if code=="storage_unavailable"));
    assert!(matches!(journal.backend_put(&row.session_id.0,"k",&1u32),Err(TransferError::Unsupported{code,..}) if code=="storage_unavailable"));
    // Read-only journal paths stay usable for diagnostics; nothing was written.
    assert!(journal.session(&scope(),&row.session_id).await.unwrap().is_none());
})}
