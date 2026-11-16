//! Backend cursors and their replay result share the same primary D1 commit.
use narou_rs::application::transfer::{SessionId,TransferError};
use serde::Deserialize;
use worker::D1PreparedStatement;
use super::backend::{WorkerTransferBackend,error,invalid,statement};

#[derive(Deserialize)]
pub(super) struct BackendStep{pub input_json:String,pub result_json:Option<String>}
impl WorkerTransferBackend{
    pub(super) async fn begin_backend_step(&self,session:&SessionId,kind:&str,input:&str)->Result<BackendStep,TransferError>{
        self.journal.ensure_execution(session).await?;
        let insert=statement(&self.db,"INSERT OR IGNORE INTO transfer_backend_steps(session_id,operation_id,kind,input_json) SELECT ?,operation_id,?,? FROM transfer_execution_leases WHERE session_id=? AND owner=? AND expires_at>unixepoch()",&[session.0.as_str().into(),kind.into(),input.into(),session.0.as_str().into(),self.journal.execution_owner().into()])?;
        let result=self.db.batch(vec![self.journal.execution_guard(session)?,insert]).await.map_err(error)?;
        crate::d1_repository::ensure_batch_success(&result).map_err(error)?;
        statement(&self.db,"SELECT b.input_json,b.result_json FROM transfer_backend_steps b JOIN transfer_execution_leases l USING(session_id,operation_id) WHERE b.session_id=? AND b.kind=? AND l.owner=? AND l.expires_at>unixepoch()",&[session.0.as_str().into(),kind.into(),self.journal.execution_owner().into()])?.first(None).await.map_err(error)?.ok_or_else(||invalid("operation_busy","backend step lost its execution owner"))
    }
    pub(super) fn finish_backend_step(&self,session:&SessionId,kind:&str,result:&str)->Result<D1PreparedStatement,TransferError>{
        statement(&self.db,"UPDATE transfer_backend_steps SET result_json=? WHERE session_id=? AND kind=? AND operation_id=(SELECT operation_id FROM transfer_execution_leases WHERE session_id=? AND owner=? AND expires_at>unixepoch())",&[result.into(),session.0.as_str().into(),kind.into(),session.0.as_str().into(),self.journal.execution_owner().into()])
    }
}
