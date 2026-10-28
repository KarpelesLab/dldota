//! The initial process stack (argc, argv, envp, auxv) `ld.so` boots from.
//!
//! It stays mapped for the context's lifetime: the foreign libc keeps
//! pointers into it (`environ`, `__libc_stack_end`, `argv`), and the foreign
//! "main" frame that boot returns from lives on it.

use std::ffi::{CStr, CString};

use crate::error::Result;
use crate::sys;

pub(crate) const AT_NULL: usize = 0;
pub(crate) const AT_PHDR: usize = 3;
pub(crate) const AT_PHENT: usize = 4;
pub(crate) const AT_PHNUM: usize = 5;
pub(crate) const AT_PAGESZ: usize = 6;
pub(crate) const AT_BASE: usize = 7;
pub(crate) const AT_FLAGS: usize = 8;
pub(crate) const AT_ENTRY: usize = 9;
pub(crate) const AT_PLATFORM: usize = 15;
pub(crate) const AT_HWCAP2: usize = 26;
pub(crate) const AT_RANDOM: usize = 25;
pub(crate) const AT_EXECFN: usize = 31;

/// Host auxv entries passed through unchanged.
pub(crate) const PASSTHROUGH: &[usize] = &[
    AT_PAGESZ, 11, /* AT_UID */ 12, /* AT_EUID */ 13,
    /* AT_GID */ 14, /* AT_EGID */
    16, /* AT_HWCAP */ 17, /* AT_CLKTCK */ 23, /* AT_SECURE */ AT_HWCAP2,
    29, /* AT_HWCAP3 */
    30, /* AT_HWCAP4 */ 33, /* AT_SYSINFO_EHDR */ 51, /* AT_MINSIGSTKSZ */
];

#[cfg(target_arch = "x86_64")]
const PLATFORM: &CStr = c"x86_64";
#[cfg(target_arch = "aarch64")]
const PLATFORM: &CStr = c"aarch64";

const STACK_SIZE: usize = 1 << 20;

pub(crate) struct BootStack {
    /// The whole mapping, guard page included: `(start, len)`.
    pub region: (usize, usize),
    /// Initial stack pointer, pointing at `argc`.
    pub sp: usize,
}

/// The host's auxiliary vector, from `/proc/self/auxv`.
pub(crate) fn host_auxv() -> Vec<(usize, usize)> {
    let Ok(raw) = std::fs::read("/proc/self/auxv") else {
        return Vec::new();
    };
    const WORD: usize = size_of::<usize>();
    raw.as_chunks::<{ 2 * WORD }>()
        .0
        .iter()
        .map(|pair| {
            let (k, v) = pair.split_at(WORD);
            (
                usize::from_ne_bytes(k.try_into().unwrap()),
                usize::from_ne_bytes(v.try_into().unwrap()),
            )
        })
        .take_while(|&(k, _)| k != AT_NULL)
        .collect()
}

/// Build the stack. `aux` holds every entry except the string/random ones,
/// which are added here.
pub(crate) fn build(
    page: usize,
    argv0: &CStr,
    env: &[CString],
    aux: &[(usize, usize)],
) -> Result<BootStack> {
    let len = STACK_SIZE + page;
    // SAFETY: a fresh private mapping.
    let start = unsafe {
        sys::mmap(
            0,
            len,
            sys::PROT_READ | sys::PROT_WRITE,
            sys::MAP_PRIVATE | sys::MAP_ANONYMOUS,
            -1,
            0,
        )
    }?;
    // Guard page at the bottom.
    // SAFETY: our own fresh mapping.
    unsafe { sys::mprotect(start, page, sys::PROT_NONE) }?;

    let mut top = start + len;
    let mut push = |bytes: &[u8]| -> usize {
        top -= bytes.len();
        // SAFETY: stays far above the guard page for any sane environment.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), top as *mut u8, bytes.len()) };
        top
    };
    let mut random = [0u8; 16];
    sys::getrandom(&mut random)?;
    let random = push(&random);
    let platform = push(PLATFORM.to_bytes_with_nul());
    let execfn = push(argv0.to_bytes_with_nul());
    let env_ptrs: Vec<usize> = env.iter().map(|e| push(e.to_bytes_with_nul())).collect();
    let argv0 = push(argv0.to_bytes_with_nul());

    let mut words = vec![1, argv0, 0];
    words.extend(&env_ptrs);
    words.push(0);
    for &(k, v) in aux {
        words.extend([k, v]);
    }
    words.extend([
        AT_RANDOM,
        random,
        AT_PLATFORM,
        platform,
        AT_EXECFN,
        execfn,
        AT_NULL,
        0,
    ]);

    let sp = (top - words.len() * size_of::<usize>()) & !15;
    // SAFETY: `sp..sp+words` lies below the strings, inside the mapping.
    unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), sp as *mut usize, words.len()) };
    Ok(BootStack {
        region: (start, len),
        sp,
    })
}
