//! Foreign code calling back into the host: every inbound call must switch the
//! thread pointer back to the host's TCB, or host TLS and the host allocator
//! break.

mod common;

use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::OnceLock;

use common::{context, global, sym};

type Compar = unsafe extern "C" fn(*const c_void, *const c_void) -> c_int;
type Qsort = unsafe extern "C" fn(*mut c_void, usize, usize, Compar);
type Strcmp = unsafe extern "C" fn(*const c_char, *const c_char) -> c_int;

thread_local! {
    static CALLS: Cell<usize> = const { Cell::new(0) };
}

/// Descending order; counts its calls in host TLS, which only works if the
/// thread pointer was switched back to the host.
unsafe extern "C" fn descending(a: *const c_void, b: *const c_void) -> c_int {
    CALLS.with(|c| c.set(c.get() + 1));
    // SAFETY: qsort hands us pointers to elements of the i32 array.
    let (a, b) = unsafe { (*a.cast::<i32>(), *b.cast::<i32>()) };
    b.cmp(&a) as c_int
}

#[test]
#[ignore = "callback trampolines not verified yet"]
fn qsort_with_host_comparator() {
    let ctx = context();
    let libc = global(&ctx);
    // SAFETY: qsort's signature from stdlib.h.
    let qsort = unsafe { sym::<Qsort>(&libc, c"qsort") };
    // SAFETY: Compar is an extern "C" fn pointer type.
    let cmp = unsafe { ctx.callback::<Compar>(descending) }.expect("create callback");

    let mut values: Vec<i32> = (0..10_000).map(|i| (i * 7919 % 10_007) - 5000).collect();
    let mut expected = values.clone();
    expected.sort_by(|a, b| b.cmp(a));

    CALLS.with(|c| c.set(0));
    // SAFETY: element size and count match the buffer.
    unsafe {
        qsort(
            values.as_mut_ptr().cast(),
            values.len(),
            size_of::<i32>(),
            *cmp,
        )
    };
    assert_eq!(values, expected);
    let calls = CALLS.with(Cell::get);
    assert!(
        calls >= values.len() - 1,
        "comparator ran only {calls} times"
    );
}

static STRCMP: OnceLock<Strcmp> = OnceLock::new();

/// Host comparator that itself calls foreign `strcmp`: host → foreign →
/// host → foreign on one thread, so any per-context lock must be re-entrant.
unsafe extern "C" fn by_foreign_strcmp(a: *const c_void, b: *const c_void) -> c_int {
    let strcmp = STRCMP.get().expect("strcmp resolved");
    // SAFETY: qsort hands us pointers to elements of the string pointer array.
    unsafe { strcmp(*a.cast::<*const c_char>(), *b.cast::<*const c_char>()) }
}

#[test]
#[ignore = "callback trampolines not verified yet"]
fn reentrant_foreign_call_from_callback() {
    let ctx = context();
    let libc = global(&ctx);
    // SAFETY: signatures from stdlib.h and string.h.
    let qsort = unsafe { sym::<Qsort>(&libc, c"qsort") };
    STRCMP.get_or_init(|| unsafe { sym::<Strcmp>(&libc, c"strcmp") });
    // SAFETY: Compar is an extern "C" fn pointer type.
    let cmp = unsafe { ctx.callback::<Compar>(by_foreign_strcmp) }.expect("create callback");

    let words: Vec<CString> = (0..2000)
        .map(|i| CString::new(format!("w{:05}", i * 7919 % 10_007)).unwrap())
        .collect();
    let mut ptrs: Vec<*const c_char> = words.iter().map(|w| w.as_ptr()).collect();
    // SAFETY: element size and count match the buffer; strings outlive the sort.
    unsafe {
        qsort(
            ptrs.as_mut_ptr().cast(),
            ptrs.len(),
            size_of::<*const c_char>(),
            *cmp,
        )
    };

    // SAFETY: every pointer still points into `words`.
    let sorted: Vec<&CStr> = ptrs.iter().map(|&p| unsafe { CStr::from_ptr(p) }).collect();
    assert!(sorted.is_sorted(), "not sorted by foreign strcmp");
    assert_eq!(sorted.len(), words.len());
}
