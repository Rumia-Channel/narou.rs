//! Shared engine errors mapped without losing stable machine codes.
use narou_rs::application::transfer::TransferError;
use worker::Response;
pub fn response(error:&TransferError)->worker::Result<Response>{
    let status=match error{
        TransferError::SessionNotFound=>404,TransferError::SecretMismatch=>403,
        TransferError::SessionExpired{..}=>410,TransferError::PreconditionRequired=>428,
        TransferError::RevisionMismatch{..}=>412,
        TransferError::Busy{code:"memory_capacity",..}=>503,TransferError::Busy{..}=>423,
        TransferError::ExceedsLimit{..}=>413,TransferError::InsufficientStorage{..}=>507,
        TransferError::Unsupported{..}=>422,TransferError::RecoveryRequired{..}|TransferError::Platform{..}=>503,
        TransferError::InvalidRequest{code:"not_found",..}=>404,
        TransferError::InvalidRequest{code:"request_expired",..}=>410,
        TransferError::InvalidRequest{code:"unsupported_media_type"|"unsupported_content_encoding",..}=>415,
        TransferError::InvalidRequest{..}=>400,_=>409,
    };
    let mut response=Response::from_json(&serde_json::json!({"error":{"code":error.code(),"message":error.message(),"retryable":error.retryable()},"session_id":null,"revision":null}))?.with_status(status);
    response.headers_mut().set("Cache-Control","no-store, no-transform")?;
    response.headers_mut().set("X-Content-Type-Options","nosniff")?;
    if error.retryable(){response.headers_mut().set("Retry-After","5")?;}
    Ok(response)
}
