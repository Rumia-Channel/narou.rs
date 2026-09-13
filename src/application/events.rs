//! Application-owned platform ports.
//!
//! The library service needs two capabilities that the platform traits do
//! not yet cover: the set of frozen novel ids (native: the freeze inventory)
//! and the per-site timezone used for the six-hour new-arrival marker.
//! Both are expressed as platform-neutral async ports so a native adapter
//! and a future Worker adapter can both implement them.

use std::collections::HashSet;

use chrono::{DateTime, NaiveDateTime, Utc};
use chrono_tz::Tz;

use crate::platform::PlatformFuture;

/// A site timezone: either a named IANA zone or a fixed UTC offset.
///
/// Mirrors the downloader's `SiteTimezone` value type without depending on
/// the downloader module, so the application layer stays platform-neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteTimezone {
    /// IANA timezone name (e.g. `Asia/Tokyo`).
    Named(Tz),
    /// Fixed UTC offset.
    Fixed(chrono::FixedOffset),
}

impl SiteTimezone {
    /// The local wall-clock time of `dt` in this timezone.
    pub fn local_naive_datetime(self, dt: DateTime<Utc>) -> NaiveDateTime {
        match self {
            Self::Named(tz) => dt.with_timezone(&tz).naive_local(),
            Self::Fixed(offset) => dt.with_timezone(&offset).naive_local(),
        }
    }
}

/// Source of the frozen-novel id set.
///
/// Desktop: the freeze inventory (`freeze.yaml`). Worker: a freeze table.
/// The service uses this to compute per-record frozen status and to let
/// `Status`/`Any` search terms match the `凍結` status text.
pub trait FreezeStore: Send + Sync {
    /// All currently frozen novel ids.
    fn frozen_ids<'a>(&'a self) -> PlatformFuture<'a, crate::error::Result<HashSet<i64>>>;
}

/// Desktop-style freeze store backed by the freeze inventory.
///
/// This unit type reports an empty frozen set; the desktop wiring layer
/// provides the real inventory-backed implementation. It exists so the
/// service can be constructed with a concrete store in tests and in the
/// desktop wiring before the inventory-backed adapter is wired in.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemFreezeStore;

impl FreezeStore for SystemFreezeStore {
    fn frozen_ids<'a>(&'a self) -> PlatformFuture<'a, crate::error::Result<HashSet<i64>>> {
        Box::pin(async move { Ok(HashSet::new()) })
    }
}

/// Resolves the timezone for a site domain.
///
/// Desktop: the `webnovel/*.yaml` site settings. Worker: bundled site
/// definitions. The service falls back to a default timezone when a domain
/// has no entry.
pub trait SiteTimezoneProvider: Send + Sync {
    /// The timezone configured for `domain`, or `None` when unknown.
    fn timezone_for_domain<'a>(
        &'a self,
        domain: &'a str,
    ) -> PlatformFuture<'a, crate::error::Result<Option<SiteTimezone>>>;
}
/// Provides URL-derived site definition behavior without exposing the
/// filesystem-backed YAML loader to application consumers.
pub trait SiteDefinitionProvider: Send + Sync {
    /// Resolve a user URL to the canonical TOC URL used by the repository.
    fn resolve_toc_url(&self, url: &str) -> Option<String>;
    /// Return URL validation patterns from the loaded site definitions.
    fn url_patterns_for_validation(&self) -> Vec<String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EmptySiteDefinitionProvider;

impl SiteDefinitionProvider for EmptySiteDefinitionProvider {
    fn resolve_toc_url(&self, _url: &str) -> Option<String> {
        None
    }

    fn url_patterns_for_validation(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Answers site-specific update capability questions without exposing site
/// definition storage to application consumers.
pub trait SiteUpdateCapabilityProvider: Send + Sync {
    fn supports_narou_api(&self, toc_url: &str) -> bool;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EmptySiteUpdateCapabilityProvider;

impl SiteUpdateCapabilityProvider for EmptySiteUpdateCapabilityProvider {
    fn supports_narou_api(&self, _toc_url: &str) -> bool {
        false
    }
}

/// A provider that knows no site timezones; callers fall back to their
/// default. Used for construction and tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptySiteTimezoneProvider;

impl SiteTimezoneProvider for EmptySiteTimezoneProvider {
    fn timezone_for_domain<'a>(
        &'a self,
        _domain: &'a str,
    ) -> PlatformFuture<'a, crate::error::Result<Option<SiteTimezone>>> {
        Box::pin(async move { Ok(None) })
    }
}

/// A platform-neutral event emitted by a long-running application operation.
///
/// The application layer deliberately does not know about WebSockets,
/// `PushServer`, or any other presentation transport.  A native web adapter
/// can translate this event into the existing push protocol, while a Worker
/// adapter can forward it to its own event stream.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationEvent {
    /// Stable event name such as `queue_complete` or `table.reload`.
    pub name: String,
    /// Structured event payload.  Presentation adapters choose the wire
    /// representation and may ignore fields they do not support.
    pub data: serde_json::Value,
}

impl ApplicationEvent {
    pub fn new(name: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            data,
        }
    }
}

/// Receives events from long-running application operations.
///
/// This is intentionally narrower than a WebSocket or console API: producers
/// publish domain events, and the presentation layer decides how (or whether)
/// to expose them to clients.
pub trait EventSink: Send + Sync {
    fn publish<'a>(
        &'a self,
        event: ApplicationEvent,
    ) -> PlatformFuture<'a, crate::error::Result<()>>;
}

/// Event sink used by composition roots that do not expose progress events.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn publish<'a>(
        &'a self,
        _event: ApplicationEvent,
    ) -> PlatformFuture<'a, crate::error::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod event_tests {
    use parking_lot::Mutex;

    use super::*;

    #[derive(Default)]
    struct MemoryEventSink(Mutex<Vec<ApplicationEvent>>);

    impl EventSink for MemoryEventSink {
        fn publish<'a>(
            &'a self,
            event: ApplicationEvent,
        ) -> PlatformFuture<'a, crate::error::Result<()>> {
            Box::pin(async move {
                self.0.lock().push(event);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn event_sink_receives_structured_event() {
        let sink = MemoryEventSink::default();
        sink.publish(ApplicationEvent::new(
            "queue_complete",
            serde_json::json!({ "job_id": "job-1" }),
        ))
        .await
        .unwrap();

        let events = sink.0.lock();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "queue_complete");
        assert_eq!(events[0].data["job_id"], "job-1");
    }
}
