//! Every foreign allocation, libc-internal ones included, must go through the
//! context's allocator, so it can be accounted for and reclaimed at unload.

mod common;

use std::ffi::{c_char, c_int, c_void};

use common::{context, global, sym};

type Malloc = unsafe extern "C" fn(usize) -> *mut c_void;
type Realloc = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;
type Free = unsafe extern "C" fn(*mut c_void);
type PosixMemalign = unsafe extern "C" fn(*mut *mut c_void, usize, usize) -> c_int;
type Strdup = unsafe extern "C" fn(*const c_char) -> *mut c_char;
type Fopen = unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_void;
type Fclose = unsafe extern "C" fn(*mut c_void) -> c_int;

#[test]
#[ignore = "loader bootstrap not implemented yet"]
fn foreign_allocations_use_the_context_heap() {
    let ctx = context();
    let libc = global(&ctx);
    // SAFETY: signatures from stdlib.h, string.h and stdio.h.
    let (malloc, realloc, free, posix_memalign, strdup, fopen, fclose) = unsafe {
        (
            sym::<Malloc>(&libc, c"malloc"),
            sym::<Realloc>(&libc, c"realloc"),
            sym::<Free>(&libc, c"free"),
            sym::<PosixMemalign>(&libc, c"posix_memalign"),
            sym::<Strdup>(&libc, c"strdup"),
            sym::<Fopen>(&libc, c"fopen"),
            sym::<Fclose>(&libc, c"fclose"),
        )
    };
    let base = ctx.heap_usage();
    let back_to_base = |what: &str| {
        let now = ctx.heap_usage();
        assert_eq!(
            (now.live_allocations, now.live_bytes),
            (base.live_allocations, base.live_bytes),
            "{what}: heap not back to baseline"
        );
    };

    // strdup allocates inside libc.
    // SAFETY: valid C string; the copy is freed with the foreign free.
    unsafe {
        let copy = strdup(c"dldota".as_ptr());
        assert!(!copy.is_null());
        let now = ctx.heap_usage();
        assert_eq!(now.live_allocations, base.live_allocations + 1);
        assert!(now.live_bytes >= base.live_bytes + 7);
        assert!(now.mapped_bytes >= now.live_bytes);
        free(copy.cast());
    }
    back_to_base("strdup/free");

    // Alignment is honoured, up to page-plus sizes.
    for align in [16, 64, 4096, 1 << 16] {
        let mut p = std::ptr::null_mut();
        // SAFETY: power-of-two alignment, multiple of the pointer size.
        unsafe {
            assert_eq!(
                posix_memalign(&mut p, align, 1000),
                0,
                "posix_memalign({align})"
            );
            assert_eq!(p as usize % align, 0, "misaligned for {align}");
            assert_eq!(ctx.heap_usage().live_allocations, base.live_allocations + 1);
            free(p);
        }
    }
    back_to_base("posix_memalign");

    // realloc keeps the contents across small, medium and huge sizes.
    // SAFETY: each pointer is the latest realloc result; sizes cover the reads.
    unsafe {
        let mut p = malloc(16).cast::<u8>();
        for i in 0..16 {
            p.add(i).write(i as u8);
        }
        for size in [100_000, 8 << 20, 32] {
            p = realloc(p.cast(), size).cast();
            assert!(!p.is_null(), "realloc to {size}");
            let head: Vec<u8> = (0..16).map(|i| p.add(i).read()).collect();
            assert_eq!(
                head,
                (0..16).collect::<Vec<u8>>(),
                "contents lost at {size}"
            );
        }
        free(p.cast());
    }
    back_to_base("realloc");

    // fopen allocates its FILE through malloc inside libc, not via our call.
    // SAFETY: valid C strings; the stream is closed below.
    unsafe {
        let f = fopen(c"/proc/self/maps".as_ptr(), c"r".as_ptr());
        assert!(!f.is_null());
        assert!(
            ctx.heap_usage().live_allocations > base.live_allocations,
            "fopen bypassed the context heap"
        );
        assert_eq!(fclose(f), 0);
    }
    back_to_base("fopen/fclose");
}
