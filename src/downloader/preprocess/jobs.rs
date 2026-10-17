//! Results of the URLs a `preprocess:` run asked for.
//!
//! A site definition can request extra URLs from the DSL (`request("…")`).
//! The requests are collected during a run, executed by the async side through
//! the site's fetch policy, and their parsed bodies become visible to the next
//! run as `fetched["<url>"]`.
//!
//! One instance lives for a whole novel download, so a series that references
//! the same illustration from many sections resolves it once. Only *settled*
//! results are kept — a job that has finished leaves the queue immediately, so
//! nothing long-lived accumulates (which matters on the Worker runtime).
//!
//! `post_form` also requests raw text. Its identity includes the URL, encoded
//! body and headers; these results live for one preprocessing call only.

use std::collections::HashMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Fetches retain their full identity: form bodies at one URL are distinct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreprocessRequest {
    Json { url: String },
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
        match self { Self::Json { url } | Self::Form { url, .. } => url }
    }

    pub fn key(&self) -> &str {
        match self { Self::Json { url } => url, Self::Form { key, .. } => key }
    }
}

/// Upper bound on retained results. A definition that keeps requesting new
/// URLs stops being served once this is reached; the run still completes with
/// whatever was resolved.
pub const MAX_PREPROCESS_JOBS: usize = 256;

#[derive(Debug, Default)]
pub struct PreprocessJobs {
    fetched: HashMap<String, Value>,
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
    }

    pub fn contains(&self, url: &str) -> bool {
        self.fetched.contains_key(url) || self.forms.contains_key(url)
    }

    /// Resolved results, keyed by URL, as the DSL sees them.
    pub fn results(&self) -> &HashMap<String, Value> {
        &self.fetched
    }

    /// Record a settled job. A failed fetch is stored as `null` so the DSL can
    /// branch on it instead of the download failing.
    pub fn insert(&mut self, url: String, value: Value) {
        if self.fetched.len() >= MAX_PREPROCESS_JOBS {
            return;
        }
        self.fetched.insert(url, value);
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fetched.get(key).or_else(|| self.forms.get(key))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settled_jobs_are_visible_and_reset_drops_them() {
        let mut jobs = PreprocessJobs::new();
        assert!(!jobs.contains("https://example.com/a"));

        jobs.insert("https://example.com/a".to_string(), Value::Null);
        assert!(jobs.contains("https://example.com/a"));
        assert_eq!(
            jobs.results().get("https://example.com/a"),
            Some(&Value::Null)
        );

        jobs.reset();
        assert!(jobs.is_empty());
    }

    #[test]
    fn job_count_is_bounded() {
        let mut jobs = PreprocessJobs::new();
        for index in 0..(MAX_PREPROCESS_JOBS + 8) {
            jobs.insert(format!("https://example.com/{index}"), Value::Null);
        }
        assert_eq!(jobs.len(), MAX_PREPROCESS_JOBS);
    }
}
