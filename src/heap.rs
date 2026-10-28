//! The per-context allocator, interposed as the foreign `malloc` family.
//!
//! It runs with the foreign thread pointer, often on foreign-created threads,
//! so it never touches host TLS, the host allocator, or anything in std that
//! might (std locks may call the host libc): it manages memory it maps itself
//! with raw syscalls, behind a futex lock.
//!
//! Small sizes (up to 32 KiB) come from size-class slabs in 64 KiB segments
//! aligned to 64 KiB, so `ptr & !(SEG - 1)` finds the segment header. Larger
//! blocks get their own mapping with the header at its start. A pointer that
//! is itself 64 KiB-aligned can only be a large block with alignment of at
//! least 64 KiB; its header sits just below it.

use std::ffi::{CStr, c_int, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::lock::RawLock;

use crate::sys;

/// Snapshot of a context's foreign heap.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapUsage {
    /// Allocations made by foreign code and not yet freed.
    pub live_allocations: usize,
    /// Usable bytes of those allocations (requests rounded to size classes).
    pub live_bytes: usize,
    /// Bytes of address space the allocator has mapped.
    pub mapped_bytes: usize,
}

const SEG: usize = 64 << 10;
const HDR: usize = 64;
const MIN_ALIGN: usize = 16;
const MAGIC_SMALL: u64 = 0x646c_646f_7461_5345; // "dldotaSE"
const MAGIC_LARGE: u64 = 0x646c_646f_7461_4c47; // "dldotaLG"
const ENOMEM: c_int = 12;
const EINVAL: c_int = 22;

const CLASSES: [usize; 40] = {
    let mut c = [0; 40];
    let mut i = 0;
    while i < 8 {
        c[i] = (i + 1) * 16;
        i += 1;
    }
    let mut pow = 128;
    while i < 40 {
        c[i] = pow * 5 / 4;
        c[i + 1] = pow * 6 / 4;
        c[i + 2] = pow * 7 / 4;
        c[i + 3] = pow * 2;
        pow *= 2;
        i += 4;
    }
    c
};
const MAX_SMALL: usize = CLASSES[CLASSES.len() - 1];

/// At the start of every segment and large mapping.
#[repr(C)]
struct Header {
    magic: u64,
    class: usize,
    /// The mapping this header describes.
    map_start: usize,
    map_len: usize,
    /// Segments: next segment. Large: neighbours in the large list.
    next: usize,
    prev: usize,
    _pad: [usize; 2],
}
const _: () = assert!(size_of::<Header>() == HDR);

struct State {
    free: [usize; CLASSES.len()],
    bump: [(usize, usize); CLASSES.len()],
    segments: usize,
    large: usize,
    usage: HeapUsage,
}

impl State {
    const EMPTY: State = State {
        free: [0; CLASSES.len()],
        bump: [(0, 0); CLASSES.len()],
        segments: 0,
        large: 0,
        usage: HeapUsage {
            live_allocations: 0,
            live_bytes: 0,
            mapped_bytes: 0,
        },
    };
}

pub(crate) struct Heap {
    state: RawLock<State>,
    page: usize,
    /// The foreign `__errno_location`, once the loader resolved it.
    errno_location: AtomicUsize,
}

/// Abort without touching std: we may be on a foreign thread.
fn die(msg: &str) -> ! {
    let _ = sys::write(2, msg.as_bytes());
    crate::arch::trap()
}

fn round_up(v: usize, to: usize) -> usize {
    (v + to - 1) & !(to - 1)
}

fn class_of(size: usize) -> Option<usize> {
    CLASSES.iter().position(|&c| c >= size)
}

impl Heap {
    pub(crate) fn new(page: usize) -> Self {
        Heap {
            state: RawLock::new(State::EMPTY),
            page,
            errno_location: AtomicUsize::new(0),
        }
    }

    pub(crate) fn set_errno_location(&self, f: usize) {
        self.errno_location.store(f, Ordering::Release);
    }

    pub(crate) fn usage(&self) -> HeapUsage {
        self.with(|s| s.usage)
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        self.state.with(f)
    }

    fn set_errno(&self, err: c_int) {
        let f = self.errno_location.load(Ordering::Acquire);
        if f != 0 {
            // SAFETY: the foreign __errno_location, called with the foreign
            // TP (we only run from foreign code).
            unsafe {
                let f: unsafe extern "C" fn() -> *mut c_int = std::mem::transmute(f);
                *f() = err;
            }
        }
    }

    /// Map `len` bytes at an `align`-aligned address (`align` ≥ page).
    fn map_aligned(&self, len: usize, align: usize, usage: &mut HeapUsage) -> Option<usize> {
        let over = len + align;
        // SAFETY: a fresh private mapping.
        let start = unsafe {
            sys::mmap(
                0,
                over,
                sys::PROT_READ | sys::PROT_WRITE,
                sys::MAP_PRIVATE | sys::MAP_ANONYMOUS,
                -1,
                0,
            )
        }
        .ok()?;
        let aligned = round_up(start, align);
        // SAFETY: trimming the unused ends of our own mapping.
        unsafe {
            if aligned > start {
                let _ = sys::munmap(start, aligned - start);
            }
            let end = aligned + len;
            if start + over > end {
                let _ = sys::munmap(end, start + over - end);
            }
        }
        usage.mapped_bytes += len;
        Some(aligned)
    }

    fn alloc_small(&self, s: &mut State, class: usize) -> Option<usize> {
        let size = CLASSES[class];
        let obj = if s.free[class] != 0 {
            let obj = s.free[class];
            // SAFETY: free objects store the next free object in their first word.
            s.free[class] = unsafe { *(obj as *const usize) };
            obj
        } else {
            let (cur, end) = s.bump[class];
            if cur != 0 && cur + size <= end {
                s.bump[class].0 = cur + size;
                cur
            } else {
                let seg = self.map_aligned(SEG, SEG, &mut s.usage)?;
                // SAFETY: a fresh segment we own.
                unsafe {
                    (seg as *mut Header).write(Header {
                        magic: MAGIC_SMALL,
                        class,
                        map_start: seg,
                        map_len: SEG,
                        next: s.segments,
                        prev: 0,
                        _pad: [0; 2],
                    })
                };
                s.segments = seg;
                s.bump[class] = (seg + HDR + size, seg + SEG);
                seg + HDR
            }
        };
        s.usage.live_allocations += 1;
        s.usage.live_bytes += size;
        Some(obj)
    }

    fn alloc_large(&self, s: &mut State, size: usize, align: usize) -> Option<usize> {
        let (start, len, ptr) = if align < SEG {
            let off = HDR.max(align);
            let len = round_up(off + size, self.page);
            let start = self.map_aligned(len, SEG, &mut s.usage)?;
            (start, len, start + off)
        } else {
            let len = round_up(HDR + size, self.page) + align;
            let start = self.map_aligned(len, self.page, &mut s.usage)?;
            (start, len, round_up(start + HDR, align))
        };
        let hdr = if ptr.is_multiple_of(SEG) {
            ptr - HDR
        } else {
            ptr & !(SEG - 1)
        };
        // SAFETY: the header lies inside the fresh mapping, below `ptr`.
        unsafe {
            (hdr as *mut Header).write(Header {
                magic: MAGIC_LARGE,
                class: usize::MAX,
                map_start: start,
                map_len: len,
                next: s.large,
                prev: 0,
                _pad: [0; 2],
            });
            if s.large != 0 {
                (*(s.large as *mut Header)).prev = hdr;
            }
        }
        s.large = hdr;
        s.usage.live_allocations += 1;
        s.usage.live_bytes += start + len - ptr;
        Some(ptr)
    }

    pub(crate) fn alloc(&self, size: usize, align: usize) -> *mut c_void {
        let size = size.max(1);
        let align = align.max(MIN_ALIGN);
        let ptr = self.with(|s| {
            let need = size.checked_add(align - MIN_ALIGN)?;
            match class_of(need) {
                Some(class) => self.alloc_small(s, class).map(|obj| round_up(obj, align)),
                None => self.alloc_large(s, size, align),
            }
        });
        match ptr {
            Some(p) => p as *mut c_void,
            None => {
                self.set_errno(ENOMEM);
                std::ptr::null_mut()
            }
        }
    }

    /// The header for `ptr`, validated.
    fn header(ptr: usize) -> &'static mut Header {
        let hdr = if ptr.is_multiple_of(SEG) {
            ptr - HDR
        } else {
            ptr & !(SEG - 1)
        };
        // SAFETY: every pointer we hand out has a header at this address; the
        // magic check catches (some) foreign bugs.
        let h = unsafe { &mut *(hdr as *mut Header) };
        if h.magic != MAGIC_SMALL && h.magic != MAGIC_LARGE {
            die("dldota: free/realloc of a pointer not from this context's heap\n");
        }
        h
    }

    /// Start of the slab object containing `ptr`.
    fn object_start(h: &Header, ptr: usize) -> usize {
        let base = h.map_start + HDR;
        base + (ptr - base) / CLASSES[h.class] * CLASSES[h.class]
    }

    fn usable(ptr: usize) -> usize {
        let h = Self::header(ptr);
        if h.magic == MAGIC_SMALL {
            Self::object_start(h, ptr) + CLASSES[h.class] - ptr
        } else {
            h.map_start + h.map_len - ptr
        }
    }

    pub(crate) fn free(&self, ptr: *mut c_void) {
        let ptr = ptr as usize;
        if ptr == 0 {
            return;
        }
        self.with(|s| {
            let h = Self::header(ptr);
            if h.magic == MAGIC_SMALL {
                let obj = Self::object_start(h, ptr);
                // SAFETY: the object is ours and no longer in use.
                unsafe { *(obj as *mut usize) = s.free[h.class] };
                s.free[h.class] = obj;
                s.usage.live_allocations -= 1;
                s.usage.live_bytes -= CLASSES[h.class];
            } else {
                // SAFETY: unlinking a live large block, then unmapping it.
                unsafe {
                    if h.prev != 0 {
                        (*(h.prev as *mut Header)).next = h.next;
                    } else {
                        s.large = h.next;
                    }
                    if h.next != 0 {
                        (*(h.next as *mut Header)).prev = h.prev;
                    }
                    s.usage.live_allocations -= 1;
                    s.usage.live_bytes -= h.map_start + h.map_len - ptr;
                    s.usage.mapped_bytes -= h.map_len;
                    let _ = sys::munmap(h.map_start, h.map_len);
                }
            }
        });
    }

    fn realloc(&self, ptr: *mut c_void, size: usize) -> *mut c_void {
        if ptr.is_null() {
            return self.alloc(size, MIN_ALIGN);
        }
        if size == 0 {
            self.free(ptr);
            return std::ptr::null_mut();
        }
        let old = Self::usable(ptr as usize);
        // Keep the block if it fits and isn't grossly oversized.
        if size <= old && (old <= MAX_SMALL || size > old / 2) {
            return ptr;
        }
        let new = self.alloc(size, MIN_ALIGN);
        if !new.is_null() {
            // SAFETY: both blocks are live and at least min(old, size) long.
            unsafe {
                std::ptr::copy_nonoverlapping(ptr as *const u8, new as *mut u8, old.min(size))
            };
            self.free(ptr);
        }
        new
    }

    /// Unmap everything. Only for unload: every pointer becomes invalid.
    pub(crate) fn release_all(&self) {
        self.with(|s| {
            // SAFETY: walking our own lists, then unmapping what they describe.
            unsafe {
                while s.large != 0 {
                    let h = &*(s.large as *const Header);
                    s.large = h.next;
                    let _ = sys::munmap(h.map_start, h.map_len);
                }
                while s.segments != 0 {
                    let seg = s.segments;
                    s.segments = (*(seg as *const Header)).next;
                    let _ = sys::munmap(seg, SEG);
                }
            }
            *s = State::EMPTY;
        });
    }
}

// The interposed functions. Each is reached through a thunk that prepends the
// context's heap to the C arguments (see `Thunks::bound`).

extern "C" fn malloc(heap: &Heap, size: usize) -> *mut c_void {
    heap.alloc(size, MIN_ALIGN)
}

extern "C" fn free(heap: &Heap, ptr: *mut c_void) {
    heap.free(ptr)
}

extern "C" fn calloc(heap: &Heap, n: usize, size: usize) -> *mut c_void {
    let Some(total) = n.checked_mul(size) else {
        heap.set_errno(ENOMEM);
        return std::ptr::null_mut();
    };
    let p = heap.alloc(total, MIN_ALIGN);
    if !p.is_null() {
        // SAFETY: freshly allocated, `total` bytes long. Recycled slab
        // objects aren't zero; fresh mappings are, but this keeps it simple.
        unsafe { std::ptr::write_bytes(p as *mut u8, 0, total) };
    }
    p
}

extern "C" fn realloc(heap: &Heap, ptr: *mut c_void, size: usize) -> *mut c_void {
    heap.realloc(ptr, size)
}

extern "C" fn reallocarray(heap: &Heap, ptr: *mut c_void, n: usize, size: usize) -> *mut c_void {
    match n.checked_mul(size) {
        Some(total) => heap.realloc(ptr, total),
        None => {
            heap.set_errno(ENOMEM);
            std::ptr::null_mut()
        }
    }
}

extern "C" fn memalign(heap: &Heap, align: usize, size: usize) -> *mut c_void {
    if !align.is_power_of_two() {
        heap.set_errno(EINVAL);
        return std::ptr::null_mut();
    }
    heap.alloc(size, align)
}

extern "C" fn aligned_alloc(heap: &Heap, align: usize, size: usize) -> *mut c_void {
    memalign(heap, align, size)
}

extern "C" fn posix_memalign(
    heap: &Heap,
    out: *mut *mut c_void,
    align: usize,
    size: usize,
) -> c_int {
    if !align.is_power_of_two() || !align.is_multiple_of(size_of::<usize>()) {
        return EINVAL;
    }
    let p = heap.alloc(size, align);
    if p.is_null() {
        return ENOMEM;
    }
    // SAFETY: the caller passes a valid out pointer.
    unsafe { out.write(p) };
    0
}

extern "C" fn valloc(heap: &Heap, size: usize) -> *mut c_void {
    heap.alloc(size, heap.page)
}

extern "C" fn pvalloc(heap: &Heap, size: usize) -> *mut c_void {
    heap.alloc(round_up(size.max(1), heap.page), heap.page)
}

extern "C" fn malloc_usable_size(_heap: &Heap, ptr: *mut c_void) -> usize {
    if ptr.is_null() {
        0
    } else {
        Heap::usable(ptr as usize)
    }
}

extern "C" fn free_sized(heap: &Heap, ptr: *mut c_void, _size: usize) {
    heap.free(ptr)
}

extern "C" fn free_aligned_sized(heap: &Heap, ptr: *mut c_void, _align: usize, _size: usize) {
    heap.free(ptr)
}

/// The interposed symbols and their implementations (at most 4 C arguments,
/// so the heap-prepending thunk can shift them).
pub(crate) fn exports() -> [(&'static CStr, usize); 13] {
    [
        (c"malloc", malloc as *const () as usize),
        (c"free", free as *const () as usize),
        (c"calloc", calloc as *const () as usize),
        (c"realloc", realloc as *const () as usize),
        (c"reallocarray", reallocarray as *const () as usize),
        (c"memalign", memalign as *const () as usize),
        (c"aligned_alloc", aligned_alloc as *const () as usize),
        (c"posix_memalign", posix_memalign as *const () as usize),
        (c"valloc", valloc as *const () as usize),
        (c"pvalloc", pvalloc as *const () as usize),
        (
            c"malloc_usable_size",
            malloc_usable_size as *const () as usize,
        ),
        (c"free_sized", free_sized as *const () as usize),
        (
            c"free_aligned_sized",
            free_aligned_sized as *const () as usize,
        ),
    ]
}
