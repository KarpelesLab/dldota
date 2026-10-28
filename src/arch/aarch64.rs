//! aarch64: thread pointer access. Booting and trampolines are not written
//! yet; `BOOT_SUPPORTED` makes context creation fail cleanly.

pub(crate) const BOOT_SUPPORTED: bool = false;
pub(crate) const R_GLOB_DAT: u32 = 1025; // R_AARCH64_GLOB_DAT
pub(crate) const THUNK_SIZE: usize = 32;

pub(crate) fn init(_hwcap2: usize) {}

/// Current thread pointer (`tpidr_el0`).
#[inline(always)]
pub(crate) fn thread_pointer() -> usize {
    let tp: usize;
    // SAFETY: tpidr_el0 is always readable from EL0.
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) tp, options(nomem, nostack, preserves_flags))
    };
    tp
}

/// # Safety
/// Never called: `BOOT_SUPPORTED` is false.
pub(crate) unsafe fn boot(_state: *mut crate::loader::BootState, _entry: usize, _sp: usize) {
    unreachable!("aarch64 boot not implemented")
}

pub(crate) fn entry_address() -> usize {
    0
}

pub(crate) fn tramp_out_address() -> usize {
    0
}

pub(crate) fn tramp_in_address() -> usize {
    0
}

/// # Safety
/// Never called: `BOOT_SUPPORTED` is false.
pub(crate) unsafe fn write_thunk(_dst: *mut u8, _target: usize, _data: usize, _tramp: usize) {
    unreachable!("aarch64 thunks not implemented")
}
