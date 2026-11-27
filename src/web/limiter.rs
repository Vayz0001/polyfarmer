//! Failed-attempt throttling for login and first-run setup-code entry.
//!
//! Follows the usual guidance (OWASP Authentication Cheat Sheet): throttle by
//! source, escalate repeat offenders, never reveal *why* a guess failed, and
//! keep an overall ceiling so a distributed guesser can't hammer the (slow,
//! CPU-heavy) password hash.
//!
//! Buckets
//! * **per client IP** — direct peers: 5 failures lock that IP for 30 s, then
//!   60 s, 2 min, … up to 15 min while it keeps failing; one IP can't affect
//!   anyone else.
//! * **proxied** — requests from loopback. The dashboard sits behind
//!   `tailscale serve` / a reverse proxy by design, which connects from
//!   127.0.0.1, so every remote visitor looks identical and a stranger and the
//!   owner share this bucket. It is deliberately more forgiving (15 failures,
//!   flat 30 s lock, no escalation) so a guesser cannot park the owner behind a
//!   long lockout, while still capping guessing at a few dozen tries a minute.
//! * **global ceiling** — 60 failures lock *everyone* for a minute: protects the
//!   argon2 hashing cost whatever the source mix.
//!
//! A success clears that source's bucket. Memory is bounded (the IP map is
//! capped and pruned), since the keys are attacker-controlled.
//!
//! Concurrency: a guess is admitted by [`AttemptLimiter::try_begin`], which
//! checks the lock and reserves the attempt in one step, so parallel requests
//! cannot all pass the check before any failure is counted. An attempt that is
//! still in flight counts against its buckets as if it had failed, and only a
//! few attempts may be in flight at once (each argon2 hash holds ~19 MiB).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Who an attempt is attributed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// A directly connected, non-loopback peer.
    Ip(IpAddr),
    /// Loopback: the local machine or a reverse proxy / Tailscale Serve in front of us.
    Proxied,
}

impl Source {
    /// Attribute a peer address; an unknown peer (e.g. in tests) counts as proxied.
    pub fn from_peer(peer: Option<IpAddr>) -> Self {
        match peer {
            Some(ip) if !ip.is_loopback() => Source::Ip(ip),
            _ => Source::Proxied,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub ip_max_fails: u32,
    pub proxied_max_fails: u32,
    pub base_lock: Duration,
    pub max_lock: Duration,
    pub global_max_fails: u32,
    pub global_lock: Duration,
    /// A bucket with no failures for this long forgets its history.
    pub forget_after: Duration,
    /// Cap on tracked IPs.
    pub max_tracked_ips: usize,
    /// Attempts in flight at once (each usually a slow password hash), across all sources.
    pub max_in_flight: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            ip_max_fails: 5,
            proxied_max_fails: 15,
            base_lock: Duration::from_secs(30),
            max_lock: Duration::from_secs(15 * 60),
            global_max_fails: 60,
            global_lock: Duration::from_secs(60),
            forget_after: Duration::from_secs(3600),
            max_tracked_ips: 1024,
            max_in_flight: 4,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    fails: u32,
    /// How many times this bucket has been locked (drives escalation).
    strikes: u32,
    locked_until: Option<Instant>,
    last_fail: Instant,
}

impl Bucket {
    fn new(now: Instant) -> Self {
        Self { fails: 0, strikes: 0, locked_until: None, last_fail: now }
    }
    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.locked_until.and_then(|u| u.checked_duration_since(now)).filter(|d| !d.is_zero())
    }
    /// `(fails, strikes)` the next failure would build on: the resets `register`
    /// applies first (quiet for `forget`, expired lock) are applied here too.
    fn standing(&self, now: Instant, forget: Duration) -> (u32, u32) {
        if now.duration_since(self.last_fail) > forget && self.remaining(now).is_none() {
            (0, 0)
        } else if self.locked_until.is_some() && self.remaining(now).is_none() {
            (0, self.strikes)
        } else {
            (self.fails, self.strikes)
        }
    }
}

/// How long the lock that a bucket's `strikes`-th lockout imposes lasts.
fn lock_length(strikes: u32, base: Duration, cap: Duration, escalate: bool) -> Duration {
    let mult = if escalate { 1u32.checked_shl(strikes.min(16)).unwrap_or(u32::MAX) } else { 1 };
    base.saturating_mul(mult).min(cap)
}

/// Wait suggested when every in-flight slot is taken (a hash takes well under that).
const BUSY_WAIT: Duration = Duration::from_secs(1);

#[derive(Debug)]
struct Inner {
    ips: HashMap<IpAddr, Bucket>,
    proxied: Option<Bucket>,
    global: Option<Bucket>,
    /// Attempts admitted by `try_begin` and not yet settled, per source. Kept
    /// apart from the buckets so a success clearing a bucket can't drop them;
    /// bounded by `max_in_flight`.
    in_flight: HashMap<Source, u32>,
    in_flight_total: u32,
}

#[derive(Debug)]
pub struct AttemptLimiter {
    policy: Policy,
    inner: Mutex<Inner>,
}

impl Default for AttemptLimiter {
    fn default() -> Self {
        Self::new(Policy::default())
    }
}

impl AttemptLimiter {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            inner: Mutex::new(Inner {
                ips: HashMap::new(),
                proxied: None,
                global: None,
                in_flight: HashMap::new(),
                in_flight_total: 0,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `Some(wait)` if `source` (or everyone) is currently locked out.
    pub fn check(&self, source: Source) -> Option<Duration> {
        let now = Instant::now();
        let inner = self.lock();
        let own = match source {
            Source::Ip(ip) => inner.ips.get(&ip).and_then(|b| b.remaining(now)),
            Source::Proxied => inner.proxied.as_ref().and_then(|b| b.remaining(now)),
        };
        let global = inner.global.as_ref().and_then(|b| b.remaining(now));
        own.max(global)
    }

    /// Admit one attempt from `source`, or `Err(wait)` if it must not try now:
    /// locked out, too many attempts in flight, or the attempts already in
    /// flight would lock it out if they all fail. Checking and reserving happen
    /// under one lock, so a burst of parallel requests gets exactly as many
    /// guesses as the same requests one at a time.
    pub fn try_begin(self: &Arc<Self>, source: Source) -> Result<Attempt, Duration> {
        let now = Instant::now();
        let p = self.policy;
        let mut inner = self.lock();
        let (own, max_fails, base, cap, escalate) = match source {
            Source::Ip(ip) => (inner.ips.get(&ip), p.ip_max_fails, p.base_lock, p.max_lock, true),
            Source::Proxied => (inner.proxied.as_ref(), p.proxied_max_fails, p.base_lock, p.max_lock, false),
        };
        let global = inner.global.as_ref();
        if let Some(wait) = own.and_then(|b| b.remaining(now)).max(global.and_then(|b| b.remaining(now))) {
            return Err(wait);
        }
        if inner.in_flight_total >= p.max_in_flight {
            return Err(BUSY_WAIT);
        }
        let (own_fails, own_strikes) = own.map_or((0, 0), |b| b.standing(now, p.forget_after));
        let own_in_flight = inner.in_flight.get(&source).copied().unwrap_or(0);
        if own_fails.saturating_add(own_in_flight) >= max_fails {
            return Err(lock_length(own_strikes, base, cap, escalate));
        }
        let (global_fails, _) = global.map_or((0, 0), |b| b.standing(now, p.forget_after));
        if global_fails.saturating_add(inner.in_flight_total) >= p.global_max_fails {
            return Err(p.global_lock);
        }
        *inner.in_flight.entry(source).or_insert(0) += 1;
        inner.in_flight_total += 1;
        Ok(Attempt { limiter: Arc::clone(self), source, settled: false })
    }

    /// Settle an attempt admitted by `try_begin`. Freeing its slot and recording
    /// its result happen under one lock, so no other attempt is admitted in between.
    fn settle(&self, source: Source, outcome: Outcome) {
        let mut inner = self.lock();
        if let Some(n) = inner.in_flight.get_mut(&source) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                inner.in_flight.remove(&source);
            }
        }
        inner.in_flight_total = inner.in_flight_total.saturating_sub(1);
        match outcome {
            Outcome::Failed => self.record_fail(&mut inner, source),
            Outcome::Succeeded => Self::record_success(&mut inner, source),
            Outcome::Uncounted => {}
        }
    }

    /// Record a failed attempt from `source`.
    pub fn fail(&self, source: Source) {
        self.record_fail(&mut self.lock(), source);
    }

    fn record_fail(&self, inner: &mut Inner, source: Source) {
        let now = Instant::now();
        let p = self.policy;

        // Source bucket.
        let (max_fails, escalate) = match source {
            Source::Ip(_) => (p.ip_max_fails, true),
            Source::Proxied => (p.proxied_max_fails, false),
        };
        let forget = p.forget_after;
        let bucket = match source {
            Source::Ip(ip) => {
                if !inner.ips.contains_key(&ip) && inner.ips.len() >= p.max_tracked_ips {
                    Self::make_room(&mut inner.ips, now, forget);
                }
                inner.ips.entry(ip).or_insert_with(|| Bucket::new(now))
            }
            Source::Proxied => inner.proxied.get_or_insert_with(|| Bucket::new(now)),
        };
        Self::register(bucket, now, max_fails, p.base_lock, p.max_lock, escalate, forget);

        // Global ceiling.
        let g = inner.global.get_or_insert_with(|| Bucket::new(now));
        Self::register(g, now, p.global_max_fails, p.global_lock, p.global_lock, false, forget);
    }

    /// A successful login clears that source's history.
    pub fn success(&self, source: Source) {
        Self::record_success(&mut self.lock(), source);
    }

    fn record_success(inner: &mut Inner, source: Source) {
        match source {
            Source::Ip(ip) => {
                inner.ips.remove(&ip);
            }
            Source::Proxied => inner.proxied = None,
        }
    }

    /// Number of IPs currently tracked (for tests / diagnostics).
    pub fn tracked_ips(&self) -> usize {
        self.lock().ips.len()
    }

    fn register(
        b: &mut Bucket,
        now: Instant,
        max_fails: u32,
        base: Duration,
        cap: Duration,
        escalate: bool,
        forget: Duration,
    ) {
        // Old history is forgotten once the source has been quiet for a while.
        if now.duration_since(b.last_fail) > forget && b.remaining(now).is_none() {
            *b = Bucket::new(now);
        }
        // An expired lock starts a fresh failure count (strikes are kept: escalation).
        if b.locked_until.is_some() && b.remaining(now).is_none() {
            b.locked_until = None;
            b.fails = 0;
        }
        b.last_fail = now;
        // Failures during a lock don't extend it (the caller shouldn't even be
        // trying), they just don't count either.
        if b.remaining(now).is_some() {
            return;
        }
        b.fails += 1;
        if b.fails >= max_fails {
            b.locked_until = Some(now + lock_length(b.strikes, base, cap, escalate));
            b.strikes = b.strikes.saturating_add(1);
            b.fails = 0;
        }
    }

    /// Drop stale entries; if still full, drop the least recently failing one.
    fn make_room(map: &mut HashMap<IpAddr, Bucket>, now: Instant, forget: Duration) {
        map.retain(|_, b| b.remaining(now).is_some() || now.duration_since(b.last_fail) <= forget);
        if let Some(oldest) =
            map.iter().filter(|(_, b)| b.remaining(now).is_none()).min_by_key(|(_, b)| b.last_fail).map(|(ip, _)| *ip)
        {
            map.remove(&oldest);
        } else if let Some(any) = map.keys().next().copied() {
            map.remove(&any);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Outcome {
    Failed,
    Succeeded,
    Uncounted,
}

/// An attempt admitted by [`AttemptLimiter::try_begin`]. It holds its in-flight
/// slot until settled; dropping it unsettled (e.g. a panic mid-check) counts
/// as a failure, so an attempt can never escape the count.
#[derive(Debug)]
#[must_use = "an attempt counts as failed unless it is settled"]
pub struct Attempt {
    limiter: Arc<AttemptLimiter>,
    source: Source,
    settled: bool,
}

impl Attempt {
    /// The guess was wrong: count it.
    pub fn failed(mut self) {
        self.finish(Outcome::Failed);
    }

    /// The guess was right: clear the source's history (like `success`).
    pub fn succeeded(mut self) {
        self.finish(Outcome::Succeeded);
    }

    /// Nothing was guessed after all (the right setup code, whose success is
    /// recorded later): free the slot without counting it either way.
    pub fn uncounted(mut self) {
        self.finish(Outcome::Uncounted);
    }

    fn finish(&mut self, outcome: Outcome) {
        if !self.settled {
            self.settled = true;
            self.limiter.settle(self.source, outcome);
        }
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        self.finish(Outcome::Failed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> Source {
        Source::Ip(IpAddr::from([203, 0, 113, n]))
    }
    fn fast() -> Policy {
        Policy {
            base_lock: Duration::from_millis(60),
            max_lock: Duration::from_millis(500),
            global_lock: Duration::from_millis(60),
            ..Policy::default()
        }
    }

    #[test]
    fn five_failures_lock_that_ip_only() {
        let l = AttemptLimiter::new(fast());
        for _ in 0..4 {
            l.fail(ip(1));
        }
        assert!(l.check(ip(1)).is_none(), "4 failures: not locked yet");
        l.fail(ip(1));
        assert!(l.check(ip(1)).is_some(), "5th failure locks");
        assert!(l.check(ip(2)).is_none(), "another IP is unaffected");
        assert!(l.check(Source::Proxied).is_none(), "and so is proxied traffic");
    }

    #[test]
    fn lock_expires_and_success_clears_history() {
        let l = AttemptLimiter::new(fast());
        for _ in 0..5 {
            l.fail(ip(1));
        }
        assert!(l.check(ip(1)).is_some());
        std::thread::sleep(Duration::from_millis(90));
        assert!(l.check(ip(1)).is_none(), "lock expired");

        for _ in 0..3 {
            l.fail(ip(1));
        }
        l.success(ip(1));
        for _ in 0..4 {
            l.fail(ip(1));
        }
        assert!(l.check(ip(1)).is_none(), "success reset the failure count");
        assert_eq!(l.tracked_ips(), 1);
    }

    #[test]
    fn repeat_offenders_are_locked_for_longer_each_time() {
        let l = AttemptLimiter::new(fast());
        let mut waits = Vec::new();
        for _ in 0..3 {
            for _ in 0..5 {
                l.fail(ip(9));
            }
            let w = l.check(ip(9)).expect("locked");
            waits.push(w);
            std::thread::sleep(w + Duration::from_millis(10));
        }
        assert!(waits[1] > waits[0] && waits[2] > waits[1], "escalating: {waits:?}");
        assert!(waits[2] <= Duration::from_millis(500), "capped at max_lock");
    }

    #[test]
    fn proxied_bucket_is_more_forgiving_and_never_escalates() {
        let l = AttemptLimiter::new(fast());
        for _ in 0..14 {
            l.fail(Source::Proxied);
        }
        assert!(l.check(Source::Proxied).is_none(), "14 < 15");
        l.fail(Source::Proxied);
        let first = l.check(Source::Proxied).expect("locked at 15");
        std::thread::sleep(first + Duration::from_millis(10));
        for _ in 0..15 {
            l.fail(Source::Proxied);
        }
        let second = l.check(Source::Proxied).expect("locked again");
        assert!(second <= first + Duration::from_millis(5), "flat lock, no escalation: {first:?} -> {second:?}");
    }

    #[test]
    fn global_ceiling_locks_everyone_across_many_sources() {
        let l = AttemptLimiter::new(fast());
        // 60 failures spread over 30 different IPs (2 each: none individually locked).
        for n in 0..30u8 {
            l.fail(ip(n));
            l.fail(ip(n));
        }
        assert!(l.check(ip(200)).is_some(), "a source that never failed is held by the global lock");
        assert!(l.check(Source::Proxied).is_some());
        std::thread::sleep(Duration::from_millis(90));
        assert!(l.check(ip(200)).is_none());
    }

    #[test]
    fn tracked_ips_are_bounded() {
        let l = AttemptLimiter::new(Policy { max_tracked_ips: 50, ..fast() });
        for a in 0..4u8 {
            for b in 0..100u8 {
                l.fail(Source::Ip(IpAddr::from([198, 51, a, b])));
            }
        }
        assert!(l.tracked_ips() <= 50, "tracked {}", l.tracked_ips());
    }

    #[test]
    fn loopback_and_unknown_peers_are_proxied() {
        assert_eq!(Source::from_peer(None), Source::Proxied);
        assert_eq!(Source::from_peer(Some(IpAddr::from([127, 0, 0, 1]))), Source::Proxied);
        assert_eq!(Source::from_peer(Some("::1".parse().unwrap())), Source::Proxied);
        let a = IpAddr::from([100, 64, 1, 2]);
        assert_eq!(Source::from_peer(Some(a)), Source::Ip(a));
    }

    fn roomy() -> Arc<AttemptLimiter> {
        Arc::new(AttemptLimiter::new(Policy { max_in_flight: 1000, ..fast() }))
    }

    #[test]
    fn two_attempts_begun_before_either_fails_cannot_both_take_the_last_guess() {
        let l = roomy();
        for _ in 0..4 {
            l.fail(ip(1));
        }
        let last = l.try_begin(ip(1)).expect("the 5th guess is allowed");
        assert!(l.try_begin(ip(1)).is_err(), "a parallel 6th guess is refused while the 5th is in flight");
        last.failed();
        assert!(l.check(ip(1)).is_some(), "the 5th failure locks, as it does one at a time");
    }

    #[test]
    fn a_burst_gets_exactly_the_sequential_number_of_guesses() {
        let l = roomy();
        let admitted: Vec<_> = (0..50).filter_map(|_| l.try_begin(Source::Proxied).ok()).collect();
        assert_eq!(admitted.len(), 15, "proxied threshold");
        let refused = l.try_begin(Source::Proxied).expect_err("refused");
        assert_eq!(refused, fast().base_lock, "told to wait for the lock the pending guesses will cause");
        admitted.into_iter().for_each(Attempt::failed);
        assert!(l.check(Source::Proxied).is_some());
        assert!(l.try_begin(ip(1)).is_ok(), "an IP source has its own bucket");
    }

    #[test]
    fn begin_then_fail_one_at_a_time_matches_fail() {
        let l = roomy();
        for _ in 0..5 {
            l.try_begin(ip(1)).expect("not locked yet").failed();
        }
        let wait = l.try_begin(ip(1)).expect_err("locked after 5");
        assert!(wait <= fast().base_lock && wait > Duration::ZERO, "{wait:?}");
    }

    #[test]
    fn many_sources_are_held_by_the_global_ceiling_while_in_flight() {
        let l = roomy();
        // 60 in flight from 30 IPs (2 each): none individually near its threshold.
        let admitted: Vec<_> = (0..30u8).flat_map(|n| [l.try_begin(ip(n)), l.try_begin(ip(n))]).collect();
        assert!(admitted.iter().all(Result::is_ok));
        assert_eq!(l.try_begin(ip(200)).expect_err("global ceiling"), fast().global_lock);
    }

    #[test]
    fn only_a_few_attempts_run_at_once() {
        let l = Arc::new(AttemptLimiter::new(fast()));
        let mut running: Vec<_> = (0..4u8).map(|n| l.try_begin(ip(n)).expect("slot")).collect();
        assert_eq!(l.try_begin(ip(9)).expect_err("all slots taken"), BUSY_WAIT);
        running.pop().expect("one").failed();
        assert!(l.try_begin(ip(9)).is_ok(), "a settled attempt frees its slot");
    }

    #[test]
    fn a_success_clears_history_but_not_the_other_attempts_in_flight() {
        let l = roomy();
        for _ in 0..3 {
            l.fail(ip(1));
        }
        let right = l.try_begin(ip(1)).expect("4th");
        let wrong = l.try_begin(ip(1)).expect("5th");
        right.succeeded();
        // Cleared: 0 failures, 1 still in flight → 4 more may start, not 5.
        let more: Vec<_> = (0..10).filter_map(|_| l.try_begin(ip(1)).ok()).collect();
        assert_eq!(more.len(), 4);
        drop(wrong);
        drop(more);
    }

    #[test]
    fn an_unsettled_attempt_counts_as_failed_and_an_uncounted_one_does_not() {
        let l = roomy();
        for _ in 0..4 {
            l.fail(ip(1));
        }
        l.try_begin(ip(1)).expect("5th").uncounted();
        assert!(l.check(ip(1)).is_none(), "an uncounted attempt is not a failure");
        drop(l.try_begin(ip(1)).expect("5th again"));
        assert!(l.check(ip(1)).is_some(), "dropped without a result: counted as a failure");
    }
}
