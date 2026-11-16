//! Platform-neutral archive format and wire protocol for novel transfer.
//!
//! Runtime-independent streaming codecs, validated archive handles, and the
//! v2 wire contract shared by Native, Workers and the browser WASM worker:
//!
//! * [`ArchiveStream`] and [`AsyncArchiveStream`] encode entries incrementally
//!   with ZIP64 support and awaited 64 KiB I/O.
//! * [`ValidatedArchive`] indexes an immutable range-readable source, verifies
//!   every entry, and streams individual entries with resume-prefix checks.
//! * [`protocol`] carries the `/api/transfer/v2` wire DTOs, binary batch
//!   framing and the bounded NDJSON wire-manifest codecs.
//! * [`projection`] classifies every file into a [`FileRole`](projection::FileRole)
//!   and applies the `runtime` / `complete` selection rules.
//! * [`StagedArchive`] verifies a whole source into private durable storage;
//!   it is only compiled for runtimes with platform stores.
//! * [`build_archive`] / [`parse_archive`] are explicit whole-value utilities
//!   with their own finite budgets; production transfer routes never use them.
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
mod manifest_json;
mod manifest_write;
mod manifest_pull;
mod memory;
pub mod projection;
pub mod protocol;
mod stream;
mod async_stream;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
mod staged;
mod structure;
mod validated;

pub use archive::{
    ArchiveFile, ArchiveManifest, ArchiveNovel, ParsedArchive, ParsedNovel, build_archive,
    parse_archive, validate_relative_path,
};
pub use error::{Result, TransferError};
pub use import::{
    ConflictPolicy, ExistingNovel, ImportAction, ImportPlan, ImportPlanEntry, plan_import, plan_manifest,
};
pub use stream::ArchiveStream;
pub use async_stream::AsyncArchiveStream;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub use staged::{StagedArchive, StagedNovel, bounded_asset_stream};
pub use validated::{
    ArchiveLimits, CatalogItem, EntryMatch, StreamedEntry, ValidatedArchive, WireSummary,
    source_fingerprint, validate_archive,
};
pub use manifest_json::read_manifest;
pub use manifest_write::{NovelJsonCursor, novel_json_chunk, write as write_manifest_json};
pub use manifest_pull::{NovelJsonLayout, NovelJsonRequest, NovelJsonText, NovelJsonOutput, NovelJsonPullCursor, novel_json_pull};
pub use structure::{VIRTUAL_INPUT_FILES, is_virtual_input_file};

/// Version written to [`ArchiveManifest::format_version`].
pub const FORMAT_VERSION: u32 = 2;

/// Root manifest entry name.
pub const MANIFEST_PATH: &str = "manifest.json";

/// Maximum serialized size of `manifest.json` inside a ZIP archive.
///
/// This is a metadata budget, not a file-body limit; the wire manifest has a
/// separate serialized budget in [`protocol::WIRE_MANIFEST_MAX_BYTES`].
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

/// Whole-value utility budget for [`parse_archive`]'s input buffer.
///
/// Streaming codecs have **no** archive byte cap — the old fixed 512 MiB
/// limit applied to sequential paths was removed; archives are bounded by
/// entry/manifest counts, the metadata budget and the destination's storage.
/// Whole-value helpers keep their own finite budget so a `Vec<u8>` input can
/// never amplify into unbounded work.
pub const UTILITY_ARCHIVE_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

/// Whole-value utility budget for the decoded contents [`parse_archive`]
/// returns in memory (sum of uncompressed entries, manifest included).
pub const UTILITY_TOTAL_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

/// Whole-value utility budget for one decoded file inside [`ParsedArchive`].
pub const UTILITY_FILE_BUDGET_BYTES: u64 = 256 * 1024 * 1024;
