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
pub mod rate_limiter;
pub mod repository;

pub use clock::{Clock, SystemClock};
pub use http::{HttpClient, HttpMethod, HttpRequest, HttpResponse, RedirectMode};
pub use object_store::{ObjectKey, ObjectMetadata, ObjectStore};
pub use rate_limiter::{RateLimitScope, RateLimiter};
pub use repository::{NovelId, NovelQuery, NovelRepository};
