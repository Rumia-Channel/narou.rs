//! Ordinary creation shares the real namespace, job and numeric-ID authority.
use super::*;
use narou_rs::platform::{JobExecutionOwner, NovelIdReservation, NovelObjectKeys, ObjectListRequest, ObjectPrefix, WriteFence};
use std::num::NonZeroUsize;

const EXACT_MAX:i64=9_007_199_254_740_991;
const ABORT:&str="abs(-9223372036854775808)";
fn text(value:&str)->BindValue{BindValue::Text(value.to_owned())}
fn exact(value:i64)->Result<i64>{if !(0..=EXACT_MAX).contains(&value){return Err(NarouError::Platform("creation identity exceeds exact D1 range".into()));}Ok(value)}
fn owner_for<'a>(fence:&WriteFence,reservation:&'a NovelIdReservation)->Result<&'a Arc<JobExecutionOwner>>{
    let owner=reservation.job_owner().ok_or_else(||NarouError::Platform("creation reservation has no execution owner".into()))?;
    if !fence.job_owner().is_some_and(|current|current.as_ref()==owner.as_ref()){return Err(NarouError::Platform("creation writer belongs to another execution".into()));}Ok(owner)
}
fn claim_owner(reservation:&NovelIdReservation)->String{format!("create:{}:{}",reservation.id().0,reservation.generation())}
pub(crate) fn ancestors(prefix:&str)->Result<String>{let entries=prefix.match_indices('/').map(|(index,_)|&prefix[..index]).chain(std::iter::once(prefix)).collect::<Vec<_>>();serde_json::to_string(&entries).map_err(worker_error)}
fn checkpoint_pair(raw:Option<&str>,job_id:&str)->Result<Option<(NovelId,i64)>>{
    let Some(raw)=raw else{return Ok(None);};let checkpoint:JsonValue=serde_json::from_str(raw).map_err(worker_error)?;
    if checkpoint.get("job_id").and_then(JsonValue::as_str)!=Some(job_id){return Err(NarouError::Platform("creation checkpoint belongs to another job".into()));}
    let Some(pair)=checkpoint.get("creation_reservation").filter(|value|!value.is_null())else{return Ok(None);};
    let pair=pair.as_array().filter(|pair|pair.len()==2).ok_or_else(||NarouError::Platform("invalid creation reservation checkpoint".into()))?;
    let id=pair[0].as_i64().ok_or_else(||NarouError::Platform("invalid checkpoint reservation id".into()))?;let generation=pair[1].as_i64().ok_or_else(||NarouError::Platform("invalid checkpoint reservation generation".into()))?;
    Ok(Some((NovelId(exact(id)?),exact(generation)?)))
}
#[derive(Deserialize)]struct JobRow{job_id:String,status:String,execution_token:Option<String>,lease_until:Option<String>,checkpoint_json:Option<String>}
#[derive(Deserialize)]struct Numeric{id:i64,generation:i64,job_id:Option<String>,available:i64}
#[derive(Deserialize)]struct Receipt{novel_id:i64,reservation_generation:i64,prefix:String,toc_url_fold:String,epoch:i64,job_id:String}
#[derive(Deserialize)]struct Claim{prefix:String,novel_id:i64,reservation_generation:i64,toc_url_fold:String,owner:String,epoch:i64,job_id:Option<String>,empty_owned:i64}
#[derive(Deserialize)]struct Origin{epoch:i64,content_revision:i64,closed:i64,owner:Option<String>,origin_url_fold:String}

impl D1NovelRepository{
    fn guard(&self,condition:&str,binds:Vec<BindValue>)->Result<D1PreparedStatement>{self.prepare(&format!("SELECT CASE WHEN {condition} THEN 1 ELSE {ABORT} END"),binds)}
    /// Wraps each D1 batch so a guard abort (`abs(INT64_MIN)` -> "integer
    /// overflow") records which creation path rejected it, not just the error.
    #[track_caller]
    async fn batch_checked(&self,statements:Vec<D1PreparedStatement>)->Result<Vec<D1Result>>{let site=std::panic::Location::caller();let results=self.db.batch(statements).await.map_err(|e|worker_error(format!("D1 batch failed ({site}): {e}")))?;ensure_batch_success(&results).map_err(|e|worker_error(format!("guard batch rejected ({site}): {e}")))?;Ok(results)}
    fn live_job_guard(&self,owner:&JobExecutionOwner)->Result<D1PreparedStatement>{
        if owner.job_id().is_empty()||owner.execution_token().is_empty(){return Err(NarouError::Platform("empty execution ownership".into()));}
        self.guard("EXISTS(SELECT 1 FROM worker_jobs WHERE job_id=?1 AND execution_token=?2 AND status='running' AND julianday(lease_until)>julianday('now'))",vec![text(owner.job_id()),text(owner.execution_token())])
    }
    fn checkpoint_guard(&self,owner:&JobExecutionOwner,reservation:&NovelIdReservation,fence:Option<&WriteFence>)->Result<D1PreparedStatement>{
        exact(reservation.id().0)?;exact(reservation.generation())?;
        let mut condition=String::from("EXISTS(SELECT 1 FROM worker_jobs WHERE job_id=?1 AND execution_token=?2 AND status='running' AND julianday(lease_until)>julianday('now') AND json_valid(checkpoint_json) AND json_extract(checkpoint_json,'$.job_id')=?1 AND json_type(checkpoint_json,'$.creation_reservation')='array' AND json_array_length(checkpoint_json,'$.creation_reservation')=2 AND json_extract(checkpoint_json,'$.creation_reservation[0]')=?3 AND json_extract(checkpoint_json,'$.creation_reservation[1]')=?4");
        let mut values=vec![text(owner.job_id()),text(owner.execution_token()),BindValue::Int(reservation.id().0),BindValue::Int(reservation.generation())];
        if let Some(fence)=fence{let epoch=i64::try_from(fence.epoch).map_err(worker_error)?;exact(epoch)?;condition.push_str(" AND rtrim(json_extract(checkpoint_json,'$.write_origin.prefix'),'/')=?5 AND json_extract(checkpoint_json,'$.write_origin.epoch')=?6");values.extend([text(fence.prefix.as_ref().trim_end_matches('/')),BindValue::Int(epoch)]);}
        condition.push(')');self.guard(&condition,values)
    }
    async fn receipt(&self,id:NovelId,generation:i64)->Result<Option<Receipt>>{self.prepare("SELECT novel_id,reservation_generation,prefix,toc_url_fold,epoch,job_id FROM novel_creation_receipts WHERE novel_id=? AND reservation_generation=?",vec![BindValue::Int(id.0),BindValue::Int(generation)])?.first(None).await.map_err(worker_error)}
    async fn creation_claim(&self,id:NovelId,generation:i64)->Result<Option<Claim>>{self.prepare("SELECT prefix,novel_id,reservation_generation,toc_url_fold,owner,epoch,job_id,empty_owned FROM novel_namespace_claims WHERE novel_id=? AND reservation_generation=?",vec![BindValue::Int(id.0),BindValue::Int(generation)])?.first(None).await.map_err(worker_error)}
    async fn origin(&self,prefix:&str)->Result<Origin>{self.prepare("SELECT epoch,content_revision,closed,owner,origin_url_fold FROM novel_access_gates WHERE prefix=?",vec![text(prefix)])?.first(None).await.map_err(worker_error)?.ok_or_else(||NarouError::Platform("creation gate is missing".into()))}
    pub(super) async fn allocate_job_creation(&self,owner:&Arc<JobExecutionOwner>)->Result<NovelIdReservation>{
        let primary=self.primary();let row:JobRow=primary.prepare("SELECT job_id,status,execution_token,lease_until,checkpoint_json FROM worker_jobs WHERE job_id=? AND execution_token=? AND status='running' AND julianday(lease_until)>julianday('now')",vec![text(owner.job_id()),text(owner.execution_token())])?.first(None).await.map_err(worker_error)?.ok_or_else(||NarouError::Platform("job execution ownership lost".into()))?;
        if let Some((id,generation))=checkpoint_pair(row.checkpoint_json.as_deref(),owner.job_id())?{return primary.resume_job_creation(owner,id,generation).await;}
        primary.batch_checked(vec![primary.guard("EXISTS(SELECT 1 FROM worker_jobs WHERE job_id=?1 AND (checkpoint_json IS NULL OR (json_valid(checkpoint_json) AND json_extract(checkpoint_json,'$.novel_id') IS NULL AND json_extract(checkpoint_json,'$.write_origin') IS NULL AND json_extract(checkpoint_json,'$.creation_reservation') IS NULL)))",vec![text(owner.job_id())])?]).await?;
        let allocation=novel_query::ALLOCATE_READ_SQL;
        let mut statements=vec![primary.live_job_guard(owner)?,primary.guard("NOT EXISTS(SELECT 1 FROM novel_id_reservations WHERE job_id=?1 AND available=0) AND COALESCE((SELECT MAX(generation) FROM novel_id_reservations),0)<9007199254740991 AND COALESCE((SELECT next_id FROM novel_id_sequence WHERE id=1),0)<9007199254740991",vec![text(owner.job_id())])?,primary.prepare(novel_query::ALLOCATE_STEP_SQL,vec![])?,primary.prepare(novel_query::ALLOCATE_BUMP_SQL,vec![])?,primary.prepare(&format!("UPDATE novel_id_reservations SET job_id=?1 WHERE (id,generation) IN ({allocation}) AND job_id IS NULL"),vec![text(owner.job_id())])?,primary.guard("changes()=1",vec![])?];
        statements.push(primary.prepare("UPDATE worker_jobs SET checkpoint_json=json_set(COALESCE(checkpoint_json,'{}'),'$.job_id',job_id,'$.novel_id',NULL,'$.phase','planned','$.next_section_index',NULL,'$.write_origin',NULL,'$.creation_reservation',json_array((SELECT id FROM novel_id_reservations WHERE job_id=?1 AND available=0),(SELECT generation FROM novel_id_reservations WHERE job_id=?1 AND available=0))),updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE job_id=?1 AND execution_token=?2 AND status='running' AND julianday(lease_until)>julianday('now') AND checkpoint_json IS ?3",vec![text(owner.job_id()),text(owner.execution_token()),row.checkpoint_json.map_or(BindValue::Null,BindValue::Text)])?);
        statements.push(primary.guard("changes()=1",vec![])?);statements.push(primary.prepare("SELECT id,generation,job_id,available FROM novel_id_reservations WHERE job_id=? AND available=0",vec![text(owner.job_id())])?);
        let results=primary.batch_checked(statements).await?;let mut rows=results.last().ok_or_else(||NarouError::Platform("numeric allocation result missing".into()))?.results::<Numeric>().map_err(worker_error)?;
        if rows.len()!=1{return Err(NarouError::Platform("numeric job ownership is not unique".into()));}let row=rows.pop().expect("one reservation");NovelIdReservation::new(NovelId(exact(row.id)?),exact(row.generation)?).with_job_owner(owner.clone())
    }
    pub(super) async fn resume_job_creation(&self,owner:&Arc<JobExecutionOwner>,id:NovelId,generation:i64)->Result<NovelIdReservation>{
        let primary=self.primary();let reservation=NovelIdReservation::new(NovelId(exact(id.0)?),exact(generation)?).with_job_owner(owner.clone())?;
        primary.batch_checked(vec![primary.checkpoint_guard(owner,&reservation,None)?,primary.guard("EXISTS(SELECT 1 FROM novel_id_reservations WHERE id=?1 AND generation=?2 AND job_id=?3 AND available=0) OR EXISTS(SELECT 1 FROM novel_creation_receipts r JOIN worker_jobs j ON j.job_id=r.job_id WHERE r.novel_id=?1 AND r.reservation_generation=?2 AND r.job_id=?3 AND json_valid(j.checkpoint_json) AND rtrim(json_extract(j.checkpoint_json,'$.write_origin.prefix'),'/')=r.prefix AND json_extract(j.checkpoint_json,'$.write_origin.epoch')=r.epoch)",vec![BindValue::Int(id.0),BindValue::Int(generation),text(owner.job_id())])?]).await?;Ok(reservation)
    }
    async fn verify_creation_replay(&self,fence:&WriteFence,reservation:&NovelIdReservation,url:&str,receipt:&Receipt)->Result<()>{
        let owner=owner_for(fence,reservation)?;let prefix=fence.prefix.as_ref().trim_end_matches('/');let folded=fold(url);
        if receipt.novel_id!=reservation.id().0||receipt.reservation_generation!=reservation.generation()||receipt.prefix!=prefix||receipt.toc_url_fold!=folded||receipt.epoch as u64!=fence.epoch||receipt.job_id!=owner.job_id(){return Err(NarouError::Platform("published creation proof conflicts with this invocation".into()));}
        let storage=crate::d1_object_store::D1ObjectStore::new(self.db.clone());let token=crate::content_store::nonce()?;
        self.batch_checked(vec![self.checkpoint_guard(owner,reservation,Some(fence))?,storage.open_guard(fence,&token)?,self.guard("EXISTS(SELECT 1 FROM novel_creation_receipts r JOIN novels n ON n.id=r.novel_id JOIN novel_gate_bindings b ON b.novel_id=n.id WHERE r.novel_id=?1 AND r.reservation_generation=?2 AND r.prefix=?3 AND r.toc_url_fold=?4 AND r.epoch=?5 AND r.job_id=?6 AND n.toc_url_fold=r.toc_url_fold AND b.prefix=r.prefix)",vec![BindValue::Int(reservation.id().0),BindValue::Int(reservation.generation()),text(prefix),text(&folded),BindValue::Int(fence.epoch as i64),text(owner.job_id())])?,storage.finish_publication_guard(&token)?]).await?;Ok(())
    }
    pub(super) async fn reserve_creation(&self,fence:&WriteFence,reservation:&NovelIdReservation,url:&str)->Result<()>{
        let primary=self.primary();let owner=owner_for(fence,reservation)?;let prefix=fence.prefix.as_ref().trim_end_matches('/');let folded=fold(url);let epoch=exact(i64::try_from(fence.epoch).map_err(worker_error)?)?;
        if let Some(receipt)=primary.receipt(reservation.id(),reservation.generation()).await?{return primary.verify_creation_replay(fence,reservation,url,&receipt).await;}
        if let Some(claim)=primary.creation_claim(reservation.id(),reservation.generation()).await?{
            if claim.prefix!=prefix||claim.toc_url_fold!=folded||claim.epoch!=epoch||claim.job_id.as_deref()!=Some(owner.job_id())||claim.owner!=claim_owner(reservation){return Err(NarouError::Platform("creation claim changed".into()));}
            primary.batch_checked(primary.creation_commit_guards(fence,reservation)?).await?;return Ok(());
        }
        primary.batch_checked(vec![primary.checkpoint_guard(owner,reservation,Some(fence))?]).await?;
        let origin=primary.origin(prefix).await?;if origin.closed!=0||origin.epoch!=epoch||(!origin.origin_url_fold.is_empty()&&origin.origin_url_fold!=folded){return Err(NarouError::Platform("creation namespace is recovering or belongs to another URL".into()));}
        if origin.origin_url_fold.is_empty(){let objects=primary.objects.as_ref().ok_or_else(||NarouError::Platform("creation storage capability is not configured".into()))?;
            for probe in [prefix.to_owned(),format!("{prefix}/挿絵")]{let mut request=ObjectListRequest::new(ObjectPrefix::new(probe)?,NonZeroUsize::new(1).expect("one"));loop{let page=objects.list_page(&request).await?;if !page.objects.is_empty(){return Err(NarouError::Platform("unowned namespace contains retained data".into()));}let Some(next)=page.next_cursor else{break;};if request.cursor.as_ref()==Some(&next){return Err(NarouError::Platform("empty namespace inventory did not advance".into()));}request.cursor=Some(next);}}
        }
        let empty_owned=origin.origin_url_fold.is_empty();let claim_owner=claim_owner(reservation);let upper=format!("{prefix}0");let lower=format!("{prefix}/");let ancestor_json=ancestors(prefix)?;
        let mut statements=vec![primary.checkpoint_guard(owner,reservation,Some(fence))?,primary.guard("EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix=?1 AND closed=0 AND epoch=?2 AND content_revision=?3 AND origin_url_fold=?4)",vec![text(prefix),BindValue::Int(epoch),BindValue::Int(origin.content_revision),text(&origin.origin_url_fold)])?,primary.guard("EXISTS(SELECT 1 FROM novel_id_reservations WHERE id=?1 AND generation=?2 AND available=0 AND job_id=?3) AND NOT EXISTS(SELECT 1 FROM novels WHERE id=?1 OR toc_url_fold=?4)",vec![BindValue::Int(reservation.id().0),BindValue::Int(reservation.generation()),text(owner.job_id()),text(&folded)])?];
        statements.push(primary.guard("NOT EXISTS(SELECT 1 FROM novel_gate_bindings b JOIN novels n ON n.id=b.novel_id WHERE b.prefix IN (SELECT value FROM json_each(?1)) OR (b.prefix>=?2 AND b.prefix<?3)) AND NOT EXISTS(SELECT 1 FROM novel_namespace_claims WHERE owner<>?4 AND (prefix IN (SELECT value FROM json_each(?1)) OR (prefix>=?2 AND prefix<?3))) AND NOT EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix<>?5 AND (closed=1 OR origin_url_fold<>'') AND (prefix IN (SELECT value FROM json_each(?1)) OR (prefix>=?2 AND prefix<?3)))",vec![text(&ancestor_json),text(&lower),text(&upper),text(&claim_owner),text(prefix)])?);
        statements.push(primary.prepare("INSERT OR IGNORE INTO novel_namespace_claims(prefix,novel_id,reservation_generation,toc_url_fold,owner,epoch,job_id,empty_owned) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",vec![text(prefix),BindValue::Int(reservation.id().0),BindValue::Int(reservation.generation()),text(&folded),text(&claim_owner),BindValue::Int(epoch),text(owner.job_id()),BindValue::Int(i64::from(empty_owned))])?);
        statements.push(primary.prepare("UPDATE novel_access_gates SET origin_url_fold=?1 WHERE prefix=?2 AND closed=0 AND epoch=?3 AND origin_url_fold IN ('',?1)",vec![text(&folded),text(prefix),BindValue::Int(epoch)])?);statements.extend(primary.creation_commit_guards(fence,reservation)?);primary.batch_checked(statements).await?;Ok(())
    }
    pub(super) fn creation_commit_guards(&self,fence:&WriteFence,reservation:&NovelIdReservation)->Result<Vec<D1PreparedStatement>>{
        let owner=owner_for(fence,reservation)?;let prefix=fence.prefix.as_ref().trim_end_matches('/');
        Ok(vec![self.checkpoint_guard(owner,reservation,Some(fence))?,self.guard("EXISTS(SELECT 1 FROM novel_namespace_claims c JOIN novel_access_gates g ON g.prefix=c.prefix JOIN novel_id_reservations i ON i.id=c.novel_id AND i.generation=c.reservation_generation WHERE c.prefix=?1 AND c.novel_id=?2 AND c.reservation_generation=?3 AND c.owner=?4 AND c.job_id=?5 AND c.epoch=?6 AND i.available=0 AND i.job_id=c.job_id AND g.closed=0 AND g.epoch=c.epoch AND g.origin_url_fold=c.toc_url_fold) AND NOT EXISTS(SELECT 1 FROM novels WHERE id=?2)",vec![text(prefix),BindValue::Int(reservation.id().0),BindValue::Int(reservation.generation()),text(&claim_owner(reservation)),text(owner.job_id()),BindValue::Int(fence.epoch as i64)])?])
    }
    pub(super) fn creation_commit_finish(&self,fence:&WriteFence,reservation:&NovelIdReservation)->Result<Vec<D1PreparedStatement>>{
        let owner=owner_for(fence,reservation)?;
        Ok(vec![self.prepare("INSERT INTO novel_creation_receipts(novel_id,reservation_generation,prefix,toc_url_fold,epoch,job_id) SELECT novel_id,reservation_generation,prefix,toc_url_fold,epoch,job_id FROM novel_namespace_claims WHERE novel_id=?1 AND reservation_generation=?2 AND owner=?3 AND job_id=?4 ON CONFLICT(novel_id,reservation_generation) DO UPDATE SET prefix=excluded.prefix,toc_url_fold=excluded.toc_url_fold,epoch=excluded.epoch,job_id=excluded.job_id",vec![BindValue::Int(reservation.id().0),BindValue::Int(reservation.generation()),text(&claim_owner(reservation)),text(owner.job_id())])?,self.guard("changes()=1",vec![])?,self.prepare("DELETE FROM novel_namespace_claims WHERE novel_id=?1 AND reservation_generation=?2 AND owner=?3 AND job_id=?4",vec![BindValue::Int(reservation.id().0),BindValue::Int(reservation.generation()),text(&claim_owner(reservation)),text(owner.job_id())])?,self.guard("changes()=1",vec![]) ?])
    }
    pub(super) async fn commit_creation(&self,fence:&WriteFence,reservation:&NovelIdReservation,record:NovelRecord)->Result<()>{
        let primary=self.primary();let keys=NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;if record.id!=reservation.id().0||keys.prefix().as_ref()!=fence.prefix.as_ref().trim_end_matches('/'){return Err(NarouError::Platform("creation record identity differs from reservation".into()));}
        if let Some(receipt)=primary.receipt(reservation.id(),reservation.generation()).await?{return primary.verify_creation_replay(fence,reservation,&record.toc_url,&receipt).await;}
        let claim=primary.creation_claim(reservation.id(),reservation.generation()).await?.ok_or_else(||NarouError::Platform("creation identity was not reserved".into()))?;if claim.toc_url_fold!=fold(&record.toc_url){return Err(NarouError::Platform("creation URL changed".into()));}
        primary.apply_mutations_primary(vec![NovelMutation::Upsert(record)],Some(std::slice::from_ref(fence)),Some((fence,reservation))).await
    }
    pub(crate) fn namespace_mutation_guard(&self,record:&NovelRecord)->Result<D1PreparedStatement>{
        let keys=NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;let prefix=keys.prefix().as_ref();let lower=format!("{prefix}/");let upper=format!("{prefix}0");
        self.guard("EXISTS(SELECT 1 FROM novels n JOIN novel_gate_bindings b ON b.novel_id=n.id WHERE n.id=?1 AND n.toc_url_fold=?2 AND b.prefix=?3) OR (NOT EXISTS(SELECT 1 FROM novel_namespace_claims WHERE novel_id=?1 OR toc_url_fold=?2 OR prefix IN (SELECT value FROM json_each(?4)) OR (prefix>=?5 AND prefix<?6)) AND NOT EXISTS(SELECT 1 FROM novel_gate_bindings b JOIN novels n ON n.id=b.novel_id WHERE n.id<>?1 AND (b.prefix IN (SELECT value FROM json_each(?4)) OR (b.prefix>=?5 AND b.prefix<?6))) AND NOT EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix<>?3 AND (closed=1 OR origin_url_fold<>'') AND (prefix IN (SELECT value FROM json_each(?4)) OR (prefix>=?5 AND prefix<?6))))",vec![BindValue::Int(record.id),text(&fold(&record.toc_url)),text(prefix),text(&ancestors(prefix)?),text(&lower),text(&upper)])
    }
    fn terminal_job_guard(&self,job:&JobRow)->Result<D1PreparedStatement>{self.guard("EXISTS(SELECT 1 FROM worker_jobs WHERE job_id=?1 AND status IN ('succeeded','partial','blocked','permanent') AND execution_token IS ?2 AND lease_until IS ?3 AND checkpoint_json IS ?4 AND (execution_token IS NULL OR (lease_until IS NOT NULL AND julianday(lease_until)<=julianday('now'))))",vec![text(&job.job_id),job.execution_token.as_deref().map_or(BindValue::Null,text),job.lease_until.as_deref().map_or(BindValue::Null,text),job.checkpoint_json.as_deref().map_or(BindValue::Null,text)])}
    fn revoke_terminal(&self,job:&JobRow)->Result<D1PreparedStatement>{self.prepare("UPDATE worker_jobs SET execution_token=NULL,lease_until=NULL WHERE job_id=?1 AND execution_token IS ?2 AND lease_until IS ?3",vec![text(&job.job_id),job.execution_token.as_deref().map_or(BindValue::Null,text),job.lease_until.as_deref().map_or(BindValue::Null,text)])}
    fn release_numeric_job(&self,reservation:&NovelIdReservation,job_id:&str)->Result<D1PreparedStatement>{self.prepare("UPDATE novel_id_reservations SET available=1,job_id=NULL WHERE id=?1 AND generation=?2 AND available=0 AND job_id=?3 AND NOT EXISTS(SELECT 1 FROM novels WHERE id=?1) AND NOT EXISTS(SELECT 1 FROM novel_namespace_claims WHERE novel_id=?1 AND reservation_generation=?2)",vec![BindValue::Int(reservation.id().0),BindValue::Int(reservation.generation()),text(job_id)])}
    pub(super) async fn release_creation(&self,reservation:NovelIdReservation)->Result<()>{
        exact(reservation.id().0)?;exact(reservation.generation())?;let primary=self.primary();let id=reservation.id();let generation=reservation.generation();
        let numeric:Option<Numeric>=primary.prepare("SELECT id,generation,job_id,available FROM novel_id_reservations WHERE id=? AND generation=?",vec![BindValue::Int(id.0),BindValue::Int(generation)])?.first(None).await.map_err(worker_error)?;
        if numeric.as_ref().is_some_and(|row|row.available!=0){return Ok(());}let receipt=primary.receipt(id,generation).await?;let claim=primary.creation_claim(id,generation).await?;
        let job_id=numeric.as_ref().and_then(|row|row.job_id.as_deref()).or_else(||receipt.as_ref().map(|row|row.job_id.as_str())).or_else(||claim.as_ref().and_then(|row|row.job_id.as_deref()));
        let Some(job_id)=job_id else{
            if numeric.is_none(){return Ok(());}primary.batch_checked(vec![primary.guard("NOT EXISTS(SELECT 1 FROM novel_namespace_claims WHERE novel_id=?1) AND NOT EXISTS(SELECT 1 FROM worker_jobs WHERE json_valid(checkpoint_json) AND json_extract(checkpoint_json,'$.creation_reservation[0]')=?1 AND json_extract(checkpoint_json,'$.creation_reservation[1]')=?2)",vec![BindValue::Int(id.0),BindValue::Int(generation)])?,primary.prepare(novel_query::RELEASE_ID_SQL,vec![BindValue::Int(id.0),BindValue::Int(generation)])?]).await?;return Ok(());
        };
        let job:JobRow=primary.prepare("SELECT job_id,status,execution_token,lease_until,checkpoint_json FROM worker_jobs WHERE job_id=?",vec![text(job_id)])?.first(None).await.map_err(worker_error)?.ok_or_else(||NarouError::Platform("creation job authority missing".into()))?;
        if !matches!(job.status.as_str(),"succeeded"|"partial"|"blocked"|"permanent"){return Err(NarouError::Platform("job still owns creation reservation".into()));}
        let pair=checkpoint_pair(job.checkpoint_json.as_deref(),job_id)?;
        if pair!=Some((id,generation))&&!(pair.is_none()&&receipt.is_some()&&job.execution_token.is_none()){return Err(NarouError::Platform("creation checkpoint ownership changed".into()));}
        if let Some(receipt)=receipt.as_ref(){
            if receipt.job_id!=job_id{return Err(NarouError::Platform("published receipt belongs to another job".into()));}
            let mut statements=vec![primary.terminal_job_guard(&job)?,primary.revoke_terminal(&job)?];
            if job.execution_token.is_some(){statements.push(primary.prepare("UPDATE novel_access_gates SET epoch=epoch+1,updated_at=unixepoch() WHERE prefix=?1 AND epoch=?2 AND closed=0 AND epoch<9007199254740991",vec![text(&receipt.prefix),BindValue::Int(receipt.epoch)])?);}
            primary.batch_checked(statements).await?;return Ok(());
        }
        let Some(claim)=claim.as_ref() else{primary.batch_checked(vec![primary.terminal_job_guard(&job)?,primary.revoke_terminal(&job)?,primary.release_numeric_job(&reservation,job_id)?]).await?;return Ok(());};
        if claim.novel_id!=id.0||claim.reservation_generation!=generation||claim.job_id.as_deref()!=Some(job_id)||claim.owner!=claim_owner(&reservation){return Err(NarouError::Platform("creation namespace authority changed".into()));}
        let origin=primary.origin(&claim.prefix).await?;let next=claim.epoch.checked_add(1).filter(|epoch|*epoch<=EXACT_MAX).ok_or_else(||NarouError::Platform("creation epoch exhausted".into()))?;
        if origin.closed!=0&&origin.epoch==next{
            let owner=origin.owner.as_deref().ok_or_else(||NarouError::Platform("creation cleanup owner missing".into()))?;let parsed=narou_rs::platform::access_fence::parse_removal_owner(owner)?;
            if parsed!=(id,true)||claim.empty_owned!=1{return Err(NarouError::Platform("creation namespace is held by another owner".into()));}primary.batch_checked(vec![primary.terminal_job_guard(&job)?,primary.revoke_terminal(&job)?]).await?;return Err(NarouError::CreationCleanupPending);
        }
        let mut statements=vec![primary.terminal_job_guard(&job)?,primary.guard("NOT EXISTS(SELECT 1 FROM novels WHERE id=?1) AND EXISTS(SELECT 1 FROM novel_id_reservations WHERE id=?1 AND generation=?2 AND available=0 AND job_id=?3)",vec![BindValue::Int(id.0),BindValue::Int(generation),text(job_id)])?,primary.revoke_terminal(&job)?];
        if origin.closed==0&&origin.epoch==claim.epoch{
            if origin.origin_url_fold!=claim.toc_url_fold{return Err(NarouError::Platform("creation retained origin changed".into()));}
            if claim.empty_owned==1{
                let owner=narou_rs::platform::access_fence::new_removal_owner(id,true)?;statements.push(primary.prepare("UPDATE novel_access_gates SET closed=1,owner=?1,epoch=?2,updated_at=unixepoch() WHERE prefix=?3 AND closed=0 AND epoch=?4 AND origin_url_fold=?5",vec![text(&owner),BindValue::Int(next),text(&claim.prefix),BindValue::Int(claim.epoch),text(&claim.toc_url_fold)])?);statements.push(primary.guard("changes()=1",vec![])?);primary.batch_checked(statements).await?;return Err(NarouError::CreationCleanupPending);
            }
            statements.push(primary.prepare("UPDATE novel_access_gates SET epoch=?1,updated_at=unixepoch() WHERE prefix=?2 AND closed=0 AND epoch=?3 AND origin_url_fold=?4",vec![BindValue::Int(next),text(&claim.prefix),BindValue::Int(claim.epoch),text(&claim.toc_url_fold)])?);statements.push(primary.guard("changes()=1",vec![])?);
        }else if origin.closed==0&&origin.epoch==next{
            if claim.empty_owned==1&&!origin.origin_url_fold.is_empty(){return Err(NarouError::Platform("creation cleanup has not cleared its origin".into()));}
            statements.push(primary.guard("EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix=?1 AND epoch=?2 AND closed=0 AND origin_url_fold=?3)",vec![text(&claim.prefix),BindValue::Int(next),text(&origin.origin_url_fold)])?);
        }else{return Err(NarouError::Platform("creation namespace epoch was superseded".into()));}
        statements.push(primary.prepare("DELETE FROM novel_namespace_claims WHERE novel_id=?1 AND reservation_generation=?2 AND owner=?3 AND job_id=?4",vec![BindValue::Int(id.0),BindValue::Int(generation),text(&claim.owner),text(job_id)])?);statements.push(primary.guard("changes()=1",vec![])?);statements.push(primary.release_numeric_job(&reservation,job_id)?);statements.push(primary.guard("changes()=1",vec![])?);primary.batch_checked(statements).await?;Ok(())
    }
}
