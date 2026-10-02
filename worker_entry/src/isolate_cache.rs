//! isolate 内で使い回す値の TTL キャッシュ。
//!
//! サイト定義・設定・秘密値・`asset_backend` のような値は isolate の寿命の
//! 間ほぼ不変なのに、以前はリクエストごとに D1 / Secrets Store /
//! オブジェクトストアへ読み直していた (D1 は 1 往復 150〜250ms かかるため、
//! これが Web UI の応答時間を支配していた)。
//!
//! 書き込み側は [`TtlMap::invalidate`] で即時反映し、他の isolate は TTL で
//! 追従する。TTL は「他 isolate の変更が反映されるまでの最大遅れ」を意味する。

use std::collections::HashMap;
use std::sync::Mutex;

/// キーごとに TTL 付きで値を保持する。
pub struct TtlMap<V> {
    ttl_ms: f64,
    entries: Mutex<HashMap<String, (f64, V)>>,
}

impl<V: Clone> TtlMap<V> {
    /// `ttl_ms` は他 isolate の変更を許容する最大遅延。
    pub fn new(ttl_ms: f64) -> Self {
        Self {
            ttl_ms,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// 期限内なら値を返す。期限切れの値は捨てる。
    pub fn get(&self, key: &str) -> Option<V> {
        let mut entries = self.entries.lock().ok()?;
        let (stored_at, value) = entries.get(key)?;
        if js_sys::Date::now() - stored_at >= self.ttl_ms {
            entries.remove(key);
            return None;
        }
        Some(value.clone())
    }

    /// 値を入れ直す。
    pub fn put(&self, key: impl Into<String>, value: V) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(key.into(), (js_sys::Date::now(), value));
        }
    }

    /// 書き込み直後に呼ぶ (同じ isolate では即時反映)。
    pub fn invalidate(&self, key: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(key);
        }
    }
}
