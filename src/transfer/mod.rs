//! Platform-neutral archive format for the v0.5.0 download/upload feature.
//!
//! Runtime-independent streaming codecs and validated import plans:
//!
//! * [`ArchiveStream`] and [`AsyncArchiveStream`] encode entries incrementally.
//! * [`StagedArchive`] verifies every entry into private durable storage before
//!   callers apply [`plan_manifest`] and publish any replacement objects.
//! * [`validate_archive`] verifies an immutable range-readable source without
//!   retaining its file bodies (used by the browser WASM bindings).
//! * [`build_archive`] / [`parse_archive`] are explicit whole-value utilities;
//!   production transfer routes use the streaming interfaces instead.
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
//! Streaming writers preserve caller order and use fixed ZIP timestamps.
//! The whole-value builder puts members before parents, then sorts by URL/title.
//! Both codecs use portable Deflate and the same manifest/integrity contract.

mod archive;
mod attribute;
mod error;
mod import;
mod request;
mod stream;
mod async_stream;
mod staged;
mod manifest_json;
mod memory;
mod manifest_write;
mod structure;

pub use archive::{
    ArchiveFile, ArchiveManifest, ArchiveNovel, ParsedArchive, ParsedNovel, build_archive,
    parse_archive,
};
pub use error::{Result, TransferError};
pub use import::{
    ConflictPolicy, ExistingNovel, ImportAction, ImportPlan, ImportPlanEntry, plan_import, plan_manifest,
};
pub use request::{flag_value, import_payload, is_zip_content_type, parse_ids};
pub use stream::ArchiveStream;
pub use async_stream::AsyncArchiveStream;
pub use staged::{StagedArchive, StagedNovel, bounded_asset_stream, validate_archive};
pub use manifest_json::read_manifest;
pub use manifest_write::write as write_manifest_json;
pub use structure::{VIRTUAL_INPUT_FILES, is_virtual_input_file};

/// Version written to [`ArchiveManifest::format_version`].
pub const FORMAT_VERSION: u32 = 2;

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

/// Wasm-only working-metadata admission limit: decoded/retained manifests,
/// parser nesting, export path indices, and ZIP filename/record allocations.
/// Charges capacity, allocation overhead, and old/new storage during growth.
/// Native keeps the serialized/count limits only; fixed I/O buffers are separate.
pub const MAX_MANIFEST_METADATA_BYTES: usize = 24 * 1024 * 1024;

/// Maximum number of ZIP entries (manifest and all novel files).
pub const MAX_ENTRIES: usize = 50_000;

/// Maximum number of novels in one archive.
pub const MAX_NOVELS: usize = 10_000;
