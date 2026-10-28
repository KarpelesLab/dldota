//! Minimal ELF64 parsing for the images we map ourselves (the interpreter).

use std::ffi::CStr;
use std::fs::File;
use std::os::fd::AsRawFd;

use crate::error::{Error, Result};
use crate::sys;

pub(crate) const PT_LOAD: u32 = 1;
pub(crate) const PT_DYNAMIC: u32 = 2;
pub(crate) const PT_INTERP: u32 = 3;
pub(crate) const PT_PHDR: u32 = 6;
pub(crate) const PT_GNU_STACK: u32 = 0x6474_e551;
pub(crate) const PF_X: u32 = 1;
pub(crate) const PF_W: u32 = 2;
pub(crate) const PF_R: u32 = 4;
pub(crate) const PHDR_SIZE: usize = 56;
const ET_DYN: u16 = 3;

#[cfg(target_arch = "x86_64")]
pub(crate) const EM_HOST: u16 = 62; // EM_X86_64
#[cfg(target_arch = "aarch64")]
pub(crate) const EM_HOST: u16 = 183; // EM_AARCH64

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// A program header, as much of it as we need.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Phdr {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
}

/// Validate the ELF header (64-bit, little-endian, host machine) and return
/// the program headers.
pub(crate) fn phdrs(image: &[u8]) -> Result<Vec<Phdr>> {
    const BAD: Error = Error::InvalidElf("truncated header");
    if image.get(..4) != Some(b"\x7fELF") {
        return Err(Error::InvalidElf("bad magic"));
    }
    if image.get(4) != Some(&2) || image.get(5) != Some(&1) {
        return Err(Error::InvalidElf("not ELF64 little-endian"));
    }
    if u16_at(image, 18).ok_or(BAD)? != EM_HOST {
        return Err(Error::InvalidElf("wrong machine"));
    }
    let phoff = u64_at(image, 32).ok_or(BAD)? as usize;
    let phentsize = u16_at(image, 54).ok_or(BAD)? as usize;
    let phnum = u16_at(image, 56).ok_or(BAD)? as usize;
    if phentsize < 56 {
        return Err(Error::InvalidElf("bad e_phentsize"));
    }
    (0..phnum)
        .map(|i| {
            let o = phoff + i * phentsize;
            Some(Phdr {
                p_type: u32_at(image, o)?,
                p_flags: u32_at(image, o + 4)?,
                p_offset: u64_at(image, o + 8)?,
                p_vaddr: u64_at(image, o + 16)?,
                p_filesz: u64_at(image, o + 32)?,
                p_memsz: u64_at(image, o + 40)?,
            })
        })
        .collect::<Option<_>>()
        .ok_or(Error::InvalidElf("truncated program headers"))
}

/// The `PT_INTERP` path of `image`, if it has one.
pub(crate) fn interp(image: &[u8]) -> Result<Option<&str>> {
    let Some(ph) = phdrs(image)?.into_iter().find(|p| p.p_type == PT_INTERP) else {
        return Ok(None);
    };
    let start = ph.p_offset as usize;
    let bytes = image
        .get(start..start + ph.p_filesz as usize)
        .ok_or(Error::InvalidElf("PT_INTERP out of bounds"))?;
    let path = CStr::from_bytes_until_nul(bytes)
        .ok()
        .and_then(|s| s.to_str().ok())
        .ok_or(Error::InvalidElf("bad PT_INTERP"))?;
    Ok(Some(path))
}

/// An ELF image mapped into memory.
pub(crate) struct Mapped {
    /// The reservation covering every segment: `(start, len)`.
    pub region: (usize, usize),
    /// Runtime address of the entry point.
    pub entry: usize,
}

fn round_down(v: usize, page: usize) -> usize {
    v & !(page - 1)
}

fn round_up(v: usize, page: usize) -> usize {
    (v + page - 1) & !(page - 1)
}

fn prot(flags: u32) -> usize {
    let mut prot = sys::PROT_NONE;
    if flags & PF_R != 0 {
        prot |= sys::PROT_READ;
    }
    if flags & PF_W != 0 {
        prot |= sys::PROT_WRITE;
    }
    if flags & PF_X != 0 {
        prot |= sys::PROT_EXEC;
    }
    prot
}

/// Map a position-independent image (the dynamic loader) the way the kernel
/// maps an interpreter: reserve its whole span, then map each `PT_LOAD` from
/// the file and zero-fill its bss.
pub(crate) fn map_image(file: &File, image: &[u8], page: usize) -> Result<Mapped> {
    const BAD: Error = Error::InvalidElf("truncated header");
    if u16_at(image, 16).ok_or(BAD)? != ET_DYN {
        return Err(Error::InvalidElf("interpreter is not ET_DYN"));
    }
    let e_entry = u64_at(image, 24).ok_or(BAD)? as usize;
    let loads: Vec<Phdr> = phdrs(image)?
        .into_iter()
        .filter(|p| p.p_type == PT_LOAD)
        .collect();
    let lo = loads
        .iter()
        .map(|p| round_down(p.p_vaddr as usize, page))
        .min()
        .ok_or(Error::InvalidElf("no PT_LOAD"))?;
    let hi = loads
        .iter()
        .map(|p| round_up((p.p_vaddr + p.p_memsz) as usize, page))
        .max()
        .unwrap_or(lo);
    let span = hi - lo;
    // SAFETY: a fresh reservation; nothing else lives there.
    let start = unsafe {
        sys::mmap(
            0,
            span,
            sys::PROT_NONE,
            sys::MAP_PRIVATE | sys::MAP_ANONYMOUS,
            -1,
            0,
        )
    }?;
    let bias = start.wrapping_sub(lo);

    let mapped = (|| -> Result<()> {
        for ph in &loads {
            if ph.p_vaddr % page as u64 != ph.p_offset % page as u64 {
                return Err(Error::InvalidElf("misaligned PT_LOAD"));
            }
            let seg = bias + ph.p_vaddr as usize;
            let map_start = round_down(seg, page);
            let file_end = seg + ph.p_filesz as usize;
            let mem_end = round_up(seg + ph.p_memsz as usize, page);
            let prot = prot(ph.p_flags);
            if ph.p_filesz > 0 {
                let len = round_up(file_end, page) - map_start;
                let off = round_down(ph.p_offset as usize, page);
                // SAFETY: inside our own reservation.
                unsafe {
                    sys::mmap(
                        map_start,
                        len,
                        prot,
                        sys::MAP_PRIVATE | sys::MAP_FIXED,
                        file.as_raw_fd(),
                        off,
                    )
                }?;
            }
            if ph.p_memsz > ph.p_filesz {
                // Zero the tail of the last file-backed page, then map the rest
                // of the bss anonymously.
                let page_end = round_up(file_end, page);
                if ph.p_filesz > 0 && page_end > file_end {
                    let writable = prot & sys::PROT_WRITE != 0;
                    // SAFETY: the page was just mapped by us.
                    unsafe {
                        if !writable {
                            sys::mprotect(
                                round_down(file_end, page),
                                page,
                                prot | sys::PROT_WRITE,
                            )?;
                        }
                        std::ptr::write_bytes(file_end as *mut u8, 0, page_end - file_end);
                        if !writable {
                            sys::mprotect(round_down(file_end, page), page, prot)?;
                        }
                    }
                }
                let anon_start = if ph.p_filesz > 0 { page_end } else { map_start };
                if mem_end > anon_start {
                    // SAFETY: inside our own reservation.
                    unsafe {
                        sys::mmap(
                            anon_start,
                            mem_end - anon_start,
                            prot,
                            sys::MAP_PRIVATE | sys::MAP_FIXED | sys::MAP_ANONYMOUS,
                            -1,
                            0,
                        )
                    }?;
                }
            }
        }
        Ok(())
    })();
    if let Err(e) = mapped {
        // SAFETY: the reservation is ours and nothing references it yet.
        let _ = unsafe { sys::munmap(start, span) };
        return Err(e);
    }
    Ok(Mapped {
        region: (start, span),
        entry: bias + e_entry,
    })
}
