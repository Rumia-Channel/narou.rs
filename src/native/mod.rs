//! Native platform implementations.
//!
//! These adapters implement the `platform` traits on top of OS/runtime
//! facilities (reqwest/curl/wget, `std::fs`, subprocesses). The domain layer
//! (`downloader`, `converter`, `db`, `commands`, `web`) must never reference
//! this module; only the binary entrypoints and the native constructors
//! (`Downloader::with_user_agent` etc.) wire it in.

pub mod converter;
pub mod downloader;
pub mod http;
pub mod legacy_persistence;
pub mod novel_repository;
pub mod object_store;
