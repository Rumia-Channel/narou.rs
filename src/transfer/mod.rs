//! Platform-neutral archive format for the v0.5.0 download/upload feature.
//!
//! This module intentionally contains no filesystem, database, network or
//! native-runtime code. The CLI, the native Web API, the Worker and the
//! browser WASM wrapper all call the same three entry points:
//!
//! * [`build_archive`] serialises selected novels into a deterministic ZIP.
//! * [`parse_archive`] validates and decodes an archive produced by any client.
//! * [`plan_import`] turns a parsed archive plus the current library index into
//!   a create / overwrite / skip plan.
//!
//! # Archive layout
//!
//! ```text
//! manifest.json
//! novels/0/<path>
//! novels/1/<path>
//! ...
//! ```
//!
//! `manifest.json` is a JSON [`ArchiveManifest`]. It carries the novel metadata
//! and, for every file, its path relative to the novel directory, its size and
//! its SHA-256 digest. The ZIP entry for a file is
//! `novels/<novel-index>/<path>`, where `<novel-index>` is the position of the
//! novel in the manifest.
//!
//! Files are sorted by path while building, novels are sorted by `toc_url` and
//! title, and timestamps are fixed, so the same input always produces the same
//! bytes. Entries are deflated with the pure-Rust `zlib-rs` backend, which
//! keeps the module usable on `wasm32-unknown-unknown`.

mod archive;
mod attribute;
mod error;
mod import;

pub use archive::{
    ArchiveFile, ArchiveManifest, ArchiveNovel, ParsedArchive, ParsedNovel, build_archive,
    parse_archive,
};
pub use error::{Result, TransferError};
pub use import::{
    ConflictPolicy, ExistingNovel, ImportAction, ImportPlan, ImportPlanEntry, plan_import,
};

/// Version written to [`ArchiveManifest::format_version`].
pub const FORMAT_VERSION: u32 = 1;

/// Root manifest entry name.
pub const MANIFEST_PATH: &str = "manifest.json";

/// Maximum size of the ZIP file handed to [`parse_archive`].
pub const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

/// Maximum sum of all uncompressed entries (including the manifest).
pub const MAX_TOTAL_UNCOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;

/// Maximum uncompressed size of a single novel file.
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum uncompressed size of `manifest.json`.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum number of ZIP entries (manifest and all novel files).
pub const MAX_ENTRIES: usize = 50_000;

/// Maximum number of novels in one archive.
pub const MAX_NOVELS: usize = 10_000;
