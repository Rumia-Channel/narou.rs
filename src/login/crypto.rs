//! XChaCha20-Poly1305 helpers shared by the login executable and the host.
//!
//! Values encrypted *at rest* carry the [`AT_REST_PREFIX`] marker so a
//! plaintext value written by an older build is still readable; it is
//! re-encrypted the next time the store writes that host.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

use crate::error::{NarouError, Result};

/// Length of a login key (at-rest or envelope).
pub const KEY_LEN: usize = 32;
/// Length of the Argon2id salt stored in an encrypted envelope.
pub const SALT_LEN: usize = 16;
/// Length of the XChaCha20 nonce.
pub const NONCE_LEN: usize = 24;
/// Marker of an inventory value encrypted at rest.
pub const AT_REST_PREFIX: &str = "enc:v1:";

/// Associated data label for values encrypted at rest.
const AT_REST_AAD: &str = "narou.rs/login-cookie";
/// Associated data label for a portable export envelope (native transfer only).
#[cfg(feature = "native-runtime")]
pub(crate) const ENVELOPE_AAD: &str = "narou.rs/login-export";

fn login_error(message: impl Into<String>) -> NarouError {
    NarouError::Login(message.into())
}

/// Fill `N` bytes from the operating system random source.
/// Identifier for one stored login credential (UUID v4 shape, lowercase hex).
///
/// Random rather than sequential so ids stay unique after exports move between
/// machines. Generated wherever a credential is first stored.
pub fn new_credential_id() -> Result<String> {
    let mut bytes: [u8; 16] = random_bytes()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    Ok(out)
}

pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| login_error(format!("random source unavailable: {error}")))?;
    Ok(bytes)
}

/// A fresh login key.
pub fn random_key() -> Result<[u8; KEY_LEN]> {
    random_bytes()
}

fn cipher(key: &[u8; KEY_LEN]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(&Key::from(*key))
}

/// Encrypt `plaintext`, binding `aad` into the authentication tag.
///
/// The result is `base64(nonce):base64(ciphertext)`, safe to embed in YAML,
/// JSON or a CLI argument.
pub fn encrypt_with_key(key: &[u8; KEY_LEN], aad: &str, plaintext: &str) -> Result<String> {
    let nonce_bytes: [u8; NONCE_LEN] = random_bytes()?;
    let nonce = XNonce::from(nonce_bytes);
    let ciphertext = cipher(key)
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext.as_bytes(),
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| login_error("encryption failed"))?;
    Ok(format!(
        "{}:{}",
        BASE64.encode(nonce_bytes),
        BASE64.encode(ciphertext)
    ))
}

/// Reverse [`encrypt_with_key`]. A wrong key, a tampered ciphertext or a
/// mismatched `aad` all fail here.
pub fn decrypt_with_key(key: &[u8; KEY_LEN], aad: &str, token: &str) -> Result<String> {
    let (nonce_part, ciphertext_part) = token
        .split_once(':')
        .ok_or_else(|| login_error("malformed ciphertext: expected nonce:payload"))?;
    let nonce_bytes = decode_exact::<NONCE_LEN>(nonce_part, "nonce")?;
    let ciphertext = BASE64
        .decode(ciphertext_part.trim())
        .map_err(|error| login_error(format!("malformed ciphertext payload: {error}")))?;
    let plaintext = cipher(key)
        .decrypt(
            &XNonce::from(nonce_bytes),
            Payload {
                msg: ciphertext.as_ref(),
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| login_error("decryption failed: wrong key or damaged data"))?;
    String::from_utf8(plaintext)
        .map_err(|error| login_error(format!("decrypted value is not text: {error}")))
}

fn decode_exact<const N: usize>(encoded: &str, what: &str) -> Result<[u8; N]> {
    let decoded = BASE64
        .decode(encoded.trim())
        .map_err(|error| login_error(format!("malformed {what}: {error}")))?;
    decoded
        .as_slice()
        .try_into()
        .map_err(|_| login_error(format!("{what} has the wrong length")))
}

/// Parse a base64-encoded login key (the `login.key` file / `NAROU_RS_LOGIN_KEY`
/// form). Portable: the Worker reads the same key from a secret.
pub fn parse_key_base64(text: &str) -> Result<[u8; KEY_LEN]> {
    let decoded = BASE64
        .decode(text.trim())
        .map_err(|error| login_error(format!("malformed login key: {error}")))?;
    decoded
        .as_slice()
        .try_into()
        .map_err(|_| login_error(format!("login key must be {KEY_LEN} bytes")))
}

/// Derive a key from a passphrase with Argon2id (19 MiB, t=2, p=1).
#[cfg(feature = "native-runtime")]
pub fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; KEY_LEN]> {
    use argon2::{Algorithm, Argon2, Params, Version};

    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::DEFAULT);
    let mut key = [0u8; KEY_LEN];
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|error| login_error(format!("key derivation failed: {error}")))?;
    Ok(key)
}

/// Whether an inventory value is encrypted at rest.
pub fn is_encrypted_at_rest(value: &str) -> bool {
    value.starts_with(AT_REST_PREFIX)
}

/// Encrypt one host's cookie header for storage in the inventory.
pub fn encrypt_at_rest(key: &[u8; KEY_LEN], host: &str, cookie: &str) -> Result<String> {
    let token = encrypt_with_key(key, &at_rest_aad(host), cookie)?;
    Ok(format!("{AT_REST_PREFIX}{token}"))
}

/// Decrypt one host's stored value.
///
/// Returns `None` for a plaintext value written by an older build, so the
/// caller can keep using it and re-encrypt on the next write.
pub fn decrypt_at_rest(key: &[u8; KEY_LEN], host: &str, value: &str) -> Result<Option<String>> {
    if !is_encrypted_at_rest(value) {
        return Ok(None);
    }
    let token = &value[AT_REST_PREFIX.len()..];
    decrypt_with_key(key, &at_rest_aad(host), token).map(Some)
}

/// Decrypt a stored value with an optional key.
///
/// Plaintext (written by an older build) returns `None` so the caller keeps
/// using it; an encrypted value without a key is an error rather than a silent
/// "no credentials".
pub fn decrypt_stored_value(
    key: Option<&[u8; KEY_LEN]>,
    host: &str,
    value: &str,
) -> Result<Option<String>> {
    if !is_encrypted_at_rest(value) {
        return Ok(None);
    }
    let Some(key) = key else {
        return Err(login_error(format!(
            "stored credentials for {host} are encrypted but no login key is configured"
        )));
    };
    decrypt_at_rest(key, host, value)
}

fn at_rest_aad(host: &str) -> String {
    format!("{AT_REST_AAD}:{host}")
}

#[cfg(all(test, feature = "native-runtime"))]
mod tests {
    use super::*;

    fn key() -> [u8; KEY_LEN] {
        [7u8; KEY_LEN]
    }

    #[test]
    fn round_trips_a_value() {
        let token = encrypt_with_key(&key(), "example.com", "session=abc").unwrap();
        assert_eq!(
            decrypt_with_key(&key(), "example.com", &token).unwrap(),
            "session=abc"
        );
    }

    #[test]
    fn aad_binds_the_ciphertext_to_its_host() {
        let token = encrypt_with_key(&key(), "example.com", "session=abc").unwrap();
        assert!(decrypt_with_key(&key(), "other.example", &token).is_err());
    }

    #[test]
    fn rejects_a_wrong_key_and_damaged_payload() {
        let token = encrypt_with_key(&key(), "example.com", "session=abc").unwrap();
        assert!(decrypt_with_key(&[9u8; KEY_LEN], "example.com", &token).is_err());
        assert!(decrypt_with_key(&key(), "example.com", "not-a-token").is_err());
        let mut damaged = token.clone();
        damaged.pop();
        assert!(decrypt_with_key(&key(), "example.com", &damaged).is_err());
    }

    #[test]
    fn nonces_differ_between_encryptions() {
        let first = encrypt_with_key(&key(), "example.com", "session=abc").unwrap();
        let second = encrypt_with_key(&key(), "example.com", "session=abc").unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn at_rest_values_carry_the_prefix_and_plaintext_stays_readable() {
        let stored = encrypt_at_rest(&key(), "example.com", "session=abc").unwrap();
        assert!(is_encrypted_at_rest(&stored));
        assert_eq!(
            decrypt_at_rest(&key(), "example.com", &stored).unwrap(),
            Some("session=abc".to_string())
        );
        assert_eq!(decrypt_at_rest(&key(), "example.com", "session=legacy").unwrap(), None);
    }

    #[test]
    fn derived_keys_depend_on_the_salt_and_passphrase() {
        let salt = [1u8; SALT_LEN];
        let other_salt = [2u8; SALT_LEN];
        let first = derive_key("correct horse", &salt).unwrap();
        assert_eq!(first, derive_key("correct horse", &salt).unwrap());
        assert_ne!(first, derive_key("correct horse", &other_salt).unwrap());
        assert_ne!(first, derive_key("battery staple", &salt).unwrap());
    }
}
