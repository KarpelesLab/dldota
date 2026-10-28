#![cfg_attr(not(target_arch = "x86_64"), allow(dead_code))] // trampolines are x86_64-only so far

//! The boundary between host and foreign code: which thread pointer each side
//! runs with, and who may be inside foreign code.
//!
//! For now every host thread borrows the TCB of the thread that booted the
//! context, so a re-entrant lock lets one host thread at a time into foreign
//! code (callbacks may re-enter it). The trampolines in `arch` call the
//! `enter_*`/`leave_*` helpers below, always with the host thread pointer set,
//! and park the caller's return address and callee-saved registers in a
//! per-thread frame stack, since the C stack must be left untouched.

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use crate::arch;

/// Shared with the trampolines: field offsets 0 and 8 are fixed.
#[repr(C)]
pub(crate) struct Gate {
    /// Foreign thread pointer of the bootstrap TCB (set once booted).
    pub boot_tp: AtomicUsize,
    /// Host thread pointer of the thread currently borrowing `boot_tp`.
    pub cur_host_tp: AtomicUsize,
    lock: Mutex<Owner>,
    released: Condvar,
}

#[derive(Default)]
struct Owner {
    /// Host thread pointer of the owner, 0 when free.
    tp: usize,
    depth: usize,
}

struct Frame {
    gate: *const Gate,
    ret: usize,
    rbx: usize,
    r12: usize,
    /// Outbound: previous `cur_host_tp`. Inbound: the foreign TP to restore.
    saved_tp: usize,
}

thread_local! {
    static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

impl Gate {
    pub(crate) fn new() -> Self {
        Gate {
            boot_tp: AtomicUsize::new(0),
            cur_host_tp: AtomicUsize::new(0),
            lock: Mutex::new(Owner::default()),
            released: Condvar::new(),
        }
    }

    /// Re-entrant: the owning thread may acquire again.
    pub(crate) fn acquire(&self) {
        let me = arch::thread_pointer();
        let mut owner = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        while owner.tp != 0 && owner.tp != me {
            owner = self.released.wait(owner).unwrap_or_else(|e| e.into_inner());
        }
        owner.tp = me;
        owner.depth += 1;
    }

    pub(crate) fn release(&self) {
        let mut owner = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        owner.depth -= 1;
        if owner.depth == 0 {
            owner.tp = 0;
            self.released.notify_one();
        }
    }

    /// Run `f` with the gate held, so several foreign calls (e.g. `dlopen`
    /// then `dlerror`) see the same borrowed TCB state.
    pub(crate) fn with<R>(&self, f: impl FnOnce() -> R) -> R {
        self.acquire();
        let r = f();
        self.release();
        r
    }
}

#[repr(C)]
pub(crate) struct TpPair {
    foreign: usize,
    host: usize,
}

fn push(frame: Frame) {
    FRAMES.with(|f| f.borrow_mut().push(frame));
}

fn pop() -> Frame {
    FRAMES
        .with(|f| f.borrow_mut().pop())
        .expect("dldota: trampoline frame stack underflow")
}

/// Outbound entry: take the gate, become its borrower, park the caller's state.
pub(crate) extern "C" fn enter_foreign(
    gate: *const Gate,
    ret: usize,
    rbx: usize,
    r12: usize,
) -> TpPair {
    // SAFETY: thunks are built with a pointer to a gate that outlives them.
    let gate = unsafe { &*gate };
    gate.acquire();
    let host = arch::thread_pointer();
    let saved_tp = gate.cur_host_tp.swap(host, Ordering::AcqRel);
    push(Frame {
        gate,
        ret,
        rbx,
        r12,
        saved_tp,
    });
    TpPair {
        foreign: gate.boot_tp.load(Ordering::Acquire),
        host,
    }
}

/// Outbound return: `out` = [return address, rbx, r12].
pub(crate) extern "C" fn leave_foreign(out: *mut [usize; 3]) {
    let frame = pop();
    // SAFETY: as in `enter_foreign`.
    let gate = unsafe { &*frame.gate };
    gate.cur_host_tp.store(frame.saved_tp, Ordering::Release);
    gate.release();
    // SAFETY: the trampoline passes a 3-word scratch area.
    unsafe { out.write([frame.ret, frame.rbx, frame.r12]) };
}

/// Inbound entry (callback), already switched to the host TP.
pub(crate) extern "C" fn enter_host(
    gate: *const Gate,
    ret: usize,
    rbx: usize,
    r12: usize,
    foreign_tp: usize,
) {
    push(Frame {
        gate,
        ret,
        rbx,
        r12,
        saved_tp: foreign_tp,
    });
}

/// Inbound return: `out` = [return address, rbx, r12, foreign TP].
pub(crate) extern "C" fn leave_host(out: *mut [usize; 4]) {
    let frame = pop();
    // SAFETY: the trampoline passes a 4-word scratch area.
    unsafe { out.write([frame.ret, frame.rbx, frame.r12, frame.saved_tp]) };
}
