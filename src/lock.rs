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

// Unwinding out of `with` aborts the process (see `AbortOnUnwind`), so a
// half-done update can never be observed and there is nothing to poison.
// Poisoning wouldn't help anyway: most callers are foreign C code (malloc,
// pthread_create, ...) that couldn't act on it.
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
        let guard = AbortOnUnwind;
        // SAFETY: the lock is held.
        let r = f(unsafe { &mut *self.value.get() });
        std::mem::forget(guard);
        if self.word.swap(0, Ordering::Release) == 2 {
            sys::futex_wake(&self.word, 1);
        }
        r
    }
}

/// Dropped only if `f` unwinds: the guarded state may be half-updated, so
/// stop right here. Can't use `std::thread::panicking()`, which reads host
/// TLS, and this may run on a foreign thread.
struct AbortOnUnwind;

impl Drop for AbortOnUnwind {
    fn drop(&mut self) {
        let _ = sys::write(2, b"dldota: panic while holding an internal lock\n");
        crate::arch::trap()
    }
}
