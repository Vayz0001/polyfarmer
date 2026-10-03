//! Bounded in-memory session store.
//!
//! tower-sessions' stock `MemoryStore` is a bare `HashMap`: expired records are
//! skipped on load but never removed, and every cookie-less visit to `/login`
//! or `/welcome` creates a record (the CSRF token lives in the session). Anyone
//! who can reach the port could therefore grow the bot's memory without limit.
//!
//! This store keeps the same semantics but is hard-capped and self-cleaning:
//!
//! * a **maximum number of sessions**; when full, expired records go first, then
//!   the oldest *unauthenticated* ones — so a flood of anonymous requests can
//!   evict other anonymous visitors but never log the owner out;
//! * a **periodic sweep** ([`BoundedSessionStore::spawn_sweeper`]) removes expired
//!   records even when nobody is touching the store;
//! * expired records are removed when they are loaded.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use tower_sessions::session::{Id, Record};
use tower_sessions::session_store::{self, SessionStore};

/// Session-data key set once a visitor has logged in (see `web::auth`).
pub const AUTH_KEY: &str = "auth";

/// Hard cap on stored sessions. One owner and a few tabs need a handful;
/// the rest of the headroom absorbs anonymous visitors and scanners.
pub const DEFAULT_MAX_SESSIONS: usize = 2_000;

/// How often the background sweep removes expired sessions.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub struct BoundedSessionStore {
    inner: Arc<Mutex<HashMap<Id, Record>>>,
    max: usize,
}

impl Default for BoundedSessionStore {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_SESSIONS)
    }
}

fn is_active(expiry: OffsetDateTime) -> bool {
    expiry > OffsetDateTime::now_utc()
}

fn is_authenticated(rec: &Record) -> bool {
    rec.data.get(AUTH_KEY).and_then(|v| v.as_bool()).unwrap_or(false)
}

impl BoundedSessionStore {
    pub fn new(max: usize) -> Self {
        Self { inner: Arc::new(Mutex::new(HashMap::new())), max: max.max(1) }
    }

    /// A poisoned lock only means some other thread panicked mid-update; the map
    /// itself is still a valid HashMap, so keep serving rather than cascade.
    fn lock(&self) -> MutexGuard<'_, HashMap<Id, Record>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Number of stored sessions (including not-yet-swept expired ones).
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove every expired session; returns how many were dropped.
    pub fn purge_expired(&self) -> usize {
        let mut map = self.lock();
        let before = map.len();
        map.retain(|_, r| is_active(r.expiry_date));
        before - map.len()
    }

    /// Spawn the periodic sweep. A no-op outside a tokio runtime (the cap and
    /// load-time expiry still bound memory).
    pub fn spawn_sweeper(&self, every: Duration) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let store = self.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.tick().await; // first tick fires immediately — skip it
            loop {
                tick.tick().await;
                store.purge_expired();
            }
        });
    }

    /// Insert `record`, first making room if the store is at capacity.
    fn insert(&self, map: &mut HashMap<Id, Record>, record: Record) {
        if !map.contains_key(&record.id) && map.len() >= self.max {
            // 1) expired records are free to drop
            map.retain(|_, r| is_active(r.expiry_date));
            // 2) still full: evict oldest unauthenticated first, and only if there
            //    are none evict the oldest authenticated one.
            while map.len() >= self.max {
                let victim = map
                    .iter()
                    .filter(|(_, r)| !is_authenticated(r))
                    .min_by_key(|(_, r)| r.expiry_date)
                    .or_else(|| map.iter().min_by_key(|(_, r)| r.expiry_date))
                    .map(|(id, _)| *id);
                match victim {
                    Some(id) => {
                        map.remove(&id);
                    }
                    None => break,
                }
            }
        }
        map.insert(record.id, record);
    }
}

#[async_trait]
impl SessionStore for BoundedSessionStore {
    async fn create(&self, record: &mut Record) -> session_store::Result<()> {
        let mut map = self.lock();
        while map.contains_key(&record.id) {
            // Session-id collision (astronomically unlikely at 128 bits): re-roll.
            record.id = Id::default();
        }
        self.insert(&mut map, record.clone());
        Ok(())
    }

    async fn save(&self, record: &Record) -> session_store::Result<()> {
        let mut map = self.lock();
        self.insert(&mut map, record.clone());
        Ok(())
    }

    async fn load(&self, id: &Id) -> session_store::Result<Option<Record>> {
        let mut map = self.lock();
        match map.get(id) {
            Some(r) if is_active(r.expiry_date) => Ok(Some(r.clone())),
            Some(_) => {
                map.remove(id);
                Ok(None)
            }
            None => Ok(None),
        }
    }

    async fn delete(&self, id: &Id) -> session_store::Result<()> {
        self.lock().remove(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    fn record(authed: bool, expires_in_secs: i64) -> Record {
        let mut data = Map::new();
        if authed {
            data.insert(AUTH_KEY.to_string(), serde_json::json!(true));
        }
        Record {
            id: Id::default(),
            data,
            expiry_date: OffsetDateTime::now_utc() + time::Duration::seconds(expires_in_secs),
        }
    }

    #[tokio::test]
    async fn create_load_save_delete_roundtrip() {
        let s = BoundedSessionStore::new(10);
        let mut r = record(false, 60);
        s.create(&mut r).await.unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s.load(&r.id).await.unwrap().unwrap().id, r.id);

        r.data.insert("k".into(), serde_json::json!("v"));
        s.save(&r).await.unwrap();
        assert_eq!(s.load(&r.id).await.unwrap().unwrap().data["k"], "v");
        assert_eq!(s.len(), 1, "save of an existing id does not grow the store");

        s.delete(&r.id).await.unwrap();
        assert!(s.load(&r.id).await.unwrap().is_none());
        assert!(s.is_empty());
    }

    #[tokio::test]
    async fn expired_sessions_are_not_returned_and_are_removed_on_load() {
        let s = BoundedSessionStore::new(10);
        let mut r = record(false, -5);
        s.create(&mut r).await.unwrap();
        assert_eq!(s.len(), 1);
        assert!(s.load(&r.id).await.unwrap().is_none());
        assert_eq!(s.len(), 0, "an expired record is dropped when it is loaded");
    }

    #[tokio::test]
    async fn store_never_exceeds_its_cap_under_an_anonymous_flood() {
        let s = BoundedSessionStore::new(50);
        for _ in 0..5_000 {
            let mut r = record(false, 1800);
            s.create(&mut r).await.unwrap();
        }
        assert_eq!(s.len(), 50);
    }

    #[tokio::test]
    async fn a_flood_of_anonymous_sessions_cannot_evict_the_owner() {
        let s = BoundedSessionStore::new(20);
        let mut owner = record(true, 3600);
        s.create(&mut owner).await.unwrap();
        for _ in 0..1_000 {
            let mut anon = record(false, 1800);
            s.create(&mut anon).await.unwrap();
        }
        assert_eq!(s.len(), 20);
        assert!(s.load(&owner.id).await.unwrap().is_some(), "authenticated session survived the flood");
    }

    #[tokio::test]
    async fn full_store_drops_expired_before_evicting_live_sessions() {
        let s = BoundedSessionStore::new(3);
        let mut live = record(false, 600);
        let mut dead1 = record(false, -1);
        let mut dead2 = record(false, -1);
        for r in [&mut live, &mut dead1, &mut dead2] {
            s.create(r).await.unwrap();
        }
        let mut fresh = record(false, 600);
        s.create(&mut fresh).await.unwrap();
        assert!(s.load(&live.id).await.unwrap().is_some(), "live session kept; only dead ones were purged");
        assert!(s.load(&fresh.id).await.unwrap().is_some());
        assert!(s.len() <= 3);
    }

    #[tokio::test]
    async fn sweep_removes_expired_records() {
        let s = BoundedSessionStore::new(100);
        for secs in [-10, -1, 600, 900] {
            let mut r = record(false, secs);
            s.create(&mut r).await.unwrap();
        }
        assert_eq!(s.purge_expired(), 2);
        assert_eq!(s.len(), 2);
    }

    #[tokio::test]
    async fn background_sweeper_runs() {
        let s = BoundedSessionStore::new(100);
        let mut r = record(false, 1);
        s.create(&mut r).await.unwrap();
        s.spawn_sweeper(Duration::from_millis(40));
        // expires in ~1s; wait past expiry plus a couple of sweep ticks
        tokio::time::sleep(Duration::from_millis(1400)).await;
        assert_eq!(s.len(), 0);
    }
}
