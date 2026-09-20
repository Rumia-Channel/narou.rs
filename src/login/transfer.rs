//! Portable login export.
//!
//! An export is a small YAML document that carries the cookies of one or more
//! hosts from the machine with the browser to the machine that downloads.
//! With a passphrase the payload is encrypted (Argon2id → XChaCha20-Poly1305);
//! without one the cookies are written in clear and the envelope says so, so an
//! import never silently trusts a file whose secrecy was assumed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{NarouError, Result};
use crate::login::crypto::{
    ENVELOPE_AAD, SALT_LEN, decrypt_with_key, derive_key, encrypt_with_key, random_bytes,
};

/// Envelope format version understood by this build.
pub const EXPORT_VERSION: u32 = 1;
/// `kdf` value written for encrypted envelopes.
pub const KDF_ARGON2ID: &str = "argon2id";

fn login_error(message: impl Into<String>) -> NarouError {
    NarouError::Login(message.into())
}

/// A portable set of login cookies.
///
/// Exactly one of `payload` (encrypted) and `cookies` (clear text) is filled;
/// `encrypted` says which one to expect.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CookieEnvelope {
    /// Format version; an unknown value is rejected on import.
    pub version: u32,
    /// RFC 3339 timestamp of the export.
    pub exported_at: String,
    /// Library the cookies came from, for the operator's benefit only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<String>,
    /// Whether `payload` holds an encrypted cookie map.
    pub encrypted: bool,
    /// Key derivation function of `payload`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf: Option<String>,
    /// Base64 Argon2id salt of `payload`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salt: Option<String>,
    /// Encrypted cookie map (`nonce:payload`, base64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    /// Clear-text cookie map, only for `encrypted: false`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cookies: BTreeMap<String, String>,
}

/// Render `cookies` as an export document.
///
/// A passphrase encrypts the payload; without one the cookies are written in
/// clear (the CLI asks for confirmation before doing that).
pub fn build_export(
    cookies: &BTreeMap<String, String>,
    passphrase: Option<&str>,
    exported_at: &str,
    library: Option<&str>,
) -> Result<String> {
    let envelope = match passphrase {
        Some(passphrase) => {
            let salt: [u8; SALT_LEN] = random_bytes()?;
            let key = derive_key(passphrase, &salt)?;
            let plaintext = serde_json::to_string(cookies).map_err(|error| {
                login_error(format!("could not serialize the cookie map: {error}"))
            })?;
            let payload = encrypt_with_key(&key, ENVELOPE_AAD, &plaintext)?;
            CookieEnvelope {
                version: EXPORT_VERSION,
                exported_at: exported_at.to_string(),
                library: library.map(str::to_string),
                encrypted: true,
                kdf: Some(KDF_ARGON2ID.to_string()),
                salt: Some(base64_encode(&salt)),
                payload: Some(payload),
                cookies: BTreeMap::new(),
            }
        }
        None => CookieEnvelope {
            version: EXPORT_VERSION,
            exported_at: exported_at.to_string(),
            library: library.map(str::to_string),
            encrypted: false,
            kdf: None,
            salt: None,
            payload: None,
            cookies: cookies.clone(),
        },
    };
    serde_yaml::to_string(&envelope).map_err(NarouError::from)
}

/// Read an export document back into a cookie map.
///
/// `passphrase` is required for an encrypted envelope and ignored otherwise.
pub fn parse_export(text: &str, passphrase: Option<&str>) -> Result<BTreeMap<String, String>> {
    let envelope: CookieEnvelope = serde_yaml::from_str(text)?;
    if envelope.version != EXPORT_VERSION {
        return Err(login_error(format!(
            "unsupported export version {} (this build reads version {})",
            envelope.version, EXPORT_VERSION
        )));
    }
    if !envelope.encrypted {
        return Ok(envelope.cookies);
    }
    let passphrase = passphrase.ok_or_else(|| {
        login_error("this export is encrypted: pass a passphrase to import it")
    })?;
    let salt = envelope
        .salt
        .as_deref()
        .ok_or_else(|| login_error("encrypted export without a salt"))?;
    if let Some(kdf) = envelope.kdf.as_deref()
        && kdf != KDF_ARGON2ID
    {
        return Err(login_error(format!("unsupported key derivation: {kdf}")));
    }
    let payload = envelope
        .payload
        .as_deref()
        .ok_or_else(|| login_error("encrypted export without a payload"))?;
    let salt_bytes = base64_decode(salt)?;
    let key = derive_key(passphrase, &salt_bytes)?;
    let plaintext = decrypt_with_key(&key, ENVELOPE_AAD, payload)?;
    serde_json::from_str(&plaintext).map_err(|error| {
        login_error(format!("decrypted payload is not a cookie map: {error}"))
    })
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(text: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .map_err(|error| login_error(format!("malformed base64 value: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookies() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("ncode.syosetu.com".to_string(), "over18=yes; ses=1".to_string()),
            ("example.com".to_string(), "sid=abc".to_string()),
        ])
    }

    #[test]
    fn plain_envelope_round_trips() {
        let text = build_export(&cookies(), None, "2026-09-20T00:00:00+09:00", Some("lib")).unwrap();
        assert!(text.contains("encrypted: false"));
        assert!(!text.contains("payload"));
        assert_eq!(parse_export(&text, None).unwrap(), cookies());
    }

    #[test]
    fn encrypted_envelope_hides_the_cookies_and_needs_the_passphrase() {
        let text = build_export(&cookies(), Some("hunter2"), "2026-09-20T00:00:00+09:00", None)
            .unwrap();
        assert!(text.contains("encrypted: true"));
        assert!(text.contains("kdf: argon2id"));
        assert!(!text.contains("sid=abc"));
        assert_eq!(parse_export(&text, Some("hunter2")).unwrap(), cookies());
        assert!(parse_export(&text, Some("wrong")).is_err());
        assert!(parse_export(&text, None).is_err());
    }

    #[test]
    fn rejects_unknown_versions_and_damaged_payloads() {
        let text = build_export(&cookies(), Some("hunter2"), "2026-09-20T00:00:00+09:00", None)
            .unwrap();
        let bumped = text.replace("version: 1", "version: 99");
        assert!(parse_export(&bumped, Some("hunter2")).is_err());

        let damaged = text.replace("payload: ", "payload: AAAA");
        assert!(parse_export(&damaged, Some("hunter2")).is_err());
    }
}
