//! Threads created *inside* the foreign libc: zstd's multithreaded compressor
//! spawns a worker pool with the system's `pthread_create` and joins it in
//! `ZSTD_freeCCtx`.
//!
//! Kept as the only test in this binary so `/proc/self/task` counts aren't
//! disturbed by other tests running in parallel.

mod common;

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};

use common::{context, corpus, open, sym, thread_count};

const ZSTD_C_COMPRESSION_LEVEL: c_int = 100;
const ZSTD_C_NB_WORKERS: c_int = 400;
const WORKERS: c_int = 4;

#[test]
#[ignore = "loader bootstrap not implemented yet"]
fn zstd_worker_pool() {
    let ctx = context();
    let zstd = open(&ctx, c"libzstd.so.1");
    // SAFETY: signatures from zstd.h (stable API).
    let (create, set_param, compress2, bound, free, decompress, is_error, error_name) = unsafe {
        (
            sym::<unsafe extern "C" fn() -> *mut c_void>(&zstd, c"ZSTD_createCCtx"),
            sym::<unsafe extern "C" fn(*mut c_void, c_int, c_int) -> usize>(
                &zstd,
                c"ZSTD_CCtx_setParameter",
            ),
            sym::<unsafe extern "C" fn(*mut c_void, *mut u8, usize, *const u8, usize) -> usize>(
                &zstd,
                c"ZSTD_compress2",
            ),
            sym::<unsafe extern "C" fn(usize) -> usize>(&zstd, c"ZSTD_compressBound"),
            sym::<unsafe extern "C" fn(*mut c_void) -> usize>(&zstd, c"ZSTD_freeCCtx"),
            sym::<unsafe extern "C" fn(*mut u8, usize, *const u8, usize) -> usize>(
                &zstd,
                c"ZSTD_decompress",
            ),
            sym::<unsafe extern "C" fn(usize) -> c_uint>(&zstd, c"ZSTD_isError"),
            sym::<unsafe extern "C" fn(usize) -> *const c_char>(&zstd, c"ZSTD_getErrorName"),
        )
    };
    let check = |what: &str, code: usize| -> usize {
        // SAFETY: plain value functions; the name is a static string.
        if unsafe { is_error(code) } != 0 {
            let name = unsafe { CStr::from_ptr(error_name(code)) };
            panic!("{what}: {name:?}");
        }
        code
    };

    // Big enough that zstd splits it into several jobs across the workers.
    let input = corpus(48 << 20);
    let before = thread_count();

    // SAFETY: buffers are sized per zstd's contract; cctx is used only here.
    let compressed = unsafe {
        let cctx = create();
        assert!(!cctx.is_null());
        check(
            "compression level",
            set_param(cctx, ZSTD_C_COMPRESSION_LEVEL, 3),
        );
        check(
            "nbWorkers (libzstd built without threads?)",
            set_param(cctx, ZSTD_C_NB_WORKERS, WORKERS),
        );
        let mut out = vec![0u8; bound(input.len())];
        let n = check(
            "compress",
            compress2(
                cctx,
                out.as_mut_ptr(),
                out.len(),
                input.as_ptr(),
                input.len(),
            ),
        );
        out.truncate(n);

        let during = thread_count();
        assert!(
            during >= before + WORKERS as usize,
            "expected {WORKERS} foreign worker threads, saw {before} -> {during}"
        );
        check("free", free(cctx));
        out
    };
    assert_eq!(
        thread_count(),
        before,
        "worker threads not joined by ZSTD_freeCCtx"
    );
    assert!(
        compressed.len() < input.len() / 2,
        "suspiciously poor ratio"
    );

    let mut roundtrip = vec![0u8; input.len()];
    // SAFETY: output buffer holds the full original size.
    let n = check("decompress", unsafe {
        decompress(
            roundtrip.as_mut_ptr(),
            roundtrip.len(),
            compressed.as_ptr(),
            compressed.len(),
        )
    });
    assert_eq!(n, input.len());
    assert!(roundtrip == input, "roundtrip mismatch");
}
