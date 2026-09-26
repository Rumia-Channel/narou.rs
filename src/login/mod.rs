//! Login credentials: at-rest encryption and portable transfer.
//!
//! `narou_rs_login` runs on the machine that has a browser and writes the
//! captured cookies into a portable envelope. The download host imports that
//! envelope through the CLI (`login import`) or the Web UI, and stores the
//! credentials encrypted at rest in the library inventory.
//!
//! Two keys are involved and they never mix:
//!
//! * the *transfer* key is derived from a passphrase (Argon2id) and protects an
//!   export file while it travels between machines;
//! * the *library* key (`.narou/login.key`, or `NAROU_RS_LOGIN_KEY`) protects
//!   the inventory values on the download host.
//!
//! Both use XChaCha20-Poly1305 with the host name (or a fixed label for the
//! envelope) as associated data, so a ciphertext cannot be moved to another
//! host without failing authentication.

pub mod crypto;
pub mod transfer;

pub use crypto::{
    AT_REST_PREFIX, KEY_LEN, decrypt_at_rest, decrypt_stored_value, decrypt_with_key, derive_key,
    encrypt_at_rest, encrypt_with_key, is_encrypted_at_rest, new_credential_id, parse_key_base64,
    random_bytes, random_key,
};
pub use transfer::{
    CookieEnvelope, EXPORT_VERSION, build_export, group_credentials, parse_export,
};
