//! Lock helpers that recover from poisoning instead of panicking.
//!
//! With `.lock().unwrap()`, one panic while a guard is held makes every later
//! caller panic too, including `BookGuard::drop`. The locks that use these
//! helpers hold counters, display-only order books, an alert buffer and an
//! `Option<String>`: none of it can be left half-written in a way that
//! matters, so carrying on with the inner data is the right call.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// [`Mutex::lock`] that returns the guard even if the mutex is poisoned.
pub trait LockExt<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// [`RwLock::read`] / [`RwLock::write`] that return the guard even if the lock is poisoned.
pub trait RwLockExt<T> {
    fn read_or_recover(&self) -> RwLockReadGuard<'_, T>;
    fn write_or_recover(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
    fn read_or_recover(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_or_recover(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn poisoned_mutex_still_returns_data() {
        let m = Mutex::new(1);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let mut g = m.lock().unwrap();
            *g = 2;
            panic!("poison");
        }));
        assert!(m.is_poisoned());
        *m.lock_or_recover() += 1;
        assert_eq!(*m.lock_or_recover(), 3);
    }

    #[test]
    fn poisoned_rwlock_still_returns_data() {
        let l = RwLock::new(String::from("a"));
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let mut g = l.write().unwrap();
            g.push('b');
            panic!("poison");
        }));
        assert!(l.is_poisoned());
        l.write_or_recover().push('c');
        assert_eq!(*l.read_or_recover(), "abc");
    }

    #[test]
    fn healthy_locks_behave_like_unwrap() {
        let m = Mutex::new(vec![1]);
        m.lock_or_recover().push(2);
        assert_eq!(*m.lock().unwrap(), vec![1, 2]);
        let l = RwLock::new(0);
        *l.write_or_recover() = 5;
        assert_eq!(*l.read().unwrap(), 5);
    }
}
