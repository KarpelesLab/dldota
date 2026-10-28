//! Per-context services interposed into the foreign world, besides malloc:
//! thread tracking (`pthread_create`) and exit handlers (`__cxa_atexit`,
//! `__cxa_finalize`, `atexit`). Like the heap, this runs on foreign threads
//! with the foreign thread pointer: no host TLS, no std locks, no host
//! allocator. Records live in the context's heap.

use std::ffi::{CStr, c_int, c_void};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use crate::heap::Heap;
use crate::lock::RawLock;
use crate::sys;

const EAGAIN: c_int = 11;

pub(crate) struct Runtime {
    pub heap: Heap,
    lists: RawLock<Lists>,
    /// The foreign libc's own `pthread_create`, called from foreign code.
    real_pthread_create: AtomicUsize,
}

struct Lists {
    threads: *mut ThreadRec,
    exits: *mut ExitRec,
}

// SAFETY: the records are plain data in the context's heap.
unsafe impl Send for Lists {}

struct ThreadRec {
    next: *mut ThreadRec,
    /// Kernel thread id, 0 until the thread has started.
    tid: AtomicI32,
    start: usize,
    arg: usize,
}

struct ExitRec {
    next: *mut ExitRec,
    func: usize,
    arg: usize,
    dso: usize,
}

impl Runtime {
    pub(crate) fn new(page: usize) -> Self {
        Runtime {
            heap: Heap::new(page),
            lists: RawLock::new(Lists {
                threads: std::ptr::null_mut(),
                exits: std::ptr::null_mut(),
            }),
            real_pthread_create: AtomicUsize::new(0),
        }
    }

    pub(crate) fn set_pthread_create(&self, f: usize) {
        self.real_pthread_create.store(f, Ordering::Release);
    }

    fn alloc<T>(&self) -> *mut T {
        self.heap.alloc(size_of::<T>(), align_of::<T>()).cast()
    }

    /// Forget threads that have exited; returns how many may still run.
    pub(crate) fn live_threads(&self) -> usize {
        let mut dead = vec_free_list();
        let live = self.lists.with(|l| {
            let mut live = 0;
            let mut link = &mut l.threads;
            while !link.is_null() {
                // SAFETY: records stay valid until unlinked here.
                let rec = unsafe { &mut **link };
                let tid = rec.tid.load(Ordering::Acquire);
                if tid != 0 && !sys::thread_alive(tid) {
                    let gone = *link;
                    *link = rec.next;
                    dead.push(gone);
                } else {
                    live += 1;
                    link = &mut rec.next;
                }
            }
            live
        });
        for rec in dead.drain() {
            self.heap.free(rec.cast());
        }
        live
    }

    /// Unlink the most recent exit handler (for `dso`, or any when `None`).
    pub(crate) fn pop_exit(&self, dso: Option<usize>) -> Option<(usize, usize)> {
        let rec = self.lists.with(|l| {
            let mut link = &mut l.exits;
            while !link.is_null() {
                // SAFETY: records stay valid until unlinked here.
                let rec = unsafe { &mut **link };
                if dso.is_none_or(|d| d == rec.dso) {
                    let found = *link;
                    *link = rec.next;
                    return Some(found);
                }
                link = &mut rec.next;
            }
            None
        })?;
        // SAFETY: unlinked above; ours to read and free.
        let (func, arg) = unsafe { ((*rec).func, (*rec).arg) };
        self.heap.free(rec.cast());
        Some((func, arg))
    }
}

/// A fixed-capacity stack of pointers that needs no allocator (we may be on
/// a foreign thread); overflow just leaves records for the next sweep.
struct FreeList {
    items: [*mut ThreadRec; 32],
    len: usize,
}

fn vec_free_list() -> FreeList {
    FreeList {
        items: [std::ptr::null_mut(); 32],
        len: 0,
    }
}

impl FreeList {
    fn push(&mut self, rec: *mut ThreadRec) {
        if self.len < self.items.len() {
            self.items[self.len] = rec;
            self.len += 1;
        }
    }
    fn drain(&mut self) -> impl Iterator<Item = *mut ThreadRec> + '_ {
        let len = std::mem::take(&mut self.len);
        self.items[..len].iter().copied()
    }
}

/// Start routine of every thread created by foreign code: records the
/// thread id, then runs the real start routine. Runs on the new thread.
extern "C" fn thread_start(rec: *mut ThreadRec) -> *mut c_void {
    // SAFETY: the record outlives the thread (freed once it has exited).
    let rec = unsafe { &*rec };
    rec.tid.store(sys::gettid(), Ordering::Release);
    // SAFETY: the start routine foreign code passed to pthread_create.
    unsafe {
        let start: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
            std::mem::transmute(rec.start);
        start(rec.arg as *mut c_void)
    }
}

extern "C" fn pthread_create(
    rt: &Runtime,
    thread: *mut c_void,
    attr: *const c_void,
    start: usize,
    arg: usize,
) -> c_int {
    rt.live_threads(); // sweep exited threads
    let real = rt.real_pthread_create.load(Ordering::Acquire);
    let rec = rt.alloc::<ThreadRec>();
    if real == 0 || rec.is_null() {
        rt.heap.free(rec.cast());
        return EAGAIN;
    }
    // SAFETY: a fresh allocation of the right size.
    unsafe {
        rec.write(ThreadRec {
            next: std::ptr::null_mut(),
            tid: AtomicI32::new(0),
            start,
            arg,
        })
    };
    type Create = unsafe extern "C" fn(
        *mut c_void,
        *const c_void,
        extern "C" fn(*mut ThreadRec) -> *mut c_void,
        *mut ThreadRec,
    ) -> c_int;
    // SAFETY: the foreign pthread_create, called from foreign code.
    let rc = unsafe { std::mem::transmute::<usize, Create>(real)(thread, attr, thread_start, rec) };
    if rc != 0 {
        rt.heap.free(rec.cast());
        return rc;
    }
    rt.lists.with(|l| {
        // SAFETY: ours until linked.
        unsafe { (*rec).next = l.threads };
        l.threads = rec;
    });
    0
}

extern "C" fn cxa_atexit(rt: &Runtime, func: usize, arg: usize, dso: usize) -> c_int {
    let rec = rt.alloc::<ExitRec>();
    if rec.is_null() {
        return -1;
    }
    rt.lists.with(|l| {
        // SAFETY: a fresh allocation of the right size.
        unsafe {
            rec.write(ExitRec {
                next: l.exits,
                func,
                arg,
                dso,
            })
        };
        l.exits = rec;
    });
    0
}

extern "C" fn atexit(rt: &Runtime, func: usize) -> c_int {
    cxa_atexit(rt, func, 0, 0)
}

/// Run (and forget) the exit handlers registered for `dso`, or all of them
/// for NULL. Called by foreign code, e.g. from a library's destructors.
extern "C" fn cxa_finalize(rt: &Runtime, dso: usize) {
    while let Some((func, arg)) = rt.pop_exit((dso != 0).then_some(dso)) {
        // SAFETY: a handler foreign code registered; we run in foreign context.
        unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(usize)>(func)(arg) };
    }
}

/// The interposed symbols; each thunk prepends the runtime.
pub(crate) fn exports() -> [(&'static CStr, usize); 4] {
    [
        (c"pthread_create", pthread_create as *const () as usize),
        (c"__cxa_atexit", cxa_atexit as *const () as usize),
        (c"__cxa_finalize", cxa_finalize as *const () as usize),
        (c"atexit", atexit as *const () as usize),
    ]
}
