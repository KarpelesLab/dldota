//! Load the host system's shared libraries from a statically linked program.
//!
//! A statically linked program has no dynamic loader, so `dlopen` is either a
//! stub (musl) or only works against the exact glibc it was built with. dldota
//! maps the system's own dynamic loader (`ld-linux-*.so`, `ld-musl-*.so`) into
//! the process, boots it as the kernel would, and hands out its `dlopen`/`dlsym`.
//! Calls into the loaded code go through trampolines that switch the thread
//! pointer, so the host runtime and the system libc can coexist.
//!
//! See `docs/design.md` for the rationale and the known hazards.
//!
//! ```ignore
//! use dldota::{Context, OpenFlags};
//!
//! let ctx = Context::new()?;
//! let libz = ctx.open(c"libz.so.1", OpenFlags::NOW)?;
//! let version = unsafe { libz.symbol::<unsafe extern "C" fn() -> *const core::ffi::c_char>(c"zlibVersion")? };
//! ```

// fullrust (libc-free static Rust) is Linux underneath but reports its own OS.
#[cfg(not(any(target_os = "linux", target_os = "fullrust")))]
compile_error!("dldota only supports Linux");

mod arch;
mod context;
mod error;
pub mod ffi;
mod heap;
mod loader;

pub use context::{Callback, Context, ContextBuilder, Library, OpenFlags, Symbol};
pub use error::{Error, Result};
pub use heap::HeapUsage;
