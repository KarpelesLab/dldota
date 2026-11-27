//! Foreign function pointers obtained at run time rather than through
//! `Library::symbol` (as `vkGetInstanceProcAddr` or `eglGetProcAddress` hand
//! them out): they jump straight into foreign code, so they must be wrapped
//! with `Context::foreign` before the host calls them.

mod common;

use std::ffi::{c_char, c_void};
use std::ptr::NonNull;

use common::{context, global, sym};

type DlSym = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
type Strlen = unsafe extern "C" fn(*const c_char) -> usize;

#[test]
#[cfg_attr(
    not(target_arch = "x86_64"),
    ignore = "loader bootstrap not implemented on this architecture yet"
)]
fn wrap_pointer_returned_by_foreign_code() {
    let ctx = context();
    let libc = global(&ctx);
    // The foreign `dlsym` stands in for any API returning function pointers.
    // SAFETY: dlsym's signature from dlfcn.h.
    let dlsym = unsafe { sym::<DlSym>(&libc, c"dlsym") };
    // SAFETY: a live handle and a valid C string.
    let raw = unsafe { dlsym(libc.as_raw().as_ptr(), c"strlen".as_ptr()) };
    let raw = NonNull::new(raw).expect("strlen in the foreign libc");

    // SAFETY: `raw` is the foreign strlen.
    let strlen = unsafe {
        ctx.foreign(std::mem::transmute::<NonNull<c_void>, Strlen>(raw))
            .expect("wrap strlen")
    };
    assert_ne!(strlen as usize, raw.as_ptr() as usize);
    // SAFETY: a valid C string.
    assert_eq!(unsafe { strlen(c"dldota".as_ptr()) }, 6);

    // One thunk per target, shared with lookups by name.
    let again = ctx.foreign_raw(raw).unwrap();
    assert_eq!(again.as_ptr() as usize, strlen as usize);
    // SAFETY: strlen's signature from string.h.
    let by_name = unsafe { sym::<Strlen>(&libc, c"strlen") };
    assert_eq!(by_name as usize, strlen as usize);

    // Already-wrapped pointers pass through unchanged.
    let rewrapped = ctx.foreign_raw(again).unwrap();
    assert_eq!(rewrapped, again);
}
