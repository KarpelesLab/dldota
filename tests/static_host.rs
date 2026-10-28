//! End-to-end tests meant to run from a fully static, libc-free host built
//! with fullrust (see `scripts/fullrust-tests.sh`), against each system loader.

use std::ffi::{CStr, c_char};

use dldota::{Context, OpenFlags};

/// These tests only prove something when the host has no dynamic loader of
/// its own: make sure the fullrust build really is one.
#[cfg(target_os = "fullrust")]
#[test]
fn host_has_no_interpreter() {
    const PT_INTERP: u32 = 3;
    let exe = std::fs::read("/proc/self/exe").unwrap();
    let field = |off: usize, len: usize| {
        let mut buf = [0u8; 8];
        buf[..len].copy_from_slice(&exe[off..off + len]);
        u64::from_le_bytes(buf) as usize
    };
    let (phoff, phentsize, phnum) = (field(32, 8), field(54, 2), field(56, 2));
    let interp = (0..phnum).any(|i| field(phoff + i * phentsize, 4) == PT_INTERP as usize);
    assert!(!interp, "fullrust test binary unexpectedly has PT_INTERP");
}

#[test]
#[cfg_attr(
    not(target_arch = "x86_64"),
    ignore = "loader bootstrap not implemented on this architecture yet"
)]
fn zlib_version() {
    let ctx = Context::new().expect("boot the system loader");
    let libz = ctx.open(c"libz.so.1", OpenFlags::NOW).expect("open libz");
    // SAFETY: zlibVersion has this signature in every zlib release.
    let sym = unsafe { libz.symbol::<unsafe extern "C" fn() -> *const c_char>(c"zlibVersion") };
    let zlib_version = *sym.expect("look up zlibVersion");
    // SAFETY: returns a static NUL-terminated string.
    let version = unsafe { CStr::from_ptr(zlib_version()) };
    assert!(
        version.to_bytes().starts_with(b"1."),
        "unexpected zlib version {version:?}"
    );
}
