//! Unloading must refuse, and hand the context back intact, while threads
//! created by foreign code are alive: unmapping their code would crash them.

mod common;

use std::ffi::{c_int, c_ulong, c_void};

use dldota::{Error, OpenFlags};

use common::{context, corpus, open, sym};

type FreeCCtx = unsafe extern "C" fn(*mut c_void) -> usize;

#[test]
#[ignore = "unload not implemented yet"]
fn unload_refused_while_foreign_threads_run() {
    let ctx = context();
    // NODELETE keeps libzstd (and its idle workers' code) mapped after the
    // Library handle is dropped, which unload() requires.
    let (cctx, free) = {
        let zstd = ctx
            .open(c"libzstd.so.1", OpenFlags::NOW | OpenFlags::NODELETE)
            .expect("open libzstd");
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
            let cctx = create();
            // Returns the value set, or an error code (near usize::MAX).
            assert!(set_param(cctx, 400 /* ZSTD_c_nbWorkers */, 2) < usize::MAX - 200);
            let input = corpus(1 << 20);
            let mut out = vec![0u8; bound(input.len())];
            assert!(
                compress2(
                    cctx,
                    out.as_mut_ptr(),
                    out.len(),
                    input.as_ptr(),
                    input.len()
                ) > 0
            );
            (cctx, sym::<FreeCCtx>(&zstd, c"ZSTD_freeCCtx"))
        }
    };

    // The worker pool is still alive: unload must refuse.
    let ctx = match ctx.unload() {
        Ok(()) => panic!("unload succeeded while zstd workers were alive"),
        Err((ctx, Error::Busy(_))) => ctx,
        Err((_, e)) => panic!("expected Error::Busy, got: {e}"),
    };

    // The context is intact and still usable.
    {
        let libz = open(&ctx, c"libz.so.1");
        // SAFETY: compressBound from zlib.h.
        let bound =
            unsafe { sym::<unsafe extern "C" fn(c_ulong) -> c_ulong>(&libz, c"compressBound") };
        assert!(unsafe { bound(1000) } > 1000);
    }

    // Joining the workers makes it unloadable.
    // SAFETY: libzstd is NODELETE, so `free` is still mapped; cctx is ours.
    assert_eq!(unsafe { free(cctx) }, 0);
    if let Err((_, e)) = ctx.unload() {
        panic!("unload after joining the workers: {e}");
    }
}
