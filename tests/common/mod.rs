//! Helpers shared by the end-to-end tests.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::CStr;

use dldota::{Context, Library, OpenFlags};

pub fn context() -> Context {
    Context::new().expect("boot the system loader")
}

pub fn open<'ctx>(ctx: &'ctx Context, name: &CStr) -> Library<'ctx> {
    ctx.open(name, OpenFlags::NOW)
        .unwrap_or_else(|e| panic!("open {name:?}: {e}"))
}

/// The foreign global scope, where the system libc's own symbols live.
pub fn global(ctx: &Context) -> Library<'_> {
    ctx.global().expect("dlopen(NULL)")
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

/// The thread count once it reaches `expected`, or after 2 s. A joined
/// thread can still be listed in /proc/self/task for a moment: `pthread_join`
/// returns when the kernel clears its tid, before the task is fully reaped.
pub fn settled_thread_count(expected: usize) -> usize {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let n = thread_count();
        if n == expected || std::time::Instant::now() >= deadline {
            return n;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
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

/// What a context could leave behind in the process.
#[derive(Debug)]
pub struct Footprint {
    /// File-backed mappings, counted per path.
    pub file_mappings: BTreeMap<String, usize>,
    /// Total size of anonymous mappings (heap, stacks, ...).
    pub anon_bytes: usize,
    pub threads: usize,
    pub fds: usize,
}

pub fn footprint() -> Footprint {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let mut file_mappings = BTreeMap::new();
    let mut anon_bytes = 0;
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let range = fields.next().unwrap();
        let path = fields.nth(4).unwrap_or("");
        let (start, end) = range.split_once('-').unwrap();
        let size =
            usize::from_str_radix(end, 16).unwrap() - usize::from_str_radix(start, 16).unwrap();
        if path.starts_with('/') {
            *file_mappings.entry(path.to_owned()).or_insert(0) += 1;
        } else if !matches!(path, "[vvar]" | "[vvar_vclock]" | "[vdso]" | "[vsyscall]") {
            anon_bytes += size;
        }
    }
    Footprint {
        file_mappings,
        anon_bytes,
        threads: thread_count(),
        fds: std::fs::read_dir("/proc/self/fd").unwrap().count(),
    }
}
