//! Raw Linux system calls. The loader can't rely on the host's libc: under
//! fullrust there is none, and a static libc may not wrap what we need.

use std::ffi::CStr;
use std::io;

#[cfg(target_arch = "x86_64")]
mod nr {
    pub const WRITE: usize = 1;
    pub const CLOSE: usize = 3;
    pub const MMAP: usize = 9;
    pub const MPROTECT: usize = 10;
    pub const MUNMAP: usize = 11;
    pub const RT_SIGPROCMASK: usize = 14;
    pub const FTRUNCATE: usize = 77;
    pub const PRCTL: usize = 157;
    pub const SET_TID_ADDRESS: usize = 218;
    pub const SET_ROBUST_LIST: usize = 273;
    pub const GET_ROBUST_LIST: usize = 274;
    pub const GETRANDOM: usize = 318;
    pub const MEMFD_CREATE: usize = 319;
    pub const FUTEX: usize = 202;
}

#[cfg(target_arch = "aarch64")]
mod nr {
    pub const WRITE: usize = 64;
    pub const CLOSE: usize = 57;
    pub const MMAP: usize = 222;
    pub const MPROTECT: usize = 226;
    pub const MUNMAP: usize = 215;
    pub const RT_SIGPROCMASK: usize = 135;
    pub const FTRUNCATE: usize = 46;
    pub const PRCTL: usize = 167;
    pub const SET_TID_ADDRESS: usize = 96;
    pub const SET_ROBUST_LIST: usize = 99;
    pub const GET_ROBUST_LIST: usize = 100;
    pub const GETRANDOM: usize = 278;
    pub const MEMFD_CREATE: usize = 279;
    pub const FUTEX: usize = 98;
}

pub(crate) const PROT_NONE: usize = 0;
pub(crate) const PROT_READ: usize = 1;
pub(crate) const PROT_WRITE: usize = 2;
pub(crate) const PROT_EXEC: usize = 4;
pub(crate) const MAP_SHARED: usize = 0x01;
pub(crate) const MAP_PRIVATE: usize = 0x02;
pub(crate) const MAP_FIXED: usize = 0x10;
pub(crate) const MAP_ANONYMOUS: usize = 0x20;
pub(crate) const SIG_SETMASK: usize = 2;
const MFD_CLOEXEC: usize = 1;
const PR_GET_TID_ADDRESS: usize = 40;

#[cfg(target_arch = "x86_64")]
unsafe fn raw(n: usize, a: [usize; 6]) -> isize {
    let ret: isize;
    // SAFETY: the caller upholds the contract of syscall `n`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a[0], in("rsi") a[1], in("rdx") a[2],
            in("r10") a[3], in("r8") a[4], in("r9") a[5],
            lateout("rcx") _, lateout("r11") _,
            options(nostack),
        )
    };
    ret
}

#[cfg(target_arch = "aarch64")]
unsafe fn raw(n: usize, a: [usize; 6]) -> isize {
    let ret: isize;
    // SAFETY: the caller upholds the contract of syscall `n`.
    unsafe {
        core::arch::asm!(
            "svc 0",
            in("x8") n,
            inlateout("x0") a[0] as isize => ret,
            in("x1") a[1], in("x2") a[2], in("x3") a[3], in("x4") a[4], in("x5") a[5],
            options(nostack),
        )
    };
    ret
}

unsafe fn syscall(n: usize, args: [usize; 6]) -> io::Result<usize> {
    // SAFETY: forwarded to the caller.
    let ret = unsafe { raw(n, args) };
    if (-4095..0).contains(&ret) {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret as usize)
    }
}

/// # Safety
/// With `MAP_FIXED`, `addr..addr+len` must not hold anything still in use.
pub(crate) unsafe fn mmap(
    addr: usize,
    len: usize,
    prot: usize,
    flags: usize,
    fd: i32,
    off: usize,
) -> io::Result<usize> {
    // SAFETY: forwarded to the caller.
    unsafe {
        syscall(
            nr::MMAP,
            [addr, len, prot, flags, fd as isize as usize, off],
        )
    }
}

/// # Safety
/// Nothing may use the range afterwards.
pub(crate) unsafe fn munmap(addr: usize, len: usize) -> io::Result<()> {
    // SAFETY: forwarded to the caller.
    unsafe { syscall(nr::MUNMAP, [addr, len, 0, 0, 0, 0]) }.map(drop)
}

/// # Safety
/// Nothing may rely on the old protection of the range.
pub(crate) unsafe fn mprotect(addr: usize, len: usize, prot: usize) -> io::Result<()> {
    // SAFETY: forwarded to the caller.
    unsafe { syscall(nr::MPROTECT, [addr, len, prot, 0, 0, 0]) }.map(drop)
}

pub(crate) fn memfd_create(name: &CStr) -> io::Result<i32> {
    // SAFETY: `name` is a valid C string.
    unsafe {
        syscall(
            nr::MEMFD_CREATE,
            [name.as_ptr() as usize, MFD_CLOEXEC, 0, 0, 0, 0],
        )
    }
    .map(|fd| fd as i32)
}

pub(crate) fn ftruncate(fd: i32, len: usize) -> io::Result<()> {
    // SAFETY: plain fd operation.
    unsafe { syscall(nr::FTRUNCATE, [fd as usize, len, 0, 0, 0, 0]) }.map(drop)
}

pub(crate) fn write(fd: i32, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: reads within `buf`.
    unsafe {
        syscall(
            nr::WRITE,
            [fd as usize, buf.as_ptr() as usize, buf.len(), 0, 0, 0],
        )
    }
}

pub(crate) fn close(fd: i32) -> io::Result<()> {
    // SAFETY: the caller owns `fd`.
    unsafe { syscall(nr::CLOSE, [fd as usize, 0, 0, 0, 0, 0]) }.map(drop)
}

/// Set the signal mask, returning the previous one.
pub(crate) fn sigprocmask_set(mask: u64) -> io::Result<u64> {
    let mut old = 0u64;
    // SAFETY: both pointers are valid 8-byte sigsets.
    unsafe {
        syscall(
            nr::RT_SIGPROCMASK,
            [
                SIG_SETMASK,
                &mask as *const u64 as usize,
                &mut old as *mut u64 as usize,
                8,
                0,
                0,
            ],
        )
    }?;
    Ok(old)
}

/// The calling thread's `clear_child_tid` address, if the kernel exposes it.
pub(crate) fn get_tid_address() -> Option<usize> {
    let mut addr = 0usize;
    // SAFETY: the kernel writes one pointer to `addr`.
    unsafe {
        syscall(
            nr::PRCTL,
            [
                PR_GET_TID_ADDRESS,
                &mut addr as *mut usize as usize,
                0,
                0,
                0,
                0,
            ],
        )
    }
    .ok()
    .map(|_| addr)
}

/// # Safety
/// `addr` must be what the thread's runtime expects the kernel to clear.
pub(crate) unsafe fn set_tid_address(addr: usize) {
    // SAFETY: forwarded to the caller; it cannot fail.
    let _ = unsafe { syscall(nr::SET_TID_ADDRESS, [addr, 0, 0, 0, 0, 0]) };
}

/// The calling thread's robust futex list: `(head, len)`.
pub(crate) fn get_robust_list() -> Option<(usize, usize)> {
    let (mut head, mut len) = (0usize, 0usize);
    // SAFETY: the kernel writes one pointer and one length.
    unsafe {
        syscall(
            nr::GET_ROBUST_LIST,
            [
                0,
                &mut head as *mut usize as usize,
                &mut len as *mut usize as usize,
                0,
                0,
                0,
            ],
        )
    }
    .ok()
    .map(|_| (head, len))
}

/// # Safety
/// `head` must be the list the thread's runtime registered.
pub(crate) unsafe fn set_robust_list(head: usize, len: usize) {
    // SAFETY: forwarded to the caller.
    let _ = unsafe { syscall(nr::SET_ROBUST_LIST, [head, len, 0, 0, 0, 0]) };
}

pub(crate) fn getrandom(buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        // SAFETY: writes within `buf`.
        filled += unsafe {
            syscall(
                nr::GETRANDOM,
                [
                    buf[filled..].as_mut_ptr() as usize,
                    buf.len() - filled,
                    0,
                    0,
                    0,
                    0,
                ],
            )
        }?;
    }
    Ok(())
}

const FUTEX_WAIT_PRIVATE: usize = 128;
const FUTEX_WAKE_PRIVATE: usize = 129;

/// Sleep while `*word == expected` (spurious wakeups possible).
pub(crate) fn futex_wait(word: &core::sync::atomic::AtomicU32, expected: u32) {
    // SAFETY: `word` is a live, aligned u32.
    let _ = unsafe {
        syscall(
            nr::FUTEX,
            [
                word.as_ptr() as usize,
                FUTEX_WAIT_PRIVATE,
                expected as usize,
                0,
                0,
                0,
            ],
        )
    };
}

pub(crate) fn futex_wake(word: &core::sync::atomic::AtomicU32, n: u32) {
    // SAFETY: `word` is a live, aligned u32.
    let _ = unsafe {
        syscall(
            nr::FUTEX,
            [
                word.as_ptr() as usize,
                FUTEX_WAKE_PRIVATE,
                n as usize,
                0,
                0,
                0,
            ],
        )
    };
}
