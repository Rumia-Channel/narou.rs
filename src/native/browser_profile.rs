//! Owned token-browser profiles, including Windows sharing-violation cleanup.

use std::io;
use std::path::Path;
#[cfg(windows)]
use std::time::{Duration, Instant};

pub struct BrowserProfile {
    directory: tempfile::TempDir,
    cleanup_on_drop: bool,
}

impl BrowserProfile {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            directory: tempfile::Builder::new().prefix("narou-rs-browser-token-").tempdir()?,
            cleanup_on_drop: true,
        })
    }

    pub fn path(&self) -> &Path { self.directory.path() }

    pub fn close(mut self) -> io::Result<()> {
        self.cleanup_on_drop = false;
        remove(self.path())
    }
}

impl Drop for BrowserProfile {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = remove(self.path());
        }
    }
}

/// A terminated Chromium child can briefly retain a cache handle on Windows.
/// Retry only sharing/lock violations, not permissions or unrelated I/O errors.
#[cfg(windows)]
pub fn remove(path: &Path) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                if matches!(error.raw_os_error(), Some(32 | 33)) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                return Err(error);
            }
        }
    }
}

#[cfg(not(windows))]
pub fn remove(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
