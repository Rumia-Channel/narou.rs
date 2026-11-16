//! Signed `creation_ticket`: a stateless, time-boxed nonce bound to the
//! auth scope and protocol version (design §6.1). It is not authentication —
//! it only proves the caller asked `/capabilities` recently, so a session
//! create cannot be replayed against a stale capability contract.

use base64::Engine;
use narou_rs::application::transfer::{AuthScope, TransferError, PROTOCOL_VERSION};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// HMAC input separator; the ticket payload itself is JSON-free and fixed.
const SEP: &str = "|";

fn signing_input(scope: &AuthScope, nonce: &str, issued_at: i64) -> Vec<u8> {
    format!(
        "narou-transfer-ticket{SEP}v{PROTOCOL_VERSION}{SEP}{}{SEP}{}{SEP}{nonce}{SEP}{issued_at}",
        scope.library_id, scope.principal
    )
    .into_bytes()
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = key_block.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = key_block.iter().map(|b| b ^ 0x5c).collect();
    let mut inner = ipad;
    inner.extend_from_slice(message);
    let inner_hash = Sha256::digest(&inner);
    let mut outer = opad;
    outer.extend_from_slice(&inner_hash);
    Sha256::digest(&outer).to_vec()
}

fn sign(key: &[u8], scope: &AuthScope, nonce: &str, issued_at: i64) -> String {
    hex::encode(hmac_sha256(key, &signing_input(scope, nonce, issued_at)))
}

/// Mint a ticket `nonce.issued_at.signature` bound to `scope`.
pub fn issue(key: &[u8], scope: &AuthScope, nonce: &str, issued_at: i64) -> String {
    let nonce_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce.as_bytes());
    format!("{nonce_b64}.{issued_at}.{}", sign(key, scope, nonce, issued_at))
}

/// Validate a ticket: well-formed, unexpired (caller passes the TTL) and
/// correctly signed for this scope. `request_expired` distinguishes a stale
/// ticket from a bad signature — design §6.1.
pub fn verify(
    key: &[u8],
    scope: &AuthScope,
    ticket: &str,
    now: i64,
    ttl_secs: i64,
) -> Result<(), TransferError> {
    let mut parts = ticket.split('.');
    let (Some(nonce_b64), Some(issued), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(TransferError::InvalidRequest {
            code: "invalid_request",
            message: "malformed creation_ticket".into(),
        });
    };
    let issued_at: i64 = issued.parse().map_err(|_| TransferError::InvalidRequest {
        code: "invalid_request",
        message: "malformed creation_ticket".into(),
    })?;
    let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(nonce_b64)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(|| TransferError::InvalidRequest {
            code: "invalid_request",
            message: "malformed creation_ticket".into(),
        })?;
    if issued_at.checked_add(ttl_secs).is_none_or(|expires| expires <= now) {
        return Err(TransferError::InvalidRequest {
            code: "request_expired",
            message: "creation_ticket has expired; fetch /capabilities again".into(),
        });
    }
    if issued_at > now + 60 {
        return Err(TransferError::InvalidRequest {
            code: "invalid_request",
            message: "creation_ticket issued in the future".into(),
        });
    }
    let expected = sign(key, scope, &nonce, issued_at);
    if !bool::from(expected.as_bytes().ct_eq(signature.as_bytes())) {
        return Err(TransferError::InvalidRequest {
            code: "invalid_request",
            message: "creation_ticket signature mismatch".into(),
        });
    }
    Ok(())
}
