//! Helpers shared by the end-to-end tests.
#![allow(dead_code)]

use std::ffi::CStr;

use dldota::{Context, Library, OpenFlags};

pub fn context() -> Context {
    Context::new().expect("boot the system loader")
}

pub fn open<'ctx>(ctx: &'ctx Context, name: &CStr) -> Library<'ctx> {
    ctx.open(name, OpenFlags::NOW)
        .unwrap_or_else(|e| panic!("open {name:?}: {e}"))
}

/// Look up `name` as a function pointer of type `T`.
///
/// # Safety
/// `T` must match the symbol's C signature, and the result must not be used
/// after `lib` is closed.
pub unsafe fn sym<T: Copy>(lib: &Library<'_>, name: &CStr) -> T {
    // SAFETY: forwarded to the caller.
    *unsafe { lib.symbol::<T>(name) }.unwrap_or_else(|e| panic!("dlsym {name:?}: {e}"))
}

/// Number of threads in this process.
pub fn thread_count() -> usize {
    std::fs::read_dir("/proc/self/task").unwrap().count()
}

/// Deterministic, compressible test data: pseudo-random words.
pub fn corpus(len: usize) -> Vec<u8> {
    const WORDS: &[&[u8]] = &[
        b"static ",
        b"dynamic ",
        b"loader ",
        b"thread ",
        b"pointer ",
        b"libc ",
        b"symbol ",
        b"relocation ",
        b"musl ",
        b"glibc ",
        b"fullrust ",
        b"\n",
    ];
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut out = Vec::with_capacity(len + 16);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(WORDS[(state % WORDS.len() as u64) as usize]);
    }
    out.truncate(len);
    out
}
