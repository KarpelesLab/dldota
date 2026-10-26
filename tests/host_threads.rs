//! Many host (Rust) threads calling into the foreign libc at once: every call
//! swaps the thread pointer, and zlib's allocations go through the foreign
//! malloc, so any sharing of foreign per-thread state shows up as corruption.

mod common;

use std::ffi::{c_int, c_ulong};

use common::{context, corpus, open, sym};

type Compress2 = unsafe extern "C" fn(*mut u8, *mut c_ulong, *const u8, c_ulong, c_int) -> c_int;
type Uncompress = unsafe extern "C" fn(*mut u8, *mut c_ulong, *const u8, c_ulong) -> c_int;
type CompressBound = unsafe extern "C" fn(c_ulong) -> c_ulong;

const THREADS: usize = 16;
const ROUNDS: usize = 200;
const Z_OK: c_int = 0;

#[test]
#[ignore = "loader bootstrap not implemented yet"]
fn concurrent_zlib_roundtrips() {
    let ctx = context();
    let libz = open(&ctx, c"libz.so.1");
    // SAFETY: signatures from zlib.h.
    let (compress2, uncompress, bound) = unsafe {
        (
            sym::<Compress2>(&libz, c"compress2"),
            sym::<Uncompress>(&libz, c"uncompress"),
            sym::<CompressBound>(&libz, c"compressBound"),
        )
    };

    std::thread::scope(|s| {
        for t in 0..THREADS {
            s.spawn(move || {
                for round in 0..ROUNDS {
                    // Distinct sizes and levels per thread and round.
                    let input = corpus(4096 + t * 977 + round * 131);
                    let level = ((t + round) % 10) as c_int;
                    // SAFETY: buffers are sized per zlib's contract.
                    unsafe {
                        let mut packed = vec![0u8; bound(input.len() as c_ulong) as usize];
                        let mut packed_len = packed.len() as c_ulong;
                        let rc = compress2(
                            packed.as_mut_ptr(),
                            &mut packed_len,
                            input.as_ptr(),
                            input.len() as c_ulong,
                            level,
                        );
                        assert_eq!(rc, Z_OK, "compress2 thread {t} round {round}");

                        let mut out = vec![0u8; input.len()];
                        let mut out_len = out.len() as c_ulong;
                        let rc =
                            uncompress(out.as_mut_ptr(), &mut out_len, packed.as_ptr(), packed_len);
                        assert_eq!(rc, Z_OK, "uncompress thread {t} round {round}");
                        assert!(out == input, "roundtrip mismatch thread {t} round {round}");
                    }
                }
            });
        }
    });
}
