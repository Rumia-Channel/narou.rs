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
pub mod cookie_store;
pub mod http;
pub mod mocks;
pub mod object_store;
pub mod progress;
pub mod rate_limiter;
pub mod repository;
pub mod s3_request;
pub mod s3_sigv4;
pub mod split_store;
pub mod store_migration;
pub mod url_policy;

pub use clock::{Clock, SystemClock};
pub use cookie_store::{
    CookieStore, LoginCredential, apply_set_cookie, assign_credential_ids, cookie_host_for_url,
    cookie_lookup_hosts, credential_was_sent, decode_credentials, decode_stored_credentials,
    encode_credentials, format_cookie_header, mask_cookie, merge_cookie_headers,
    merge_credentials_for, merge_stored_cookies, normalize_cookie_host, parse_cookie_header,
    tidy_credentials,
};
pub use http::{HttpClient, HttpMethod, HttpRequest, HttpResponse, RedirectMode};
pub use object_store::{
    prefix_upper_bound,
    AssetChunk, AssetStore, AssetStream, GeneratedAssetKey, NovelObjectKeys, ObjectEncoding,
    ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata, ObjectPrefix, ObjectStore,
    compress_object_payload, content_type_for_key, decompress_object_payload, object_crc32,
    verify_object_crc32,
};
pub use progress::ProgressReporter;
pub use rate_limiter::{normalize_site_key, RateLimitScope, RateLimiter};
pub use split_store::{SplitStore, is_illustration_key};
pub use store_migration::{StoreMigrationState, migrate_page};
pub use url_policy::{is_safe_public_ip, validate_url_syntax};
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
