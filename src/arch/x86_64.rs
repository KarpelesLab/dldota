/// Current thread pointer (FS base). Both glibc and musl keep a self-pointer
/// at `%fs:0`, so this avoids an `arch_prctl` syscall.
#[allow(dead_code)]
#[inline(always)]
pub(crate) fn thread_pointer() -> usize {
    let tp: usize;
    // SAFETY: reads the TCB self-pointer, which every Linux x86_64 libc sets up.
    unsafe {
        core::arch::asm!("mov {}, fs:0", out(reg) tp, options(nostack, readonly, preserves_flags))
    };
    tp
}
