//! The synthesized in-memory "main program" handed to `ld.so` via `AT_PHDR`.
//!
//! It carries `PT_PHDR`/`PT_INTERP`/`PT_LOAD`/`PT_DYNAMIC`, a `.dynamic` with
//! `DT_NEEDED libc.so.6`, and `GLOB_DAT` relocations that make `ld.so` fill a
//! GOT with `dlopen`, `dlsym`, `dlclose`, `dlerror`, ... for us. `AT_ENTRY`
//! points at the arch entry thunk that returns control to `bootstrap()`.
//!
//! It will also *define* the symbols we interpose (first in every lookup scope):
//! the `malloc` family (backed by [`crate::heap`]), `pthread_create`, and
//! `__cxa_atexit`/`__cxa_finalize`/`atexit`.
//!
//! Every address inside is relative to the block's start, which is the
//! object's load bias (`PT_PHDR` has vaddr 0).

use std::ffi::CStr;

use super::elf::{PF_R, PF_W, PHDR_SIZE, PT_DYNAMIC, PT_GNU_STACK, PT_INTERP, PT_LOAD, PT_PHDR};
use crate::arch;
use crate::error::Result;
use crate::sys;

const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_HASH: u64 = 4;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_RELAENT: u64 = 9;
const DT_STRSZ: u64 = 10;
const DT_SYMENT: u64 = 11;
const DT_DEBUG: u64 = 21;
/// STB_GLOBAL << 4 | STT_FUNC
const GLOBAL_FUNC: u8 = 0x12;

pub(crate) struct Stub {
    /// The block: `(start, len)`. Also the object's load bias.
    pub region: (usize, usize),
    /// Runtime address of the program headers (`AT_PHDR`).
    pub phdr: usize,
    pub phnum: usize,
    /// Runtime address of the GOT: one slot per import, in order.
    pub got: usize,
    /// Runtime address of the `DT_DEBUG` value, where ld.so stores `&r_debug`.
    pub debug_slot: usize,
}

/// Byte buffer with 8-byte aligned appends.
#[derive(Default)]
struct Buf(Vec<u8>);

impl Buf {
    fn align(&mut self) -> usize {
        self.0.resize(self.0.len().next_multiple_of(8), 0);
        self.0.len()
    }
    fn put(&mut self, bytes: &[u8]) -> usize {
        let at = self.0.len();
        self.0.extend_from_slice(bytes);
        at
    }
    fn u16(&mut self, v: u16) {
        self.put(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.put(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.put(&v.to_le_bytes());
    }
    fn phdr(&mut self, p_type: u32, flags: u32, off: usize, size: usize, align: u64) {
        self.u32(p_type);
        self.u32(flags);
        self.u64(off as u64); // p_offset
        self.u64(off as u64); // p_vaddr
        self.u64(off as u64); // p_paddr
        self.u64(size as u64); // p_filesz
        self.u64(size as u64); // p_memsz
        self.u64(align);
    }
}

/// Build the main program: depends on `needed`, imports `imports` into a GOT,
/// and defines `defs` (name, absolute address), which then take precedence
/// over every library's definition.
pub(crate) fn build(
    page: usize,
    interp: &CStr,
    needed: &[&CStr],
    imports: &[&CStr],
    defs: &[(&CStr, usize)],
) -> Result<Stub> {
    const PHNUM: usize = 5;
    let nsyms = imports.len() + defs.len() + 1;

    // Lay out everything after the program headers first; the headers need
    // the final offsets, so they are written last into the reserved space.
    let mut b = Buf::default();
    b.put(&[0; PHNUM * PHDR_SIZE]);

    let interp_off = b.put(interp.to_bytes_with_nul());

    let strtab_off = b.put(&[0]);
    let needed_names: Vec<usize> = needed
        .iter()
        .map(|n| b.put(n.to_bytes_with_nul()) - strtab_off)
        .collect();
    let import_names: Vec<usize> = imports
        .iter()
        .map(|n| b.put(n.to_bytes_with_nul()) - strtab_off)
        .collect();
    let def_names: Vec<usize> = defs
        .iter()
        .map(|(n, _)| b.put(n.to_bytes_with_nul()) - strtab_off)
        .collect();
    let strsz = b.0.len() - strtab_off;

    let symtab_off = b.align();
    b.put(&[0; 24]); // index 0: the null symbol
    for &name in &import_names {
        b.u32(name as u32);
        b.put(&[GLOBAL_FUNC, 0]); // st_info, st_other
        b.u16(0); // SHN_UNDEF
        b.u64(0); // st_value
        b.u64(0); // st_size
    }
    // Defined symbols: any section index but SHN_UNDEF/SHN_ABS, so every
    // loader adds the load bias to st_value; patched once the block is mapped.
    let mut def_values = vec![];
    for &name in &def_names {
        b.u32(name as u32);
        b.put(&[GLOBAL_FUNC, 0]);
        b.u16(1);
        def_values.push(b.put(&[0; 8]));
        b.u64(0);
    }

    // SysV hash with a single bucket chaining every symbol: lookups into this
    // object stay correct whatever the hash function says.
    let hash_off = b.align();
    b.u32(1); // nbucket
    b.u32(nsyms as u32); // nchain
    b.u32((nsyms - 1) as u32); // bucket[0]
    for i in 0..nsyms {
        b.u32(i.saturating_sub(1) as u32); // chain[i] -> i-1, chain[0] = 0
    }

    let got_off = b.align();
    b.put(&vec![0; imports.len() * 8]);

    let rela_off = b.align();
    for i in 0..imports.len() {
        b.u64((got_off + i * 8) as u64); // r_offset
        b.u64((((i + 1) as u64) << 32) | arch::R_GLOB_DAT as u64); // r_info
        b.u64(0); // r_addend
    }
    let relasz = b.0.len() - rela_off;

    let dynamic_off = b.align();
    let mut dynamic = vec![];
    dynamic.extend(needed_names.iter().map(|&n| (DT_NEEDED, n as u64)));
    dynamic.extend([
        (DT_HASH, hash_off as u64),
        (DT_STRTAB, strtab_off as u64),
        (DT_STRSZ, strsz as u64),
        (DT_SYMTAB, symtab_off as u64),
        (DT_SYMENT, 24),
        (DT_RELA, rela_off as u64),
        (DT_RELASZ, relasz as u64),
        (DT_RELAENT, 24),
        (DT_DEBUG, 0),
        (DT_NULL, 0),
    ]);
    let debug_slot = dynamic_off
        + dynamic
            .iter()
            .position(|&(tag, _)| tag == DT_DEBUG)
            .unwrap()
            * 16
        + 8;
    for (tag, val) in &dynamic {
        b.u64(*tag);
        b.u64(*val);
    }
    let dynamic_size = b.0.len() - dynamic_off;
    let total = b.0.len();

    let mut headers = Buf::default();
    headers.phdr(PT_PHDR, PF_R, 0, PHNUM * PHDR_SIZE, 8);
    headers.phdr(
        PT_INTERP,
        PF_R,
        interp_off,
        interp.to_bytes_with_nul().len(),
        1,
    );
    headers.phdr(PT_LOAD, PF_R | PF_W, 0, total, page as u64);
    headers.phdr(PT_DYNAMIC, PF_R | PF_W, dynamic_off, dynamic_size, 8);
    headers.phdr(PT_GNU_STACK, PF_R | PF_W, 0, 0, 16);
    b.0[..headers.0.len()].copy_from_slice(&headers.0);

    // Writable: ld.so fills the GOT and DT_DEBUG.
    let len = total.next_multiple_of(page);
    // SAFETY: a fresh private mapping.
    let start = unsafe {
        sys::mmap(
            0,
            len,
            sys::PROT_READ | sys::PROT_WRITE,
            sys::MAP_PRIVATE | sys::MAP_ANONYMOUS,
            -1,
            0,
        )
    }?;
    for (&at, &(_, addr)) in def_values.iter().zip(defs) {
        b.0[at..at + 8].copy_from_slice(&(addr.wrapping_sub(start) as u64).to_le_bytes());
    }
    // SAFETY: `total <= len`, freshly mapped.
    unsafe { std::ptr::copy_nonoverlapping(b.0.as_ptr(), start as *mut u8, total) };
    Ok(Stub {
        region: (start, len),
        phdr: start,
        phnum: PHNUM,
        got: start + got_off,
        debug_slot: start + debug_slot,
    })
}
