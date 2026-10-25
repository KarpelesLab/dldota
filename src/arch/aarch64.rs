/// Current thread pointer (`tpidr_el0`).
#[allow(dead_code)]
#[inline(always)]
pub(crate) fn thread_pointer() -> usize {
    let tp: usize;
    // SAFETY: tpidr_el0 is always readable from EL0.
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) tp, options(nomem, nostack, preserves_flags))
    };
    tp
}
