//! Tiny in-memory TTL cache for dashboard reads of slow / rate-limited
//! upstreams (Gamma metadata, reward scoring, balance, Data API).
//!
//! Two read styles:
//!   * [`TtlCache::get_or_fetch`] — await a fresh value (fetches on miss/stale).
//!   * [`TtlCache::get_swr`] — stale-while-revalidate: return whatever is cached
//!     *now* (possibly stale or `None`) and refresh in the background, so a
//!     page or status strip never blocks on the network.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct TtlCache<V> {
    ttl: Duration,
    entries: Mutex<HashMap<String, (Instant, V)>>,
    /// Keys with a background refresh in flight (dedupes `get_swr` refreshes).
    refreshing: Mutex<HashSet<String>>,
}

impl<V: Clone + Send + 'static> TtlCache<V> {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, entries: Mutex::new(HashMap::new()), refreshing: Mutex::new(HashSet::new()) }
    }

    /// Cached value and whether it is still fresh.
    pub fn peek(&self, key: &str) -> Option<(V, bool)> {
        let entries = self.entries.lock().unwrap();
        entries.get(key).map(|(at, v)| (v.clone(), at.elapsed() < self.ttl))
    }

    pub fn get_fresh(&self, key: &str) -> Option<V> {
        self.peek(key).and_then(|(v, fresh)| fresh.then_some(v))
    }

    pub fn put(&self, key: &str, value: V) {
        self.entries.lock().unwrap().insert(key.to_string(), (Instant::now(), value));
    }

    /// Fresh cached value, or await `fetch` and cache its success. Errors are
    /// not cached; a stale value is returned instead of an error if one exists.
    pub async fn get_or_fetch<F, Fut, E>(&self, key: &str, fetch: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        let stale = match self.peek(key) {
            Some((v, true)) => return Ok(v),
            Some((v, false)) => Some(v),
            None => None,
        };
        match fetch().await {
            Ok(v) => {
                self.put(key, v.clone());
                Ok(v)
            }
            Err(e) => stale.ok_or(e),
        }
    }

    /// Stale-while-revalidate: returns the cached value immediately (or `None`)
    /// and, if it is missing or stale, refreshes it in a background task.
    pub fn get_swr<F, Fut, E>(self: &Arc<Self>, key: &str, fetch: F) -> Option<V>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<V, E>> + Send + 'static,
        E: Send + 'static,
    {
        let current = self.peek(key);
        let needs_refresh = !matches!(current, Some((_, true)));
        if needs_refresh && self.refreshing.lock().unwrap().insert(key.to_string()) {
            let this = Arc::clone(self);
            let key = key.to_string();
            tokio::spawn(async move {
                if let Ok(v) = fetch().await {
                    this.put(&key, v);
                }
                this.refreshing.lock().unwrap().remove(&key);
            });
        }
        current.map(|(v, _)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_or_fetch_caches_and_falls_back_to_stale() {
        let c: TtlCache<u32> = TtlCache::new(Duration::from_millis(30));
        let v: Result<u32, ()> = c.get_or_fetch("k", || async { Ok(1) }).await;
        assert_eq!(v, Ok(1));
        // Fresh hit — fetch not called.
        let v: Result<u32, ()> = c.get_or_fetch("k", || async { panic!("should be cached") }).await;
        assert_eq!(v, Ok(1));
        tokio::time::sleep(Duration::from_millis(40)).await;
        // Stale + failing fetch → stale value rather than an error.
        let v: Result<u32, &str> = c.get_or_fetch("k", || async { Err("down") }).await;
        assert_eq!(v, Ok(1));
    }

    #[tokio::test]
    async fn swr_returns_immediately_then_fills() {
        let c: Arc<TtlCache<u32>> = Arc::new(TtlCache::new(Duration::from_secs(60)));
        assert_eq!(c.get_swr("k", || async { Ok::<_, ()>(7) }), None);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(c.get_swr("k", || async { Ok::<_, ()>(8) }), Some(7));
    }
}
