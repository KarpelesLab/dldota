//! x86_64: thread pointer access, the boot jump into `ld.so`, and the call
//! trampolines that switch the thread pointer (FS base) at the boundary.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::gate;
use crate::loader::BOOT_STATE;

pub(crate) const BOOT_SUPPORTED: bool = true;
pub(crate) const R_GLOB_DAT: u32 = 6; // R_X86_64_GLOB_DAT
pub(crate) const THUNK_SIZE: usize = 48;
const HWCAP2_FSGSBASE: usize = 1 << 1;

/// `wrfsbase` is usable (kernel ≥ 5.9 on a capable CPU): switching the thread
/// pointer is one instruction instead of an `arch_prctl` syscall.
static HAS_FSGSBASE: AtomicBool = AtomicBool::new(false);

pub(crate) fn init(hwcap2: usize) {
    HAS_FSGSBASE.store(hwcap2 & HWCAP2_FSGSBASE != 0, Ordering::Relaxed);
}

/// Current thread pointer (FS base). Both glibc and musl keep a self-pointer
/// at `%fs:0`, so this avoids an `arch_prctl` syscall.
#[inline(always)]
pub(crate) fn thread_pointer() -> usize {
    let tp: usize;
    // SAFETY: reads the TCB self-pointer, which every Linux x86_64 libc sets up.
    unsafe {
        core::arch::asm!("mov {}, fs:0", out(reg) tp, options(nostack, readonly, preserves_flags))
    };
    tp
}

unsafe extern "C" {
    fn __dldota_x86_64_boot(state: *mut crate::loader::BootState, entry: usize, sp: usize);
    fn __dldota_x86_64_entry();
    fn __dldota_x86_64_tramp_out();
    fn __dldota_x86_64_tramp_in();
}

/// Jump into `ld.so` at `entry` with the initial stack `sp`; returns once the
/// foreign libc reached our `main`, with the host thread pointer restored.
///
/// # Safety
/// `entry` and `sp` must be a mapped loader and a stack built for it, and
/// `state` must be published in `BOOT_STATE` for the entry code.
pub(crate) unsafe fn boot(state: *mut crate::loader::BootState, entry: usize, sp: usize) {
    // SAFETY: forwarded to the caller.
    unsafe { __dldota_x86_64_boot(state, entry, sp) }
}

pub(crate) fn entry_address() -> usize {
    __dldota_x86_64_entry as *const () as usize
}

pub(crate) fn tramp_out_address() -> usize {
    __dldota_x86_64_tramp_out as *const () as usize
}

pub(crate) fn tramp_in_address() -> usize {
    __dldota_x86_64_tramp_in as *const () as usize
}

/// Write a thunk that loads `target` into r11 and `data` into r10, then jumps
/// to `tramp`. r10/r11 are scratch registers no C argument travels in.
///
/// # Safety
/// `dst` must be writable for `THUNK_SIZE` bytes.
pub(crate) unsafe fn write_thunk(dst: *mut u8, target: usize, data: usize, tramp: usize) {
    let mut code = [0u8; THUNK_SIZE];
    code[0..2].copy_from_slice(&[0x49, 0xbb]); // movabs r11, target
    code[2..10].copy_from_slice(&target.to_le_bytes());
    code[10..12].copy_from_slice(&[0x49, 0xba]); // movabs r10, data
    code[12..20].copy_from_slice(&data.to_le_bytes());
    code[20..26].copy_from_slice(&[0xff, 0x25, 0, 0, 0, 0]); // jmp [rip+0]
    code[26..34].copy_from_slice(&tramp.to_le_bytes());
    code[34..].fill(0xcc); // int3 padding
    // SAFETY: forwarded to the caller.
    unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), dst, THUNK_SIZE) };
}

/// Write a thunk that calls `func(data, a0, a1, a2, a3)` for a C call `(a0,
/// a1, a2, a3)`: shifts up to four integer arguments and prepends `data`.
///
/// # Safety
/// `dst` must be writable for `THUNK_SIZE` bytes.
pub(crate) unsafe fn write_bind_thunk(dst: *mut u8, func: usize, data: usize) {
    let mut code = [0xccu8; THUNK_SIZE];
    code[0..12].copy_from_slice(&[
        0x49, 0x89, 0xc8, // mov r8, rcx
        0x48, 0x89, 0xd1, // mov rcx, rdx
        0x48, 0x89, 0xf2, // mov rdx, rsi
        0x48, 0x89, 0xfe, // mov rsi, rdi
    ]);
    code[12..14].copy_from_slice(&[0x48, 0xbf]); // movabs rdi, data
    code[14..22].copy_from_slice(&data.to_le_bytes());
    code[22..24].copy_from_slice(&[0x48, 0xb8]); // movabs rax, func
    code[24..32].copy_from_slice(&func.to_le_bytes());
    code[32..34].copy_from_slice(&[0xff, 0xe0]); // jmp rax
    // SAFETY: forwarded to the caller.
    unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), dst, THUNK_SIZE) };
}

/// Crash immediately, without touching std or any libc.
pub(crate) fn trap() -> ! {
    // SAFETY: raises SIGILL.
    unsafe { core::arch::asm!("ud2", options(noreturn, nomem, nostack)) }
}

// Set FS base to rsi. Clobbers rdi, rax, rcx, r11 (syscall path).
macro_rules! set_fs_rsi {
    () => {
        concat!(
            "cmp byte ptr [rip + {fsgs}], 0\n",
            "je 2f\n",
            ".byte 0xf3, 0x48, 0x0f, 0xae, 0xd6\n", // wrfsbase rsi
            "jmp 3f\n",
            "2:\n",
            "mov edi, 0x1002\n", // ARCH_SET_FS
            "mov eax, 158\n",    // SYS_arch_prctl
            "syscall\n",
            "3:\n",
        )
    };
}

// Save/restore the argument registers, rax (vararg count), r10/r11 and
// xmm0-7 in a 200-byte area at rsp.
macro_rules! save_args {
    () => {
        concat!(
            "mov [rsp], rdi\n",
            "mov [rsp + 8], rsi\n",
            "mov [rsp + 16], rdx\n",
            "mov [rsp + 24], rcx\n",
            "mov [rsp + 32], r8\n",
            "mov [rsp + 40], r9\n",
            "mov [rsp + 48], rax\n",
            "mov [rsp + 56], r10\n",
            "mov [rsp + 64], r11\n",
            "movdqu [rsp + 72], xmm0\n",
            "movdqu [rsp + 88], xmm1\n",
            "movdqu [rsp + 104], xmm2\n",
            "movdqu [rsp + 120], xmm3\n",
            "movdqu [rsp + 136], xmm4\n",
            "movdqu [rsp + 152], xmm5\n",
            "movdqu [rsp + 168], xmm6\n",
            "movdqu [rsp + 184], xmm7\n",
        )
    };
}
macro_rules! restore_args {
    () => {
        concat!(
            "mov rdi, [rsp]\n",
            "mov rsi, [rsp + 8]\n",
            "mov rdx, [rsp + 16]\n",
            "mov rcx, [rsp + 24]\n",
            "mov r8, [rsp + 32]\n",
            "mov r9, [rsp + 40]\n",
            "mov rax, [rsp + 48]\n",
            "mov r11, [rsp + 64]\n",
            "movdqu xmm0, [rsp + 72]\n",
            "movdqu xmm1, [rsp + 88]\n",
            "movdqu xmm2, [rsp + 104]\n",
            "movdqu xmm3, [rsp + 120]\n",
            "movdqu xmm4, [rsp + 136]\n",
            "movdqu xmm5, [rsp + 152]\n",
            "movdqu xmm6, [rsp + 168]\n",
            "movdqu xmm7, [rsp + 184]\n",
        )
    };
}

core::arch::global_asm!(
    ".pushsection .text.dldota_x86_64,\"ax\",@progbits",

    // void __dldota_x86_64_boot(BootState *state, entry, sp)
    // Saves the host's callee-saved registers and stack in `state`, then
    // enters ld.so exactly as the kernel would.
    ".p2align 4",
    ".globl __dldota_x86_64_boot",
    ".hidden __dldota_x86_64_boot",
    "__dldota_x86_64_boot:",
    "push rbp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov [rdi], rsp",          // state.host_sp
    "mov [rip + {state}], rdi",
    "mov rsp, rdx",
    "xor edx, edx",            // no atexit function from the "kernel"
    "xor ebp, ebp",
    "jmp rsi",

    // The program entry ld.so jumps to (AT_ENTRY), shaped like crt1's _start:
    // rsp -> argc, rdx = ld.so's fini function. Calls the foreign
    // __libc_start_main, which initializes libc and calls our "main".
    ".p2align 4",
    ".globl __dldota_x86_64_entry",
    ".hidden __dldota_x86_64_entry",
    "__dldota_x86_64_entry:",
    "mov rax, [rip + {state}]",
    "mov [rax + 32], rdx",     // state.rtld_fini
    "xor ebp, ebp",
    "mov r9, rdx",             // rtld_fini
    "pop rsi",                 // argc
    "mov rdx, rsp",            // argv
    "and rsp, -16",
    "push rax",
    "push rsp",                // stack_end
    "xor r8d, r8d",            // fini
    "xor ecx, ecx",            // init
    "lea rdi, [rip + 4f]",     // main
    "mov rax, [rax + 16]",     // state.start_main_slot
    "call [rax]",
    "ud2",
    // "main": the foreign libc is up. Record its thread pointer, restore the
    // host's, and return from __dldota_x86_64_boot. This frame stays on the
    // boot stack, which the context keeps.
    "4:",
    "mov r8, [rip + {state}]",
    "mov rcx, qword ptr fs:[0]",
    "mov [r8 + 24], rcx",      // state.foreign_tp
    "mov rsi, [r8 + 8]",       // state.host_tp
    "mov edi, 0x1002",         // ARCH_SET_FS
    "mov eax, 158",
    "syscall",
    "mov rsp, [r8]",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",

    // Host -> foreign. Entered from a thunk: r11 = target, r10 = gate,
    // [rsp] = the host caller's return address, TP = host.
    // The return address is parked in a host-side frame stack and the call
    // re-issued from the same stack slot, so stack arguments stay in place.
    // rbx carries the host TP across the call (callee-saved); the caller's
    // rbx/r12 are parked too.
    ".p2align 4",
    ".globl __dldota_x86_64_tramp_out",
    ".hidden __dldota_x86_64_tramp_out",
    "__dldota_x86_64_tramp_out:",
    "sub rsp, 200",
    save_args!(),
    "mov rdi, r10",
    "mov rsi, [rsp + 200]",
    "mov rdx, rbx",
    "mov rcx, r12",
    "call {enter_foreign}",    // -> rax = foreign TP, rdx = host TP
    "mov rbx, rdx",
    "mov rsi, rax",
    set_fs_rsi!(),
    restore_args!(),
    "add rsp, 208",            // save area + the caller's return address
    "call r11",
    "sub rsp, 80",             // out[3], rax, rdx, xmm0, xmm1; return slot at +72
    "mov [rsp + 24], rax",
    "mov [rsp + 32], rdx",
    "movdqu [rsp + 40], xmm0",
    "movdqu [rsp + 56], xmm1",
    "mov rsi, rbx",
    set_fs_rsi!(),
    "mov rdi, rsp",
    "call {leave_foreign}",    // out = [ret, rbx, r12]
    "mov r11, [rsp]",
    "mov [rsp + 72], r11",
    "mov rbx, [rsp + 8]",
    "mov r12, [rsp + 16]",
    "mov rax, [rsp + 24]",
    "mov rdx, [rsp + 32]",
    "movdqu xmm0, [rsp + 40]",
    "movdqu xmm1, [rsp + 56]",
    "add rsp, 72",
    "ret",

    // Foreign -> host (callbacks). Entered from a thunk: r11 = host function,
    // r10 = gate, TP = foreign. Only threads borrowing the gate's TCB have a
    // host TCB to return to.
    ".p2align 4",
    ".globl __dldota_x86_64_tramp_in",
    ".hidden __dldota_x86_64_tramp_in",
    "__dldota_x86_64_tramp_in:",
    "sub rsp, 216",            // save area + foreign TP + padding
    save_args!(),
    "mov rax, qword ptr fs:[0]",
    "mov [rsp + 200], rax",
    "cmp rax, [r10]",          // gate.boot_tp
    "jne 5f",
    "mov rsi, [r10 + 8]",      // gate.cur_host_tp
    set_fs_rsi!(),
    "mov rdi, [rsp + 56]",     // gate (saved r10)
    "mov rsi, [rsp + 216]",    // return address
    "mov rdx, rbx",
    "mov rcx, r12",
    "mov r8, [rsp + 200]",     // foreign TP
    "call {enter_host}",
    restore_args!(),
    "add rsp, 224",
    "call r11",
    "sub rsp, 96",             // out[4], rax, rdx, xmm0, xmm1; return slot at +88
    "mov [rsp + 32], rax",
    "mov [rsp + 40], rdx",
    "movdqu [rsp + 48], xmm0",
    "movdqu [rsp + 64], xmm1",
    "mov rdi, rsp",
    "call {leave_host}",       // out = [ret, rbx, r12, foreign TP]
    "mov rsi, [rsp + 24]",
    set_fs_rsi!(),
    "mov r11, [rsp]",
    "mov [rsp + 88], r11",
    "mov rbx, [rsp + 8]",
    "mov r12, [rsp + 16]",
    "mov rax, [rsp + 32]",
    "mov rdx, [rsp + 40]",
    "movdqu xmm0, [rsp + 48]",
    "movdqu xmm1, [rsp + 64]",
    "add rsp, 88",
    "ret",
    "5:",
    "mov edi, 2",
    "lea rsi, [rip + 6f]",
    "lea rdx, [rip + 7f]",
    "sub rdx, rsi",
    "mov eax, 1",              // SYS_write
    "syscall",
    "ud2",
    "6:",
    ".ascii \"dldota: callback called on a thread created by foreign code (not supported yet)\\n\"",
    "7:",
    ".popsection",

    state = sym BOOT_STATE,
    fsgs = sym HAS_FSGSBASE,
    enter_foreign = sym gate::enter_foreign,
    leave_foreign = sym gate::leave_foreign,
    enter_host = sym gate::enter_host,
    leave_host = sym gate::leave_host,
);
