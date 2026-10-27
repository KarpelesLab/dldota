//! The per-context allocator, interposed as the foreign `malloc` family.
//!
//! It runs with the foreign thread pointer, often on foreign-created threads,
//! so it never touches host TLS or the host allocator: it manages regions it
//! maps itself, all of which are unmapped when the context unloads.

/// Snapshot of a context's foreign heap.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapUsage {
    /// Allocations made by foreign code and not yet freed.
    pub live_allocations: usize,
    /// Bytes requested by those allocations.
    pub live_bytes: usize,
    /// Bytes of address space the allocator has mapped.
    pub mapped_bytes: usize,
}
