//! C ABI exported by `libdldota.a`; mirrored by `include/dldota.h`.
//!
//! Errors follow the `dlerror` convention: functions return NULL / non-zero,
//! and `dldota_error()` returns the calling thread's last message (or NULL).

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::ptr::{self, NonNull};

use crate::{Context, Error, OpenFlags};

/// Opaque context handle (`dldota_ctx` in C).
pub type DldotaCtx = Context;

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_error(e: Error) {
    let msg = CString::new(e.to_string().replace('\0', "?")).unwrap_or_default();
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(msg));
}

fn ok_or_null<T>(r: crate::Result<NonNull<T>>) -> *mut T {
    r.map_or_else(
        |e| {
            set_error(e);
            ptr::null_mut()
        },
        NonNull::as_ptr,
    )
}

/// Create a context. `interpreter` may be NULL to auto-detect the system
/// dynamic loader. Returns NULL on failure.
///
/// # Safety
/// `interpreter` must be NULL or a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dldota_ctx_new(interpreter: *const c_char) -> *mut DldotaCtx {
    let mut builder = Context::builder();
    if !interpreter.is_null() {
        // SAFETY: caller contract.
        let path = unsafe { CStr::from_ptr(interpreter) };
        builder = builder.interpreter(std::ffi::OsStr::from_bytes(path.to_bytes()));
    }
    ok_or_null(
        builder
            .build()
            .map(|ctx| NonNull::from(Box::leak(Box::new(ctx)))),
    )
}

/// Release a context handle. The loader itself stays mapped.
///
/// # Safety
/// `ctx` must come from `dldota_ctx_new` and not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dldota_ctx_free(ctx: *mut DldotaCtx) {
    if !ctx.is_null() {
        // SAFETY: caller contract.
        drop(unsafe { Box::from_raw(ctx) });
    }
}

/// `dlopen` through the context. `name` may be NULL for the global scope.
///
/// # Safety
/// `ctx` must be valid; `name` NULL or a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dldota_open(
    ctx: *mut DldotaCtx,
    name: *const c_char,
    flags: c_int,
) -> *mut c_void {
    // SAFETY: caller contract.
    let (ctx, name) = unsafe { (&*ctx, (!name.is_null()).then(|| CStr::from_ptr(name))) };
    ok_or_null(ctx.open_raw(name, OpenFlags(flags)))
}

/// `dlsym` through the context. The returned function pointer is safe to
/// call directly from host code.
///
/// # Safety
/// `ctx` must be valid, `handle` from `dldota_open`, `name` a valid string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dldota_sym(
    ctx: *mut DldotaCtx,
    handle: *mut c_void,
    name: *const c_char,
) -> *mut c_void {
    let Some(handle) = NonNull::new(handle) else {
        set_error(Error::Dl("NULL handle".into()));
        return ptr::null_mut();
    };
    // SAFETY: caller contract.
    let (ctx, name) = unsafe { (&*ctx, CStr::from_ptr(name)) };
    ok_or_null(ctx.symbol_raw(handle, name))
}

/// `dlclose` through the context. Returns 0 on success.
///
/// # Safety
/// `ctx` must be valid and `handle` from `dldota_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dldota_close(ctx: *mut DldotaCtx, handle: *mut c_void) -> c_int {
    let Some(handle) = NonNull::new(handle) else {
        return -1;
    };
    // SAFETY: caller contract.
    match unsafe { &*ctx }.close_raw(handle) {
        Ok(()) => 0,
        Err(e) => {
            set_error(e);
            -1
        }
    }
}

/// Wrap the host function `func` so foreign code can call it (e.g. as a
/// `qsort` comparator); pass the returned pointer to foreign code instead of
/// `func`. Returns NULL on failure.
///
/// # Safety
/// `ctx` must be valid and `func` an `extern "C"` function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dldota_callback(ctx: *mut DldotaCtx, func: *mut c_void) -> *mut c_void {
    let Some(func) = NonNull::new(func) else {
        set_error(Error::Dl("NULL function".into()));
        return ptr::null_mut();
    };
    // SAFETY: caller contract.
    ok_or_null(unsafe { &*ctx }.callback_raw(func))
}

/// Release a thunk from `dldota_callback`. Foreign code must no longer hold it.
///
/// # Safety
/// `ctx` must be valid and `thunk` from `dldota_callback`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dldota_callback_free(ctx: *mut DldotaCtx, thunk: *mut c_void) {
    if let Some(thunk) = NonNull::new(thunk) {
        // SAFETY: caller contract.
        unsafe { &*ctx }.free_callback_raw(thunk);
    }
}

/// Last error on this thread, or NULL. Like `dlerror`, reading clears it; the
/// string stays valid until the next dldota call on this thread.
#[unsafe(no_mangle)]
pub extern "C" fn dldota_error() -> *const c_char {
    thread_local! {
        static RETURNED: RefCell<Option<CString>> = const { RefCell::new(None) };
    }
    let msg = LAST_ERROR.with(|slot| slot.borrow_mut().take());
    RETURNED.with(|ret| {
        let mut ret = ret.borrow_mut();
        *ret = msg;
        ret.as_deref().map_or(ptr::null(), CStr::as_ptr)
    })
}
