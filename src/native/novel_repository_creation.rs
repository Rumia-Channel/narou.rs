//! The numeric claim, namespace claim and original OS liveness stay coupled.
use super::*;
use crate::platform::{ObjectPrefix,NovelIdReservation,WriteFence,ObjectStore,ObjectListRequest};
use rusqlite::{Connection,OptionalExtension};
use crate::native::sqlite::sqlite_error;

#[derive(Debug,Clone,PartialEq,Eq)]struct Claim{prefix:ObjectPrefix,url:String,epoch:u64,empty_owned:bool}
fn claim(connection:&Connection,reservation:&NovelIdReservation)->Result<Option<Claim>>{
    connection.query_row("SELECT prefix,folded_url,epoch,empty_owned FROM native_transfer_target_claims WHERE owner_kind='ordinary' AND novel_id=?1 AND generation=?2 AND liveness_key=?3 AND owner=('ordinary:'||?1||':'||?2||':'||epoch)",(reservation.id().0,reservation.generation(),format!("{}-{}",reservation.id().0,reservation.generation())),|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,crate::native::sqlite::read_u64(row,2)?,row.get::<_,bool>(3)?))).optional().map_err(sqlite_error)?.map(|(prefix,url,epoch,empty_owned)|Ok(Claim{prefix:ObjectPrefix::new(prefix)?,url,epoch,empty_owned})).transpose()
}
fn current_matches(record:&NovelRecord,claim:&Claim)->Result<bool>{let keys=crate::platform::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;Ok(keys.prefix().as_ref()==claim.prefix.as_ref().trim_end_matches('/')&&crate::db::novel_codec::fold(&record.toc_url)==claim.url)}
fn occupied(objects:&dyn ObjectStore,prefix:ObjectPrefix)->Result<bool>{
    let mut request=ObjectListRequest::new(prefix,std::num::NonZeroUsize::new(1).unwrap());loop{let page=futures::executor::block_on(objects.list_page(&request))?;if !page.objects.is_empty(){return Ok(true);}let Some(next)=page.next_cursor else{return Ok(false);};if request.cursor.as_ref()==Some(&next){return Err(NarouError::Platform("empty namespace inventory did not advance".into()));}request.cursor=Some(next);}
}
impl NativeNovelRepository{
    pub(super) fn reserve_native_creation(&self,fence:&WriteFence,reservation:&NovelIdReservation,toc_url:&str)->Result<()>{
        let stores=crate::native::object_store::NativeStores::for_current_root()?;let root=stores.primary.archive_root_path().parent().ok_or_else(||NarouError::Platform("archive has no library root".into()))?.to_owned();let url=crate::db::novel_codec::fold(toc_url);
        stores.primary.with_open_write(fence,||{
            let(_,origin)=stores.primary.namespace_state_locked(&fence.prefix)?;if !origin.is_empty()&&origin!=url{return Err(NarouError::Platform("namespace belongs to another source URL".into()));}
            let existing={let connection=crate::native::transfer::control_connection(&root)?;let connection=connection.lock();claim(&connection,reservation)?};
            let empty_owned=if let Some(existing)=existing{if existing.prefix.as_ref().trim_end_matches('/')!=fence.prefix.as_ref().trim_end_matches('/')||existing.epoch!=fence.epoch||existing.url!=url{return Err(NarouError::Platform("creation claim identity changed".into()));}existing.empty_owned}else if !origin.is_empty(){false}else{
                let registered=self.get_current_sync(reservation.id())?;if let Some(record)=registered{if !current_matches(&record,&Claim{prefix:fence.prefix.clone(),url:url.clone(),epoch:fence.epoch,empty_owned:false})?{return Err(NarouError::Platform("creation ID already belongs to another identity".into()));}false}else{
                    if !reservation.has_liveness(){return Err(NarouError::Platform("native pending creation has no retained OS liveness".into()));}
                    if occupied(stores.objects.as_ref(),fence.prefix.clone())?||occupied(stores.objects.as_ref(),ObjectPrefix::new(format!("{}/挿絵",fence.prefix.as_ref().trim_end_matches('/')))?)?{return Err(NarouError::Platform("nonempty legacy namespace has no proven source origin".into()));}true
                }
            };
            crate::db::with_database_mut(|database|database.reserve_create_identity(fence,reservation,toc_url,empty_owned))?;
            crate::db::with_database(|database|database.with_library_metadata_snapshot(|inventory,primary|crate::native::transfer::with_control_metadata_snapshot(inventory.root_dir(),primary,|connection|crate::native::object_store::bind_namespace_origin(inventory.root_dir(),connection,&fence.prefix,&url))))
        })
    }
    pub(super) fn release_native_creation(&self,reservation:NovelIdReservation)->Result<()>{
        let store=current_store()?;let root=store.archive_root_path().parent().ok_or_else(||NarouError::Platform("archive has no library root".into()))?.to_owned();let existing={let connection=crate::native::transfer::control_connection(&root)?;let connection=connection.lock();claim(&connection,&reservation)?};
        let Some(existing)=existing else{return crate::db::with_database_mut(|database|database.release_id(&reservation));};
        let mut fence=WriteFence::new(existing.prefix.clone(),existing.epoch);fence.retain_reservation(&reservation)?;
        // A bare published replay may consume its exact receipt, but an orphan
        // scan must still exclude a live creator before promoting YAML publish.
        let _liveness=if reservation.has_liveness(){None}else{Some(crate::db::database::id_reservations::try_liveness_lock(&root,reservation.id(),reservation.generation())?.ok_or_else(||crate::platform::access_fence::novel_busy_error(&existing.prefix))?)};
        store.with_gate_lock(&existing.prefix,||{
            let(state,origin)=store.namespace_state_locked(&existing.prefix)?;let next=existing.epoch.checked_add(1).ok_or_else(||NarouError::Platform("creation epoch exhausted".into()))?;
            enum Outcome{Published,Pending,Released}
            let outcome=crate::db::with_database(|database|database.with_library_metadata_snapshot(|inventory,primary|{let yaml=database.metadata_is_yaml_in_snapshot(primary);crate::native::transfer::with_control_metadata_snapshot(inventory.root_dir(),primary,|connection|{
                let transaction=rusqlite::Transaction::new_unchecked(connection,rusqlite::TransactionBehavior::Immediate).map_err(sqlite_error)?;
                if claim(&transaction,&reservation)?.as_ref()!=Some(&existing){return Err(NarouError::Platform("creation namespace changed before release".into()));}
                let committed=crate::db::database::id_reservations::verify_generation(&transaction,&reservation,yaml)?;
                let current=if yaml{load_current_yaml_records(inventory)?.remove(&reservation.id().0)}else{crate::native::sqlite::SqliteNovelRepository::point_snapshot_conn(&transaction,reservation.id())?};
                if let Some(record)=current{
                    if !committed||!current_matches(&record,&existing)?{return Err(NarouError::Platform("published creation receipt does not match current book".into()));}
                    if state.state!=crate::platform::GateState::Open||state.epoch!=existing.epoch{return Err(crate::platform::access_fence::gate_closed_error(&existing.prefix));}
                    crate::db::database::id_reservations::acknowledge_create(&transaction,&reservation,yaml)?;
                    crate::native::transfer::consume_ordinary_claim_conn(&transaction,reservation.id().0,reservation.generation(),&existing.prefix,existing.epoch)?;
                    transaction.commit().map_err(sqlite_error)?;return Ok(Outcome::Published);
                }
                if state.state==crate::platform::GateState::Closed{
                    let owner=state.owner.as_deref().ok_or_else(||NarouError::Platform("closed cleanup has no owner".into()))?;
                    if state.epoch!=next||!existing.empty_owned||crate::platform::access_fence::parse_removal_owner(owner)?!=(reservation.id(),true){return Err(crate::platform::access_fence::gate_closed_error(&existing.prefix));}
                    transaction.commit().map_err(sqlite_error)?;return Ok(Outcome::Pending);
                }
                if state.epoch==next{
                    if existing.empty_owned&&!origin.is_empty()||!existing.empty_owned&&origin!=existing.url{return Err(NarouError::Platform("post-cleanup namespace origin is not proven".into()));}
                    transaction.commit().map_err(sqlite_error)?;return Ok(Outcome::Released);
                }
                if state.epoch!=existing.epoch||!origin.is_empty()&&origin!=existing.url||!existing.empty_owned&&origin.is_empty(){return Err(NarouError::Platform("creation release lost its original epoch/origin".into()));}
                crate::native::transfer::assert_removal_identity(inventory,if yaml{None}else{Some(&*transaction)},&transaction,&fence,reservation.id())?;
                let sql=primary.is_some().then_some(&*transaction);
                let outcome=if existing.empty_owned{let owner=crate::platform::access_fence::new_removal_owner(reservation.id(),true)?;store.persist_retirement_locked(&fence,&owner,sql)?;Outcome::Pending}else{store.invalidate_creation_locked(&fence,sql)?;Outcome::Released};
                transaction.commit().map_err(sqlite_error)?;Ok(outcome)
            })}))?;
            match outcome{
                Outcome::Published=>Ok(()),
                // The closed-removal mark committed inside the transaction:
                // release succeeded, only the deferred namespace teardown
                // remains (reader drain + orphan sweep). Report success so a
                // cancelled create still returns its original outcome instead
                // of `novel_recovering: creation cleanup pending`.
                Outcome::Pending=>Ok(()),
                Outcome::Released=>{
                    // This proof is deliberately outside DATABASE, while the
                    // original OS lock still excludes every gate transition.
                    let(proof,origin)=store.namespace_state_locked(&existing.prefix)?;if proof.state!=crate::platform::GateState::Open||proof.epoch!=next||existing.empty_owned&&!origin.is_empty()||!existing.empty_owned&&origin!=existing.url{return Err(NarouError::Platform("creation cleanup is not durably open".into()));}
                    crate::db::with_database_mut(|database|database.release_creation_after_cleanup(&existing.prefix,next,&reservation))
                }
            }
        })
    }
    /// Startup collector: actual nonblocking OS ownership, never age/PID/TTL.
    pub fn reap_creation_claims_sync(&self,cursor:Option<i64>,limit:usize)->Result<(usize,Option<i64>)>{
        let(root,(candidates,next))=crate::db::with_database(|database|Ok((database.archive_root().parent().ok_or_else(||NarouError::Platform("archive has no library root".into()))?.to_owned(),database.ordinary_id_liveness_page(cursor,limit.clamp(1,100))?)))?;let mut count=0;
        for(id,generation)in candidates{let Some(resource)=crate::db::database::id_reservations::try_liveness_lock(&root,id,generation)?else{continue;};let reservation=NovelIdReservation::new(id,generation).with_liveness(resource);match self.release_native_creation(reservation){Ok(())=>count+=1,Err(error)=>return Err(error)}}Ok((count,next))
    }
}
