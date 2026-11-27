#![cfg_attr(not(target_arch = "x86_64"), allow(dead_code))] // trampolines are x86_64-only so far

//! The boundary between host and foreign code: which thread pointer each side
//! runs with.
//!
//! Each host thread that enters foreign code gets a foreign thread of its
//! own: a thread created by the foreign libc's `pthread_create` that parks
//! right away (a *donor*). The host thread runs foreign code with the donor's
//! thread pointer, so it has its own `pthread_self`, `errno`, `dlerror` and
//! `__thread` variables, and host threads run foreign code concurrently. The
//! donor exits, running the foreign thread-local destructors, when its host
//! thread does.
//!
//! The TCB of the thread that booted the context (the foreign "main thread")
//! is only borrowed, under a re-entrant lock: to create donors, to run exit
//! handlers at unload, and for every call when donors are unavailable (a
//! libc without the needed `pthread_*` functions, or thread creation failing).
//!
//! The trampolines in `arch` call the `enter_*`/`leave_*` helpers below,
//! always with the host thread pointer set, and park the caller's return
//! address and callee-saved registers in a per-thread frame stack, since the
//! C stack must be left untouched.

use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::arch;
use crate::sys;

/// Shared with the trampolines: the offsets of the first four fields are fixed.
#[repr(C)]
pub(crate) struct Gate {
    /// Foreign thread pointer of the bootstrap TCB (set once booted).
    pub boot_tp: AtomicUsize,
    /// Host thread pointer of the thread currently borrowing `boot_tp`. Its
    /// address is the bootstrap TCB's value for `key`, as a donor's
    /// [`Donor::host_tp`] is for its own.
    pub cur_host_tp: AtomicUsize,
    /// The foreign `pthread_getspecific`, or 0 without donor support: how a
    /// callback finds the host thread pointer to switch to.
    getspecific: AtomicUsize,
    /// The foreign thread-specific key (`pthread_key_t`) holding it.
    key: AtomicUsize,
    /// Never reused, unlike the gate's address: identifies it in [`STATE`].
    id: u64,
    lock: Mutex<Owner>,
    released: Condvar,
    threads: OnceLock<ThreadSupport>,
    /// Set while unloading: every call borrows the bootstrap TCB.
    closing: AtomicBool,
    /// Every donor whose thread may still be running.
    donors: Mutex<Vec<Arc<Donor>>>,
}

#[derive(Default)]
struct Owner {
    /// Host thread pointer of the owner, 0 when free.
    tp: usize,
    depth: usize,
}

type StartRoutine = extern "C" fn(*const Donor) -> *mut c_void;
type PthreadCreate =
    unsafe extern "C" fn(*mut usize, *const c_void, StartRoutine, *const Donor) -> c_int;
type PthreadJoin = unsafe extern "C" fn(usize, *mut *mut c_void) -> c_int;
type PthreadKeyCreate = unsafe extern "C" fn(*mut u32, usize) -> c_int;
type PthreadSetspecific = unsafe extern "C" fn(u32, *const c_void) -> c_int;

/// Addresses of the foreign libc's thread functions.
#[derive(Default)]
pub(crate) struct PthreadFns {
    pub create: usize,
    pub join: usize,
    pub key_create: usize,
    pub setspecific: usize,
    pub getspecific: usize,
}

/// What creating a donor takes.
struct ThreadSupport {
    /// Outbound thunks, called from the host.
    create: PthreadCreate,
    join: PthreadJoin,
    /// The raw function, called by the donor itself.
    setspecific: usize,
    key: u32,
}

const STARTING: u32 = 0;
const PARKED: u32 = 1;
const EXIT: u32 = 2;

/// A parked foreign thread whose TCB one host thread runs foreign code with.
#[repr(C)]
struct Donor {
    /// The owning host thread's pointer. Offset 0: read by the inbound
    /// trampoline through the foreign thread-specific key.
    host_tp: usize,
    foreign_tp: AtomicUsize,
    /// Futex word: `STARTING`, `PARKED` or `EXIT`.
    state: AtomicU32,
    /// Kernel thread id, valid once parked.
    tid: AtomicI32,
    /// The foreign `pthread_t`, to join the thread once it has exited: only
    /// then does the foreign libc release its stack.
    thread: AtomicUsize,
    setspecific: usize,
    key: u32,
}

impl Donor {
    fn parked(&self) -> bool {
        self.state.load(Ordering::Acquire) == PARKED
    }

    /// Let the thread exit. It no longer touches `self` once it has seen this.
    fn retire(&self) {
        self.state.store(EXIT, Ordering::Release);
        sys::futex_wake(&self.state, 1);
    }

    fn gone(&self) -> bool {
        !self.parked() && !sys::thread_alive(self.tid.load(Ordering::Acquire))
    }
}

/// Start routine of a donor. Runs on the new thread, with its foreign thread
/// pointer: no host TLS, no host allocator.
extern "C" fn park(donor: *const Donor) -> *mut c_void {
    // SAFETY: the record outlives the thread: the gate holds a reference
    // until the thread is gone.
    let d = unsafe { &*donor };
    // SAFETY: the foreign `pthread_setspecific`, called from foreign code.
    let setspecific = unsafe { std::mem::transmute::<usize, PthreadSetspecific>(d.setspecific) };
    // SAFETY: a key created at boot.
    unsafe { setspecific(d.key, donor.cast()) };
    d.tid.store(sys::gettid(), Ordering::Release);
    d.foreign_tp
        .store(arch::thread_pointer(), Ordering::Release);
    // Fails only if the context is already being unloaded.
    let _ = d
        .state
        .compare_exchange(STARTING, PARKED, Ordering::AcqRel, Ordering::Acquire);
    sys::futex_wake(&d.state, 1);
    while d.state.load(Ordering::Acquire) == PARKED {
        sys::futex_wait(&d.state, PARKED);
    }
    // The host thread is gone: a callback from a thread-local destructor
    // must find no host thread pointer rather than a stale one.
    // SAFETY: as above.
    unsafe { setspecific(d.key, std::ptr::null()) };
    std::ptr::null_mut()
}

struct Frame {
    gate: *const Gate,
    ret: usize,
    rbx: usize,
    r12: usize,
    /// Outbound on the bootstrap TCB: previous `cur_host_tp`. Inbound: the
    /// foreign TP to restore.
    saved_tp: usize,
    /// Outbound: the call borrows the bootstrap TCB and holds the lock.
    locked: bool,
}

/// A host thread's foreign thread in one context.
struct Slot {
    gate: u64,
    /// `None` while the donor is being created, or if that failed: the
    /// thread borrows the bootstrap TCB.
    donor: Option<Arc<Donor>>,
}

struct ThreadState {
    frames: Vec<Frame>,
    slots: Vec<Slot>,
}

impl Drop for ThreadState {
    fn drop(&mut self) {
        // The host thread exits: so do its foreign threads.
        for donor in self.slots.iter().filter_map(|s| s.donor.as_ref()) {
            donor.retire();
        }
    }
}

thread_local! {
    static STATE: RefCell<ThreadState> = const {
        RefCell::new(ThreadState {
            frames: Vec::new(),
            slots: Vec::new(),
        })
    };
}

/// Every signal but the ones glibc keeps for itself (32-34: cancellation,
/// setxid). A donor must never run a host signal handler: it has no host TCB.
const DONOR_SIGMASK: u64 = !(0b111 << 31);

/// How long unload waits for donors to exit.
const RETIRE_TIMEOUT: Duration = Duration::from_secs(10);

impl Gate {
    pub(crate) fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Gate {
            boot_tp: AtomicUsize::new(0),
            cur_host_tp: AtomicUsize::new(0),
            getspecific: AtomicUsize::new(0),
            key: AtomicUsize::new(0),
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            lock: Mutex::new(Owner::default()),
            released: Condvar::new(),
            threads: OnceLock::new(),
            closing: AtomicBool::new(false),
            donors: Mutex::new(Vec::new()),
        }
    }

    /// Give each host thread its own foreign thread from now on. `thunk`
    /// makes a foreign function callable from the host. Does nothing when a
    /// function is missing; calls then keep sharing the bootstrap TCB.
    pub(crate) fn enable_threads(
        &self,
        fns: &PthreadFns,
        mut thunk: impl FnMut(usize) -> std::io::Result<usize>,
    ) {
        if [
            fns.create,
            fns.join,
            fns.key_create,
            fns.setspecific,
            fns.getspecific,
        ]
        .contains(&0)
        {
            debug!("foreign libc lacks pthread functions: host threads share one foreign thread");
            return;
        }
        let (Ok(create), Ok(join), Ok(key_create), Ok(setspecific)) = (
            thunk(fns.create),
            thunk(fns.join),
            thunk(fns.key_create),
            thunk(fns.setspecific),
        ) else {
            return;
        };
        let mut key = 0u32;
        // SAFETY: thunks to the foreign functions of these signatures. No
        // donor exists yet, so both run on the bootstrap TCB, whose value
        // for the key becomes `cur_host_tp`.
        let ok = unsafe {
            std::mem::transmute::<usize, PthreadKeyCreate>(key_create)(&mut key, 0) == 0
                && std::mem::transmute::<usize, PthreadSetspecific>(setspecific)(
                    key,
                    (&raw const self.cur_host_tp).cast(),
                ) == 0
        };
        if !ok {
            return;
        }
        self.key.store(key as usize, Ordering::Release);
        self.getspecific.store(fns.getspecific, Ordering::Release);
        // SAFETY: thunks to the foreign functions of these signatures.
        let support = unsafe {
            ThreadSupport {
                create: std::mem::transmute::<usize, PthreadCreate>(create),
                join: std::mem::transmute::<usize, PthreadJoin>(join),
                setspecific: fns.setspecific,
                key,
            }
        };
        let _ = self.threads.set(support);
    }

    /// Take the bootstrap TCB. Re-entrant: the owning thread may acquire again.
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

    /// Run `f` so that its foreign calls (e.g. `dlopen` then `dlerror`) see
    /// the same foreign thread state, untouched by other host threads.
    pub(crate) fn with<R>(&self, f: impl FnOnce() -> R) -> R {
        if self.thread_tp(arch::thread_pointer()).is_some() {
            return f();
        }
        self.acquire();
        let r = f();
        self.release();
        r
    }

    /// The thread pointer of the calling host thread's own foreign thread,
    /// created on first use. `None` when it must borrow the bootstrap TCB.
    fn thread_tp(&self, host: usize) -> Option<usize> {
        let support = self.threads.get()?;
        if self.closing.load(Ordering::Acquire) {
            return None;
        }
        // `Err(true)`: this thread has no (live) donor in this context yet.
        let found = STATE.with(|s| {
            match s.borrow().slots.iter().find(|slot| slot.gate == self.id) {
                Some(Slot { donor: Some(d), .. }) if d.parked() => {
                    Ok(d.foreign_tp.load(Ordering::Acquire))
                }
                // Retired by an unload that then failed: start over.
                Some(Slot { donor: Some(_), .. }) | None => Err(true),
                Some(Slot { donor: None, .. }) => Err(false),
            }
        });
        match found {
            Ok(tp) => Some(tp),
            Err(false) => None,
            Err(true) => {
                // Calls made while creating the donor find the empty slot
                // and borrow the bootstrap TCB.
                self.set_slot(None);
                let donor = self.spawn(support, host);
                let tp = donor.as_ref().map(|d| d.foreign_tp.load(Ordering::Acquire));
                self.set_slot(donor);
                tp
            }
        }
    }

    fn set_slot(&self, donor: Option<Arc<Donor>>) {
        STATE.with(|s| {
            let slots = &mut s.borrow_mut().slots;
            // Forget contexts that were unloaded since.
            slots.retain(|slot| {
                slot.gate != self.id && slot.donor.as_ref().is_some_and(|d| d.parked())
            });
            slots.push(Slot {
                gate: self.id,
                donor,
            });
        });
    }

    /// Join the donors whose thread has exited, and forget them. Call with
    /// the bootstrap TCB held and this thread's calls going to it.
    fn reap(&self, support: &ThreadSupport, donors: &mut Vec<Arc<Donor>>) {
        donors.retain(|d| {
            let gone = d.gone();
            if gone {
                // SAFETY: the foreign `pthread_join`, through a thunk, on a
                // thread that has exited and was not joined yet.
                unsafe { (support.join)(d.thread.load(Ordering::Acquire), std::ptr::null_mut()) };
            }
            !gone
        });
    }

    /// Create a parked foreign thread for the host thread `host`.
    fn spawn(&self, support: &ThreadSupport, host: usize) -> Option<Arc<Donor>> {
        // Hold the bootstrap TCB across the foreign calls. They find this
        // thread's empty slot, so they run on that TCB.
        self.acquire();
        let mut donors = self.donors.lock().unwrap_or_else(|e| e.into_inner());
        self.reap(support, &mut donors);
        let donor = Arc::new(Donor {
            host_tp: host,
            foreign_tp: AtomicUsize::new(0),
            state: AtomicU32::new(STARTING),
            tid: AtomicI32::new(0),
            thread: AtomicUsize::new(0),
            setspecific: support.setspecific,
            key: support.key,
        });
        // The new thread inherits the mask.
        let mask = sys::sigprocmask_set(DONOR_SIGMASK);
        let mut thread = 0usize;
        // SAFETY: the foreign `pthread_create`, through a thunk; `park` only
        // uses the record, which `donors` keeps alive until it is joined.
        let rc =
            unsafe { (support.create)(&mut thread, std::ptr::null(), park, Arc::as_ptr(&donor)) };
        if let Ok(mask) = mask {
            let _ = sys::sigprocmask_set(mask);
        }
        if rc == 0 {
            donor.thread.store(thread, Ordering::Release);
            donors.push(donor.clone());
        }
        drop(donors);
        self.release();
        if rc != 0 {
            debug!("foreign pthread_create failed ({rc}): sharing the bootstrap thread");
            return None;
        }
        while donor.state.load(Ordering::Acquire) == STARTING {
            sys::futex_wait(&donor.state, STARTING);
        }
        debug!(
            "foreign thread {} for host thread {}",
            donor.tid.load(Ordering::Relaxed),
            sys::gettid()
        );
        Some(donor)
    }

    /// For unload, with the bootstrap TCB held: make every donor exit, as
    /// threads do before a process's exit handlers run, and send all calls
    /// to the bootstrap TCB from now on. Fails if a donor is still running after a while (stuck in a
    /// thread-local destructor); [`Gate::reopen`] then undoes this.
    pub(crate) fn close_threads(&self) -> Result<(), usize> {
        self.closing.store(true, Ordering::Release);
        let Some(support) = self.threads.get() else {
            return Ok(());
        };
        let mut donors = self.donors.lock().unwrap_or_else(|e| e.into_inner());
        for donor in donors.iter() {
            donor.retire();
        }
        let deadline = Instant::now() + RETIRE_TIMEOUT;
        loop {
            self.reap(support, &mut donors);
            if donors.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(donors.len());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Host threads get foreign threads again, after a failed unload.
    pub(crate) fn reopen(&self) {
        self.closing.store(false, Ordering::Release);
    }
}

#[repr(C)]
pub(crate) struct TpPair {
    foreign: usize,
    host: usize,
}

fn push(frame: Frame) {
    STATE.with(|s| s.borrow_mut().frames.push(frame));
}

fn pop() -> Frame {
    STATE
        .with(|s| s.borrow_mut().frames.pop())
        .expect("dldota: trampoline frame stack underflow")
}

/// Outbound entry: pick the foreign thread to run as, park the caller's state.
pub(crate) extern "C" fn enter_foreign(
    gate: *const Gate,
    ret: usize,
    rbx: usize,
    r12: usize,
) -> TpPair {
    // SAFETY: thunks are built with a pointer to a gate that outlives them.
    let gate = unsafe { &*gate };
    let host = arch::thread_pointer();
    let (foreign, locked, saved_tp) = match gate.thread_tp(host) {
        Some(tp) => (tp, false, 0),
        None => {
            gate.acquire();
            let saved_tp = gate.cur_host_tp.swap(host, Ordering::AcqRel);
            (gate.boot_tp.load(Ordering::Acquire), true, saved_tp)
        }
    };
    push(Frame {
        gate,
        ret,
        rbx,
        r12,
        saved_tp,
        locked,
    });
    TpPair { foreign, host }
}

/// Outbound return: `out` = [return address, rbx, r12].
pub(crate) extern "C" fn leave_foreign(out: *mut [usize; 3]) {
    let frame = pop();
    if frame.locked {
        // SAFETY: as in `enter_foreign`.
        let gate = unsafe { &*frame.gate };
        gate.cur_host_tp.store(frame.saved_tp, Ordering::Release);
        gate.release();
    }
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
        locked: false,
    });
}

/// Inbound return: `out` = [return address, rbx, r12, foreign TP].
pub(crate) extern "C" fn leave_host(out: *mut [usize; 4]) {
    let frame = pop();
    // SAFETY: the trampoline passes a 4-word scratch area.
    unsafe { out.write([frame.ret, frame.rbx, frame.r12, frame.saved_tp]) };
}
