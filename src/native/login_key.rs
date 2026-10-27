//! Library login key used for at-rest credential encryption.
//!
//! The key lives outside the stored data: either in the
//! `NAROU_RS_LOGIN_KEY` environment variable (base64, for servers that keep
//! secrets in a service manager) or in `.narou/login.key` next to the library.
//! It is created on first use with a random value, so an existing library keeps
//! working without any setup.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::db::inventory::Inventory;
use crate::error::Result;
use crate::login::LOGIN_KEY_ENV_VAR;
use crate::login::crypto::{KEY_LEN, parse_key_base64, random_key};

/// File holding the library login key, inside `.narou`.
pub const KEY_FILE_NAME: &str = "login.key";

/// Where a [`LoginKey`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// `NAROU_RS_LOGIN_KEY`.
    Environment,
    /// `.narou/login.key`, already present.
    File,
    /// `.narou/login.key`, created by this run.
    CreatedFile,
}

impl KeySource {
    /// Operator-facing description.
    pub fn describe(self) -> &'static str {
        match self {
            KeySource::Environment => LOGIN_KEY_ENV_VAR,
            KeySource::File => ".narou/login.key",
            KeySource::CreatedFile => ".narou/login.key (created)",
        }
    }
}

/// The library's credential key.
#[derive(Clone)]
pub struct LoginKey {
    bytes: [u8; KEY_LEN],
    source: KeySource,
}

impl LoginKey {
    /// Key for the library the current working directory belongs to.
    pub fn for_current_library() -> Result<Self> {
        let root = Inventory::with_default_root()?.root_dir().to_path_buf();
        Self::load_or_create(&root.join(".narou").join(KEY_FILE_NAME))
    }

    /// Read the key from the environment or `path`, creating the file when
    /// neither exists.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if let Some(value) = std::env::var_os(LOGIN_KEY_ENV_VAR) {
            let value = value.to_string_lossy().into_owned();
            return Ok(Self {
                bytes: parse_key_base64(&value)?,
                source: KeySource::Environment,
            });
        }
        if path.is_file() {
            let text = std::fs::read_to_string(path)?;
            return Ok(Self {
                bytes: parse_key_base64(&text)?,
                source: KeySource::File,
            });
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // `create_new` keeps two processes from overwriting each other's key:
        // the loser reads whatever the winner wrote and both agree on it.
        let bytes = random_key()?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Unix では **作る時から** 0600 にする。作成直後に chmod すると、
        // その間のファイルは umask 由来 (通常 0644) で他ユーザーから読めてしまい、
        // AEAD の鍵が露出する (鍵が漏れると保存済み Cookie を復号できる)。
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(mut file) => {
                use std::io::Write as _;
                file.write_all(BASE64.encode(bytes).as_bytes())?;
                drop(file);
                // 既存の緩いモード (旧ビルド-created ファイル) も締める。
                restrict_permissions(path)?;
                Ok(Self {
                    bytes,
                    source: KeySource::CreatedFile,
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let text = std::fs::read_to_string(path)?;
                Ok(Self {
                    bytes: parse_key_base64(&text)?,
                    source: KeySource::File,
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Wrap key bytes that were produced elsewhere.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self {
            bytes,
            source: KeySource::File,
        }
    }

    /// Raw key material.
    pub fn bytes(&self) -> &[u8; KEY_LEN] {
        &self.bytes
    }

    /// Where the key came from.
    pub fn source(&self) -> KeySource {
        self.source
    }

    /// Path the key would use inside `root`.
    pub fn default_path(root: &Path) -> PathBuf {
        root.join(".narou").join(KEY_FILE_NAME)
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    // Windows inherits the directory ACL; the key file is not world readable
    // unless the library itself is.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_the_key_file_once_and_reuses_it() {
        let dir = std::env::temp_dir().join(format!("narou-login-key-{}", std::process::id()));
        let path = dir.join("login.key");
        let _ = std::fs::remove_dir_all(&dir);

        let created = LoginKey::load_or_create(&path).unwrap();
        assert_eq!(created.source(), KeySource::CreatedFile);
        assert!(path.is_file());

        let reused = LoginKey::load_or_create(&path).unwrap();
        assert_eq!(reused.source(), KeySource::File);
        assert_eq!(created.bytes(), reused.bytes());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_a_malformed_key_file() {
        let dir = std::env::temp_dir().join(format!("narou-login-key-bad-{}", std::process::id()));
        let path = dir.join("login.key");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "not base64!").unwrap();

        assert!(LoginKey::load_or_create(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// umask を緩く (0) しても鍵ファイルが 0600 で始まること。
    ///
    /// 作成後に chmod する実装だと、宽松な umask の下では 0666 で一瞬だけ
    /// 読めるファイルになる (鍵が露出すると保存済み Cookie を復号できる)。
    #[cfg(unix)]
    #[test]
    fn the_key_file_is_created_with_owner_only_permissions() {
        // umask はプロセス全体に影響するので、他のテストと直列化する。
        let _global = crate::test_support::global_state_guard();
        let previous = unsafe { libc::umask(0) };
        let result = std::panic::catch_unwind(|| {
            let dir = std::env::temp_dir()
                .join(format!("narou-login-key-mode-{}", std::process::id()));
            let path = dir.join("login.key");
            let _ = std::fs::remove_dir_all(&dir);

            LoginKey::load_or_create(&path).unwrap();

            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            let _ = std::fs::remove_dir_all(&dir);
            mode
        });
        unsafe {
            libc::umask(previous);
        }
        assert_eq!(result.unwrap(), 0o600);
    }
}
