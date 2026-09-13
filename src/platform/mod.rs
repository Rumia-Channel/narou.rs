//! Platform abstraction layer.
//!
//! Domain logic (`downloader`, `converter`, `db`, `commands`, `web`) depends on
//! these traits; native and Cloudflare Workers implementations implement them.
//! This module must never depend on `reqwest`, `curl`, `std::fs`,
//! `std::process`, or any Cloudflare-specific type.
//!
//! Dependency direction:
//!
//! ```text
//! core → platform (traits) ← native / worker implementations
//! ```
//!
//! See `docs/platform-abstraction.md` for the full design.

pub mod clock;
pub mod http;
pub mod mocks;
pub mod object_store;
pub mod progress;
pub mod rate_limiter;
pub mod repository;

pub use clock::{Clock, SystemClock};
pub use http::{HttpClient, HttpMethod, HttpRequest, HttpResponse, RedirectMode};
pub use object_store::{
    AssetChunk, AssetStore, AssetStream, GeneratedAssetKey, NovelObjectKeys, ObjectEncoding,
    ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata, ObjectPrefix, ObjectStore,
    compress_object_payload, decompress_object_payload, object_crc32, verify_object_crc32,
};
pub use progress::ProgressReporter;
pub use rate_limiter::{normalize_site_key, RateLimitScope, RateLimiter};
pub use repository::{
    NovelFilter, NovelId, NovelMutation, NovelQuery, NovelRepository, NovelSort, NovelSortKey,
    SearchField, SearchTerm,
};

/// Target-aware future type used by every async platform trait.
///
/// Native implementations drive blocking transports through
/// `tokio::task::spawn_blocking` and parallel updates through `tokio::spawn`,
/// so the future must be `Send`. Cloudflare Workers' JS bindings are not
/// guaranteed to be `Send`, so on `wasm32` we relax the bound.
#[cfg(not(target_arch = "wasm32"))]
pub type PlatformFuture<'a, T> = futures::future::BoxFuture<'a, T>;

#[cfg(target_arch = "wasm32")]
pub type PlatformFuture<'a, T> = futures::future::LocalBoxFuture<'a, T>;

/// Marker for services that live on the platform side of the abstraction.
///
/// On native, services are shared across threads (`Send + Sync`, required by
/// `tokio::spawn` and the axum `State`); on `wasm32` every type is `Send`
/// automatically, so no bound is needed.
#[cfg(not(target_arch = "wasm32"))]
pub trait PlatformService: Send + Sync {}

#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + Sync + ?Sized> PlatformService for T {}

#[cfg(target_arch = "wasm32")]
pub trait PlatformService {}

#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> PlatformService for T {}
