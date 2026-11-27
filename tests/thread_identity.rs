//! Each host thread runs foreign code as a foreign thread of its own: its
//! own `pthread_self`, `errno` and thread-local data, concurrently with the
//! other host threads, until it exits.

mod common;

use std::ffi::{c_int, c_void};
use std::sync::mpsc;

use common::{context, global, settled_thread_count, sym, thread_count};

// pthread_t is `unsigned long` in glibc and a pointer in musl: both usize.
type PthreadSelf = unsafe extern "C" fn() -> usize;
type ErrnoLocation = unsafe extern "C" fn() -> *mut c_int;
type Close = unsafe extern "C" fn(c_int) -> c_int;
type Pipe = unsafe extern "C" fn(*mut [c_int; 2]) -> c_int;
type Read = unsafe extern "C" fn(c_int, *mut c_void, usize) -> isize;
type Write = unsafe extern "C" fn(c_int, *const c_void, usize) -> isize;

/// The tests count this process's threads: one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const EBADF: c_int = 9;
const THREADS: usize = 8;

#[test]
#[cfg_attr(
    not(target_arch = "x86_64"),
    ignore = "loader bootstrap not implemented on this architecture yet"
)]
fn host_threads_are_distinct_foreign_threads() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let ctx = context();
    let libc = global(&ctx);
    // SAFETY: signatures from pthread.h, errno.h and unistd.h.
    let (pthread_self, errno_location, close) = unsafe {
        (
            sym::<PthreadSelf>(&libc, c"pthread_self"),
            sym::<ErrnoLocation>(&libc, c"__errno_location"),
            sym::<Close>(&libc, c"close"),
        )
    };
    // (pthread_self, errno address) as seen by the calling host thread.
    // SAFETY: no preconditions.
    let identity = move || unsafe { (pthread_self(), errno_location() as usize) };

    let me = identity();
    assert_eq!(me, identity(), "a thread keeps its foreign thread");

    let baseline = thread_count();
    let (seen_tx, seen_rx) = mpsc::channel();
    std::thread::scope(|s| {
        let (go_tx, go_rx) = mpsc::channel::<()>();
        // The first thread fails a call and checks its errno survives the
        // others' calls; the others check they start with a clean errno.
        let seen = seen_tx.clone();
        s.spawn(move || {
            // SAFETY: closing an invalid descriptor only sets errno.
            assert_eq!(unsafe { close(-1) }, -1);
            seen.send(identity()).unwrap();
            go_rx.recv().unwrap();
            // SAFETY: this thread's errno.
            assert_eq!(unsafe { *errno_location() }, EBADF);
        });
        std::thread::scope(|inner| {
            for _ in 1..THREADS {
                let seen = seen_tx.clone();
                inner.spawn(move || {
                    // SAFETY: this thread's errno.
                    assert_eq!(unsafe { *errno_location() }, 0);
                    // SAFETY: closing an invalid descriptor only sets errno.
                    assert_eq!(unsafe { close(-1) }, -1);
                    seen.send(identity()).unwrap();
                    assert_eq!(identity(), identity());
                });
            }
        });
        go_tx.send(()).unwrap();
    });
    drop(seen_tx);

    let mut all: Vec<(usize, usize)> = seen_rx.iter().collect();
    assert_eq!(all.len(), THREADS);
    all.push(me);
    let distinct = |f: fn(&(usize, usize)) -> usize| {
        let mut v: Vec<usize> = all.iter().map(f).collect();
        v.sort_unstable();
        v.dedup();
        v.len()
    };
    // Threads of the inner scope may reuse an exited one's TCB address, but
    // those alive together cannot: the first thread, this one, and at least
    // one more.
    assert!(distinct(|i| i.0) >= 2, "pthread_self shared: {all:x?}");
    assert!(distinct(|i| i.1) >= 2, "errno shared: {all:x?}");
    assert_ne!(all[0], me);

    // The foreign threads exit with their host threads.
    assert_eq!(settled_thread_count(baseline), baseline);
    // SAFETY: this thread's errno, untouched by the others' failures.
    assert_eq!(unsafe { *errno_location() }, 0);
}

/// A host thread blocked inside a foreign call doesn't keep the others out.
#[test]
#[cfg_attr(
    not(target_arch = "x86_64"),
    ignore = "loader bootstrap not implemented on this architecture yet"
)]
fn foreign_calls_run_concurrently() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let ctx = context();
    let libc = global(&ctx);
    // SAFETY: signatures from unistd.h.
    let (pipe, read, write, close) = unsafe {
        (
            sym::<Pipe>(&libc, c"pipe"),
            sym::<Read>(&libc, c"read"),
            sym::<Write>(&libc, c"write"),
            sym::<Close>(&libc, c"close"),
        )
    };
    let mut fds = [0; 2];
    // SAFETY: a valid two-element array.
    assert_eq!(unsafe { pipe(&mut fds) }, 0);
    let [rd, wr] = fds;

    std::thread::scope(|s| {
        let (entering_tx, entering_rx) = mpsc::channel();
        let reader = s.spawn(move || {
            let mut byte = 0u8;
            entering_tx.send(()).unwrap();
            // Blocks in the foreign libc until the main thread writes.
            // SAFETY: a one-byte buffer.
            let n = unsafe { read(rd, (&raw mut byte).cast(), 1) };
            (n, byte)
        });
        entering_rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // SAFETY: a one-byte buffer.
        assert_eq!(unsafe { write(wr, c"x".as_ptr().cast(), 1) }, 1);
        assert_eq!(reader.join().unwrap(), (1, b'x'));
    });
    // SAFETY: descriptors from pipe().
    unsafe {
        close(rd);
        close(wr);
    }
}
