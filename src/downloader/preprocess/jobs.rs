//! Results of the URLs a `preprocess:` run asked for.
//!
//! A site definition can request extra URLs from the DSL (`request("…")`).
//! The requests are collected during a run, executed by the async side through
//! the site's fetch policy, and their parsed bodies become visible to the next
//! run as `fetched["<url>"]`.
//!
//! One instance lives for a whole novel download. Settled JSON responses are
//! reused only within the same credential/header context and a bounded FIFO;
//! evicted responses can be fetched again. Original JSON text stays in memory
//! so reusing it for a section does not change user-defined regex behavior.
//!
//! `post_form` also requests raw text. Its identity includes the URL, encoded
//! body and headers; these results live for one preprocessing call only.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};
use crate::error::NarouError;

#[derive(Debug, thiserror::Error)]
pub enum PreprocessError {
    #[error("{0}")]
    Definition(String),
    #[error("required JSON request failed: {0}")]
    RequiredJson(String),
}

impl From<String> for PreprocessError {
    fn from(message: String) -> Self { Self::Definition(message) }
}

impl From<&str> for PreprocessError {
    fn from(message: &str) -> Self { Self::Definition(message.to_string()) }
}

/// Fetches retain their full identity: form bodies at one URL are distinct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreprocessRequest {
    Json { url: String },
    JsonRequired { url: String, missing_statuses: Vec<u16> },
    Form { url: String, body: String, headers: Vec<(String, String)>, key: String },
}

impl PreprocessRequest {
    pub fn form(url: String, body: String, headers: Vec<(String, String)>) -> Self {
        let mut hash = Sha256::new();
        for part in std::iter::once(url.as_str()).chain(std::iter::once(body.as_str()))
            .chain(headers.iter().flat_map(|(name, value)| [name.as_str(), value.as_str()]))
        {
            hash.update((part.len() as u64).to_le_bytes());
            hash.update(part.as_bytes());
        }
        let key = format!("post-form:{}", hex::encode(hash.finalize()));
        Self::Form { url, body, headers, key }
    }

    pub fn url(&self) -> &str {
        match self { Self::Json { url } | Self::JsonRequired { url, .. } | Self::Form { url, .. } => url }
    }

    pub fn key(&self) -> &str {
        match self { Self::Json { url } | Self::JsonRequired { url, .. } => url, Self::Form { key, .. } => key }
    }
}

/// Maximum settled JSON results retained in FIFO order. Completed pages may
/// evict older responses, but a successful fetch must never be discarded.
pub const MAX_PREPROCESS_JOBS: usize = 256;

#[derive(Debug)]
struct CachedJson {
    value: Value,
    source: Option<String>,
    failure: Option<(Option<u16>, Arc<NarouError>)>,
}

#[derive(Debug, Default)]
pub struct PreprocessJobs {
    fetched: HashMap<String, CachedJson>,
    order: VecDeque<String>,
    context: Option<[u8; 32]>,
    has_cookie: bool,
    /// Raw form responses live only for one preprocessing call, not a novel.
    forms: HashMap<String, Value>,
}

impl PreprocessJobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every result, e.g. when a different novel starts downloading.
    pub fn reset(&mut self) {
        self.fetched.clear();
        self.forms.clear();
        self.order.clear();
        self.context = None;
        self.has_cookie = false;
    }

    pub(crate) fn bind_policy(&mut self, policy: &super::super::http_policy::FetchPolicy, url: &str) {
        let context = policy.cache_context();
        if context.is_none() || self.context != context {
            self.reset();
            self.context = context;
        }
        self.has_cookie = policy.has_cookie_for(url);
    }

    pub(super) fn has_cookie(&self) -> bool { self.has_cookie }

    pub(super) fn validate_required(&self, url: &str, missing_statuses: &[u16]) -> Result<(), PreprocessError> {
        let Some(result) = self.fetched.get(url) else { return Ok(()) };
        if let Some((status, _)) = &result.failure {
            if status.is_some_and(|status| missing_statuses.contains(&status)) {
                return Ok(());
            }
            return Err(PreprocessError::RequiredJson(url.to_string()));
        }
        if result.value.is_null() {
            return Err(PreprocessError::RequiredJson(url.to_string()));
        }
        Ok(())
    }

    pub(crate) fn failure(&self, url: &str) -> Option<&Arc<NarouError>> {
        self.fetched.get(url).and_then(|result| result.failure.as_ref().map(|(_, error)| error))
    }

    pub(crate) fn insert_failure(&mut self, url: String, status: Option<u16>, error: NarouError) {
        self.insert_result(url, CachedJson {
            value: Value::Null, source: None, failure: Some((status, Arc::new(error))),
        });
    }

    pub(crate) fn insert_json(&mut self, url: String, value: Value, source: String) {
        self.insert_result(url, CachedJson { value, source: Some(source), failure: None });
    }

    pub(crate) fn json_source(&self, url: &str) -> Option<String> {
        self.fetched.get(url).filter(|result| !result.value.is_null())
            .and_then(|result| result.source.clone())
    }

    pub fn contains(&self, url: &str) -> bool {
        self.fetched.contains_key(url) || self.forms.contains_key(url)
    }

    /// Resolved results, keyed by URL, as the DSL sees them.
    pub fn results(&self) -> impl ExactSizeIterator<Item = (&String, &Value)> {
        self.fetched.iter().map(|(url, result)| (url, &result.value))
    }

    /// Record a settled job. A failed fetch is stored as `null` so the DSL can
    /// branch on it instead of the download failing.
    pub fn insert(&mut self, url: String, value: Value) {
        self.insert_result(url, CachedJson { value, source: None, failure: None });
    }

    fn insert_result(&mut self, url: String, result: CachedJson) {
        if !self.fetched.contains_key(&url) {
            while self.fetched.len() >= MAX_PREPROCESS_JOBS {
                let Some(oldest) = self.order.pop_front() else { break };
                self.fetched.remove(&oldest);
            }
            self.order.push_back(url.clone());
        }
        self.fetched.insert(url, result);
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fetched.get(key).map(|result| &result.value).or_else(|| self.forms.get(key))
    }

    pub fn insert_form(&mut self, key: String, value: Value) -> Result<(), String> {
        if !self.forms.contains_key(&key) && self.forms.len() >= MAX_PREPROCESS_JOBS {
            return Err("preprocess: form request limit exceeded".into());
        }
        self.forms.insert(key, value);
        Ok(())
    }

    pub fn clear_forms(&mut self) {
        self.forms.clear();
    }

    pub fn len(&self) -> usize {
        self.fetched.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fetched.is_empty()
    }
}
