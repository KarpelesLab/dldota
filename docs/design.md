# dldota design notes

## Goal

A statically linked program (Rust on `*-linux-musl`, static glibc, static-pie, or a
C program linked against `libdldota.a`) creates a **context** and uses it to
`dlopen`/`dlsym` the *host system's* shared libraries (libGL, libvulkan, libSDL,
libpulse, ...), the same way a dynamically linked program would.

## The two options

### A. Load everything ourselves (a custom dynamic linker)

Map the requested library and its `DT_NEEDED` closure, apply relocations, set up
TLS, run initializers. Prior art: `dlopen-rs` / `elf_loader` (Rust).

It breaks down on the libraries people actually want to load. Nearly every
system library depends on `libc.so.6`, and glibc's `libc.so.6` is tightly coupled
to *its own* `ld.so`:

- `_rtld_global` / `_rtld_global_ro` (GLIBC_PRIVATE), which libc reads directly
- `__tls_get_addr` and the DTV layout, static TLS sizing, `__libc_early_init`
- IFUNC resolvers that read CPU features computed by `ld.so`
- `dl_iterate_phdr`, `dladdr`, `dlopen` from *inside* libc (NSS, iconv/gconv,
  libgcc_s unwinding, and Mesa/Vulkan loaders that `dlopen` their own drivers)

Emulating this means tracking glibc internals across versions, and then doing the
same again for musl-based systems. It isn't viable for "load whatever is on the
system."

### B. Bootstrap the system's own `ld.so` and use its `dlopen` (recommended)

Act as the kernel's ELF loader for the system interpreter. Map
`/lib64/ld-linux-x86-64.so.2` (or `ld-musl-*.so.1`), build an initial process
stack (argc/argv/envp/auxv), and jump to its entry point. The real `ld.so` loads
`libc.so.6`, initializes it, and finally jumps to "the program entry," which is
our code. We capture `dlopen`/`dlsym`/`dlclose`/`dlerror`/`dladdr`, then return to
the caller. From then on, the system's own loader resolves every library. This
works for glibc and musl systems alike, and the result is correct by
construction.

This is what `foreign-dlopen` (pfalcon), `detour` (graphitemaster) and
Cosmopolitan's `cosmo_dlopen` do. **We go with B.** The rest of this document
covers what those projects leave unsolved.

## Bootstrap details

### No helper executable on disk

The prior art needs a helper ELF that is dynamically linked against the system
libc. Detour and foreign-dlopen ship one; cosmo compiles one at runtime with
`cc`. We can avoid that: `ld.so` only learns about the main program through
`AT_PHDR`/`AT_PHNUM`/`AT_ENTRY`, because the kernel has normally mapped it
already. So we **synthesize the main program in memory**:

- `PT_PHDR` (so `ld.so` computes `l_addr`), one `PT_LOAD` covering the block,
  `PT_DYNAMIC`, `PT_GNU_STACK`
- `.dynamic`: `DT_NEEDED libc.so.6` (plus `libdl.so.2` for glibc < 2.34),
  `DT_STRTAB`/`DT_SYMTAB`/`DT_RELA`, and `DT_DEBUG` so gdb sees the link map
- `R_*_GLOB_DAT` relocations for `dlopen`, `dlsym`, `dlclose`, `dlerror`,
  `dladdr`, `dlinfo`, `pthread_create`, ... into a GOT we own
- **definitions** (`.dynsym` entries pointing at our code) for the symbols we
  interpose. The main program comes first in every lookup scope, so these win
  over libc's for every library, as `LD_PRELOAD` would:
  - the `malloc` family: see [Per-context allocator](#per-context-allocator)
  - `pthread_create`: track foreign-created threads (their kernel tids) for
    [Unloading](#unloading). Their stacks stay glibc's/musl's; glibc runs with
    `glibc.pthread.stack_cache_size=0` so joined threads' stacks are unmapped
    instead of cached where unload couldn't find them. (Giving threads
    context-owned stacks instead would need `pthread_join`/`pthread_detach`
    interposed too, to know when a stack can be freed.)
  - `__cxa_atexit`, `__cxa_finalize`, `atexit`: own the exit-handler list, so
    library destructors can run at unload on glibc *and* musl
- `AT_ENTRY` points at our asm entry thunk

The block is RW because `ld.so` writes `DT_DEBUG` and the GOT. This needs a spike
to confirm glibc's `dl_main` accepts it, including with no `PT_INTERP`. If it
doesn't, the fallback is a hand-written ELF image embedded as bytes and passed
through `memfd` + `AT_EXECFD`.

### Finding the interpreter

A static binary has no `PT_INTERP` of its own. Candidates, in order:

1. explicit path from the caller
2. `PT_INTERP` of a known system binary (`/bin/sh`, `/usr/bin/env`)
3. per-arch well-known paths (`/lib64/ld-linux-x86-64.so.2`,
   `/lib/ld-musl-x86_64.so.1`, `/lib/ld-linux-aarch64.so.1`, ...)

### Initial stack and auxv

Allocated with `mmap` and **leaked**, because the foreign libc keeps pointers into
it (`environ`, `__libc_stack_end`, `argv`). Auxv is copied from the host
(`getauxval` / `/proc/self/auxv`): `AT_SYSINFO_EHDR`, `AT_HWCAP{,2,3,4}`,
`AT_PLATFORM`, `AT_PAGESZ`, `AT_CLKTCK`, `AT_MINSIGSTKSZ`, `AT_SECURE`,
`AT_RANDOM` (fresh 16 bytes). The fake-program fields are added: `AT_PHDR`,
`AT_PHNUM`, `AT_PHENT`, `AT_ENTRY`, and `AT_BASE` = the ld.so load address. The
envp passed in is the context's environment, so `LD_LIBRARY_PATH`,
`LD_PRELOAD` and `GLIBC_TUNABLES` all work, per context. We append
`glibc.pthread.rseq=0` (and `glibc.pthread.stack_cache_size=0`, see below) to
`GLIBC_TUNABLES`: glibc's rseq area lives in its TCB,
and the kernel keeps writing to it, which would fault once the TCB is unmapped
at unload. This also matters for borrowed TCBs.

### Return path

`bootstrap()` saves callee-saved registers, the stack pointer and the host
thread pointer, switches to the fake stack, and jumps to `ld.so`. Our entry thunk
records the foreign thread pointer, restores the host one, and jumps back. This
is a setjmp/longjmp pair written in asm, not libc's.

## The hard part: two runtimes, one process

Prior art mostly stops at "printf works." These are the issues we must handle.

### 1. Thread pointer (TLS)

`ld.so` installs its own TCB (`arch_prctl(ARCH_SET_FS)` on x86_64, `tpidr_el0`
on aarch64). Every glibc function assumes TP points to a glibc TCB; every
host function assumes the host TCB. So **every call crossing the boundary must
switch TP**, in both directions:

- **Outbound** (host → foreign): each `dlsym` result is wrapped in a
  per-symbol stub (`mov r11, target; jmp tramp` / `ldr x16, ...; b tramp`) placed
  in an RX page owned by the context. The shared trampoline must not touch
  argument registers or stack arguments, so variadic functions and many arguments
  work. It keeps the return address and host TP in a per-thread side area
  (host TLS reached at a fixed TP offset, since static executables use the
  local-exec TLS model), swaps TP, calls, and swaps back.
- **Inbound** callbacks (foreign → host, e.g. `qsort` comparators, Vulkan
  debug callbacks): the mirror-image thunk, created by `Context::callback()`
  (`dldota_callback()` in C).
- **Cost:** `arch_prctl` is a syscall. With FSGSBASE (`AT_HWCAP2` bit 1, Linux
  5.9+), `wrfsbase` is a single instruction. On aarch64, `msr tpidr_el0` is
  cheap.

### 2. Threads

There is one foreign TCB after bootstrap, belonging to the bootstrap thread.
Using it concurrently from several host threads corrupts glibc state (errno,
tcache, `dlerror`, locale). Options:

- **(MVP)** a per-context lock: one host thread inside foreign code at a time,
  all of them borrowing the bootstrap TCB. The lock must be re-entrant,
  because a callback can call back into foreign code (host → foreign → host →
  foreign, see `tests/callbacks.rs`).
- **(Later)** a foreign TCB per host thread. Lazily `pthread_create` a parked
  foreign thread through the *foreign* libc and borrow its TCB. Caveat: the TCB's
  `tid` is the parked thread's, which matters for PI/robust mutexes only.

Threads created *by* foreign libraries run foreign code with a foreign TCB, which
is fine. If they call back into host code, though, there is no host TCB. Rust
callbacks on foreign-created threads must either avoid host TLS entirely (which
rules out most of std) or be rejected/documented. This is an open question.

### 3. Process-global kernel state touched by libc init

| State | Who touches it | Mitigation |
|---|---|---|
| FS base / `tpidr_el0` | ld.so | save/restore around bootstrap and every call (above) |
| `set_tid_address` | glibc init (for the bootstrap thread) | `prctl(PR_GET_TID_ADDRESS)` before, restore after. Otherwise musl's `pthread_join` on that thread can hang |
| `set_robust_list` | glibc init | save (`get_robust_list`) / restore |
| rseq | glibc ≥ 2.35 registers it | disabled with `GLIBC_TUNABLES=glibc.pthread.rseq=0` (the TCB must be unmappable at unload) |
| `brk` | a second malloc would cache the break | avoided: the foreign libc's malloc is replaced by the per-context allocator (mmap only) |
| signals 32–34 | musl: SIGTIMER=32, SIGCANCEL=33, SIGSYNCCALL=34. glibc: SIGCANCEL=32, SIGSETXID=33 | check whether glibc installs these eagerly (≥2.34 seems lazy). Snapshot host handlers before bootstrap and compare after |

### 4. Signals during foreign code

A host signal handler (e.g. Rust std's SIGSEGV stack-overflow handler, which
reads TLS) can fire while TP is foreign. Plan: optionally wrap host handlers with
a shim that restores host TP when the interrupted TP is a known foreign TCB.
The cheap fallback is blocking signals around calls, at the cost of a syscall per
call.

### 5. Other

- Two allocators: memory from foreign code must be freed by foreign code (the
  library's own free function, or libc `free` through the context), never by
  the host allocator. This is normal FFI discipline but deserves a note in the
  docs.
- Unwinding must never cross the boundary: Rust panics abort at `extern "C"`,
  and C++ exceptions must be caught on the foreign side.
- Unloading is explicit (`Context::unload`, see [Unloading](#unloading)).
  Dropping a `Context` without it leaks the foreign world, because an implicit
  unload could crash under a still-running foreign thread.
- gdb: `DT_DEBUG` → `r_debug` lets gdb find foreign libraries. Worth keeping
  working.
- If the host is in fact dynamically linked, we could offer a passthrough to the
  real `dlopen`, but that is out of scope for now.

## Per-context allocator

The fake main program defines the whole `malloc` family: `malloc`, `free`,
`calloc`, `realloc`, `reallocarray`, `memalign`, `posix_memalign`,
`aligned_alloc`, `valloc`, `pvalloc`, `malloc_usable_size`, `free_sized` and
`free_aligned_sized`. glibc officially supports replacing malloc this way,
libc-internal allocations included (`fopen`, `strdup`, and `ld.so` itself once
relocated), and so does musl ≥ 1.2.1. Every foreign allocation then lands in
memory the context owns:

- **Unloading reclaims everything**: the heap is a set of context-owned
  regions, unmapped wholesale. Without this, the foreign libc's heap leaks.
- **Accounting**: `Context::heap_usage()` reports live allocations and bytes,
  which helps find leaks in foreign libraries.
- **No `brk` sharing**: the allocator is mmap-only, so the host and foreign
  heaps can't shrink each other.

Constraints on the allocator:

- It runs with the *foreign* TP and on foreign-created threads, so it must not
  touch host TLS or the host allocator. That's no_std-style Rust working on
  regions it mmaps itself; no TP switch is needed to enter it.
- It must be thread-safe and reasonably fast under contention: size-class
  slabs, with large allocations getting their own mapping. Per-thread caches,
  keyed by foreign TP, can come later.
- Fork safety: a host `fork()` while another thread holds an allocator lock
  must not deadlock the child, which needs an atfork hook or a reset in the
  child.
- `malloc_usable_size` and the alignment variants must be exact; some
  libraries rely on them.

## Unloading

`Context::unload(self) -> Result<(), (Context, Error)>` tears the whole foreign
world down. On failure the caller gets the context back intact.

1. Take the context lock; consuming `self` guarantees no `Library`, `Symbol`
   or `Callback` borrow survives on the Rust side (C callers must ensure the
   same).
2. Refuse with `Error::Busy` if a thread created through the interposed
   `pthread_create` is still alive (checked against `/proc/self/task`): its code
   would be unmapped under it, and a single thread can't be killed safely.
3. `fflush(NULL)` in the foreign libc.
4. Run the exit-handler list we own (`__cxa_atexit`/`atexit`), newest first.
   Then run library finalizers: on glibc, the `_dl_fini` that `ld.so` passes to
   our entry point; on musl (where `dlclose` and `__cxa_finalize` are no-ops),
   each object's `DT_FINI_ARRAY`/`DT_FINI` in reverse load order, found through
   the `DT_DEBUG` link map.
5. Restore signal handlers that point into foreign mappings to the snapshot
   taken at bootstrap.
6. Unmap: every object's `PT_LOAD` ranges (collected with the foreign
   `dl_iterate_phdr` before step 4), `ld.so`, the stub, the boot stack, foreign
   TCBs, call/callback thunk pages, and the allocator's regions.

What it can't reclaim: file descriptors and raw `mmap`s that a library
forgets to release in its destructors, since they can't be told apart from
the host's. Function pointers copied out of a `Symbol` dangle afterwards,
which is part of the `unsafe` contract.

Acceptance test (`tests/unload_cycles.rs`): boot, use and unload 100 times, then
check that file-backed mappings, thread count and fd count are back to the
baseline and anonymous memory hasn't grown.

## Crate layout

```
src/lib.rs          public Rust API re-exports
src/context.rs      Context, ContextBuilder, Library, Symbol, Callback
src/heap.rs         per-context allocator interposed as the foreign malloc
src/error.rs        Error type
src/ffi.rs          C ABI (libdldota.a), mirrored by include/dldota.h
src/loader/elf.rs   ELF parsing + mapping of the interpreter
src/loader/stub.rs  synthesized in-memory main program
src/loader/stack.rs initial stack / auxv construction
src/arch/*.rs       thread pointer access, bootstrap jump, call trampolines
```

The core loader should depend on nothing but raw syscalls, so its behaviour
doesn't depend on which libc the host uses.

## Targets

- First: x86_64 and aarch64 Linux, with glibc or musl systems as the *foreign*
  side
- Later: riscv64

## First spikes

1. In-memory stub accepted by glibc `dl_main` (x86_64); `dlopen("libz.so.1")`
   and call `zlibVersion` from a `x86_64-unknown-linux-musl` static binary
2. The same against a musl system interpreter (Alpine container)
3. Verify the signal 32–34 behaviour
4. Trampolines plus a multi-threaded call stress test under the MVP lock
5. Per-context allocator, interposed on both glibc and musl
6. Unload, then the boot/use/unload loop with no leaks
