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
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Entry cap for caches that don't choose their own. Several caches are keyed by
/// user-influenced text (search box, slugs), so none may grow without bound.
pub const DEFAULT_CAPACITY: usize = 256;

/// Lock that survives poisoning: the guarded maps hold plain data that is valid
/// after a panic elsewhere, and one panicked task must not wedge every later read.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct TtlCache<V> {
    ttl: Duration,
    capacity: usize,
    entries: Mutex<HashMap<String, (Instant, V)>>,
    /// Keys with a background refresh in flight (dedupes `get_swr` refreshes).
    refreshing: Mutex<HashSet<String>>,
}

/// Frees a key's `refreshing` slot when the refresh task ends — including when
/// the fetch panics, which would otherwise leave the key un-refreshable forever.
struct RefreshGuard<V: Clone + Send + 'static> {
    cache: Arc<TtlCache<V>>,
    key: String,
}

impl<V: Clone + Send + 'static> Drop for RefreshGuard<V> {
    fn drop(&mut self) {
        lock(&self.cache.refreshing).remove(&self.key);
    }
}

impl<V: Clone + Send + 'static> TtlCache<V> {
    pub fn new(ttl: Duration) -> Self {
        Self::with_capacity(ttl, DEFAULT_CAPACITY)
    }

    pub fn with_capacity(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity: capacity.max(1),
            entries: Mutex::new(HashMap::new()),
            refreshing: Mutex::new(HashSet::new()),
        }
    }

    /// Number of cached entries (fresh or stale).
    pub fn len(&self) -> usize {
        lock(&self.entries).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Cached value and whether it is still fresh.
    pub fn peek(&self, key: &str) -> Option<(V, bool)> {
        let entries = lock(&self.entries);
        entries.get(key).map(|(at, v)| (v.clone(), at.elapsed() < self.ttl))
    }

    pub fn get_fresh(&self, key: &str) -> Option<V> {
        self.peek(key).and_then(|(v, fresh)| fresh.then_some(v))
    }

    /// Insert, keeping the cache within capacity: a new key into a full cache first
    /// drops entries past a few TTLs (they can't be served as anything useful),
    /// then the oldest entry until there is room.
    pub fn put(&self, key: &str, value: V) {
        let mut entries = lock(&self.entries);
        if !entries.contains_key(key) && entries.len() >= self.capacity {
            let horizon = self.ttl.saturating_mul(4);
            entries.retain(|_, (at, _)| at.elapsed() < horizon);
            while entries.len() >= self.capacity {
                let Some(oldest) = entries.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone()) else { break };
                entries.remove(&oldest);
            }
        }
        entries.insert(key.to_string(), (Instant::now(), value));
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
        if needs_refresh && lock(&self.refreshing).insert(key.to_string()) {
            let guard = RefreshGuard { cache: Arc::clone(self), key: key.to_string() };
            tokio::spawn(async move {
                if let Ok(v) = fetch().await {
                    guard.cache.put(&guard.key, v);
                }
                // `guard` drops here (or during unwind), releasing the refresh slot.
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

    #[test]
    fn never_exceeds_capacity_and_evicts_the_oldest_first() {
        let c: TtlCache<u32> = TtlCache::with_capacity(Duration::from_secs(60), 3);
        for (i, k) in ["a", "b", "c"].into_iter().enumerate() {
            c.put(k, i as u32);
            std::thread::sleep(Duration::from_millis(2));
        }
        c.put("d", 3);
        assert_eq!(c.len(), 3);
        assert!(c.peek("a").is_none(), "oldest entry is evicted");
        assert!(c.peek("b").is_some() && c.peek("c").is_some() && c.peek("d").is_some());
        // Overwriting an existing key never evicts anything.
        c.put("b", 99);
        assert_eq!(c.len(), 3);
        assert_eq!(c.peek("b").unwrap().0, 99);
        // A flood of distinct keys stays bounded.
        for i in 0..1000 {
            c.put(&format!("k{i}"), i);
        }
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn expired_entries_are_dropped_before_live_ones_when_full() {
        let c: TtlCache<u32> = TtlCache::with_capacity(Duration::from_millis(5), 2);
        c.put("old", 1);
        std::thread::sleep(Duration::from_millis(30)); // > 4 × ttl
        c.put("live", 2);
        c.put("new", 3); // full: "old" is expired and goes, "live" survives
        assert!(c.peek("old").is_none());
        assert!(c.peek("live").is_some() && c.peek("new").is_some());
    }

    #[tokio::test]
    async fn a_panicking_refresh_frees_its_slot() {
        let c: Arc<TtlCache<u32>> = Arc::new(TtlCache::new(Duration::from_secs(60)));
        let _ = c.get_swr("k", || async {
            if true {
                panic!("upstream exploded");
            }
            Ok::<u32, ()>(0)
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The slot was released, so the next read can start a working refresh.
        assert_eq!(c.get_swr("k", || async { Ok::<_, ()>(5) }), None);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(c.peek("k").map(|(v, _)| v), Some(5));
    }
}
