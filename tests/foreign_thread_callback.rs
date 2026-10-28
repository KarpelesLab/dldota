//! A host callback running on a thread the foreign libc created. Such a
//! thread has no host TCB, so this is an open design question; today it
//! aborts the process, hence its own test binary.

mod common;

use std::cell::Cell;
use std::ffi::{c_int, c_void};

use common::{context, global, sym};

type StartRoutine = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
// pthread_t is `unsigned long` in glibc and a pointer in musl: both usize.
type PthreadCreate =
    unsafe extern "C" fn(*mut usize, *const c_void, StartRoutine, *mut c_void) -> c_int;
type PthreadJoin = unsafe extern "C" fn(usize, *mut *mut c_void) -> c_int;

thread_local! {
    static CALLS: Cell<usize> = const { Cell::new(0) };
}

/// Runs on a thread created by the foreign libc, which has no host TCB:
/// uses host TLS and the host allocator, and hands a heap value back.
unsafe extern "C" fn host_work(arg: *mut c_void) -> *mut c_void {
    CALLS.with(|c| c.set(c.get() + 1));
    let seed = arg as usize;
    let text: String = (0..1000)
        .map(|i| char::from(b'a' + ((seed + i) % 26) as u8))
        .collect();
    let summary = format!("{}:{}", CALLS.with(Cell::get), text.len());
    Box::into_raw(Box::new(summary)).cast()
}

#[test]
#[ignore = "callbacks on foreign-created threads need a host TCB (open design question)"]
fn callback_on_foreign_thread() {
    let ctx = context();
    let libc = global(&ctx);
    // SAFETY: signatures from pthread.h.
    let (create, join) = unsafe {
        (
            sym::<PthreadCreate>(&libc, c"pthread_create"),
            sym::<PthreadJoin>(&libc, c"pthread_join"),
        )
    };
    // SAFETY: StartRoutine is an extern "C" fn pointer type.
    let start = unsafe { ctx.callback::<StartRoutine>(host_work) }.expect("create callback");

    let threads: Vec<usize> = (0..8)
        .map(|seed| {
            let mut tid = 0usize;
            // SAFETY: default attributes; `start` outlives the threads (joined below).
            let rc = unsafe { create(&mut tid, std::ptr::null(), *start, seed as *mut c_void) };
            assert_eq!(rc, 0, "pthread_create");
            tid
        })
        .collect();
    for tid in threads {
        let mut ret = std::ptr::null_mut();
        // SAFETY: joining a thread we created, once.
        assert_eq!(unsafe { join(tid, &mut ret) }, 0, "pthread_join");
        // SAFETY: host_work returns a Box<String>.
        let summary = unsafe { Box::from_raw(ret.cast::<String>()) };
        // Each foreign thread starts with fresh host TLS.
        assert_eq!(*summary, "1:1000");
    }
}
