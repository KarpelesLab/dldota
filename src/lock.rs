//! A futex lock that needs no TLS and no libc, usable from foreign threads.

use std::cell::UnsafeCell;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::sys;

pub(crate) struct RawLock<T> {
    word: AtomicU32,
    value: UnsafeCell<T>,
}

// SAFETY: `value` is only reached with the lock held.
unsafe impl<T: Send> Sync for RawLock<T> {}

// No poisoning is needed for unwind safety: the guarded code never panics
// (fatal errors trap), and if it did, the lock is never released by
// unwinding, so later users would block instead of seeing half-done updates.
impl<T> UnwindSafe for RawLock<T> {}
impl<T> RefUnwindSafe for RawLock<T> {}

impl<T> RawLock<T> {
    pub(crate) const fn new(value: T) -> Self {
        RawLock {
            word: AtomicU32::new(0),
            value: UnsafeCell::new(value),
        }
    }

    /// Run `f` with the lock held. Not re-entrant.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        if self
            .word
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.word.swap(2, Ordering::Acquire) != 0 {
                sys::futex_wait(&self.word, 2);
            }
        }
        // SAFETY: the lock is held.
        let r = f(unsafe { &mut *self.value.get() });
        if self.word.swap(0, Ordering::Release) == 2 {
            sys::futex_wake(&self.word, 1);
        }
        r
    }
}
