//! The synthesized in-memory "main program" handed to `ld.so` via `AT_PHDR`.
//!
//! It carries `PT_PHDR`/`PT_LOAD`/`PT_DYNAMIC`, a `.dynamic` with
//! `DT_NEEDED libc.so.6`, and `GLOB_DAT` relocations that make `ld.so` fill a
//! GOT with `dlopen`, `dlsym`, `dlclose`, `dlerror`, ... for us. `AT_ENTRY`
//! points at the arch entry thunk that returns control to `bootstrap()`.
//!
//! It also *defines* the symbols we interpose (first in every lookup scope):
//! the `malloc` family (backed by [`crate::heap`]), `pthread_create`, and
//! `__cxa_atexit`/`__cxa_finalize`/`atexit`.
