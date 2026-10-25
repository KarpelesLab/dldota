//! Minimal ELF64 parsing for the images we map ourselves (the interpreter).

use std::ffi::CStr;

use crate::error::{Error, Result};

pub(crate) const PT_INTERP: u32 = 3;

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
#[allow(dead_code)] // the rest is used once the interpreter mapper lands
#[derive(Clone, Copy, Debug)]
pub(crate) struct Phdr {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
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
                p_align: u64_at(image, o + 48)?,
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
