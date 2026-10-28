//! A context must unload completely: boot, use and unload it many times and
//! the process footprint (mappings, threads, fds, memory) must not grow.
//!
//! The only test in this binary, so nothing else changes the footprint.

mod common;

use std::ffi::{c_int, c_ulong, c_void};

use common::{Footprint, context, corpus, footprint, open, sym};

const CYCLES: usize = 100;
/// Allowed growth of anonymous memory over all cycles (host allocator noise).
const ANON_SLACK: usize = 1 << 20;

type CompressBound = unsafe extern "C" fn(c_ulong) -> c_ulong;

/// Boot, load libz and libzstd, run a multithreaded zstd compression (so
/// foreign threads come and go), then unload.
fn cycle() {
    let ctx = context();
    {
        let libz = open(&ctx, c"libz.so.1");
        // SAFETY: compressBound from zlib.h.
        let bound = unsafe { sym::<CompressBound>(&libz, c"compressBound") };
        assert!(unsafe { bound(1000) } > 1000);

        let zstd = open(&ctx, c"libzstd.so.1");
        // SAFETY: signatures from zstd.h.
        unsafe {
            let create = sym::<unsafe extern "C" fn() -> *mut c_void>(&zstd, c"ZSTD_createCCtx");
            let set_param = sym::<unsafe extern "C" fn(*mut c_void, c_int, c_int) -> usize>(
                &zstd,
                c"ZSTD_CCtx_setParameter",
            );
            let bound = sym::<unsafe extern "C" fn(usize) -> usize>(&zstd, c"ZSTD_compressBound");
            let compress2 = sym::<
                unsafe extern "C" fn(*mut c_void, *mut u8, usize, *const u8, usize) -> usize,
            >(&zstd, c"ZSTD_compress2");
            let free = sym::<unsafe extern "C" fn(*mut c_void) -> usize>(&zstd, c"ZSTD_freeCCtx");
            let input = corpus(1 << 20);
            let cctx = create();
            assert!(!cctx.is_null());
            assert_eq!(set_param(cctx, 400 /* ZSTD_c_nbWorkers */, 2), 0);
            let mut out = vec![0u8; bound(input.len())];
            let n = compress2(
                cctx,
                out.as_mut_ptr(),
                out.len(),
                input.as_ptr(),
                input.len(),
            );
            assert!(n > 0 && n < input.len());
            assert_eq!(free(cctx), 0);
        }
    }
    if let Err((_, e)) = ctx.unload() {
        panic!("unload: {e}");
    }
}

#[test]
#[ignore = "unload not implemented yet"]
fn boot_use_unload_cycles() {
    let initial = footprint();
    // Warm-up: lets the host allocator and std reach a steady state.
    cycle();
    let baseline = footprint();
    for _ in 0..CYCLES {
        cycle();
    }
    let after = footprint();

    let leaked_files = |reference: &Footprint| {
        after
            .file_mappings
            .iter()
            .filter(|(path, n)| reference.file_mappings.get(*path) != Some(n))
            .map(|(path, n)| format!("{path} x{n}"))
            .collect::<Vec<_>>()
    };
    let leaked = leaked_files(&initial);
    assert!(leaked.is_empty(), "file mappings left behind: {leaked:?}");
    assert_eq!(after.threads, initial.threads, "threads left behind");
    assert_eq!(after.fds, initial.fds, "file descriptors left behind");
    assert!(
        after.anon_bytes <= baseline.anon_bytes + ANON_SLACK,
        "anonymous memory grew by {} KiB over {CYCLES} cycles",
        (after.anon_bytes - baseline.anon_bytes) / 1024
    );
}
