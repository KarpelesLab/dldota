//! Executable thunks: one per foreign function handed to the host (outbound)
//! and per host function handed to foreign code (callbacks). Each loads its
//! target and the context's gate, then jumps to the shared trampoline.
//!
//! Code is written through a writable view of a memfd and executed from a
//! separate read+exec view, so no page is ever writable and executable at
//! once. Where memfd mappings aren't allowed to be executable, falls back to
//! an anonymous RWX mapping.

use std::collections::HashMap;
use std::io;

use crate::arch;
use crate::gate::Gate;
use crate::sys;

const CHUNK: usize = 64 << 10;

struct Chunk {
    rw: usize,
    rx: usize,
}

#[derive(Default)]
pub(crate) struct Thunks {
    chunks: Vec<Chunk>,
    used: usize,
    cache: HashMap<(usize, usize), usize>,
}

impl Thunks {
    /// A host-callable address for the foreign function `target`.
    pub(crate) fn outbound(&mut self, gate: &Gate, target: usize) -> io::Result<usize> {
        self.get(gate, target, arch::tramp_out_address())
    }

    /// A foreign-callable address for the host function `target`.
    pub(crate) fn inbound(&mut self, gate: &Gate, target: usize) -> io::Result<usize> {
        self.get(gate, target, arch::tramp_in_address())
    }

    /// A C-callable address that calls `func(data, args...)`; see
    /// `arch::write_bind_thunk`.
    pub(crate) fn bound(&mut self, func: usize, data: usize) -> io::Result<usize> {
        let (rw, rx) = self.slot()?;
        // SAFETY: a fresh slot in the writable view.
        unsafe { arch::write_bind_thunk(rw as *mut u8, func, data) };
        Ok(rx)
    }

    /// The mappings backing the thunks, as `(start, len)`.
    #[allow(dead_code)] // used by unload
    pub(crate) fn regions(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.chunks
            .iter()
            .flat_map(|c| [(c.rw, CHUNK), (c.rx, CHUNK)])
            .filter(|r| r.0 != 0)
    }

    fn get(&mut self, gate: &Gate, target: usize, tramp: usize) -> io::Result<usize> {
        if let Some(&thunk) = self.cache.get(&(target, tramp)) {
            return Ok(thunk);
        }
        let (rw, rx) = self.slot()?;
        // SAFETY: a fresh slot in the writable view.
        unsafe { arch::write_thunk(rw as *mut u8, target, gate as *const Gate as usize, tramp) };
        self.cache.insert((target, tramp), rx);
        Ok(rx)
    }

    /// A fresh slot: (writable address, executable address).
    fn slot(&mut self) -> io::Result<(usize, usize)> {
        if self.chunks.is_empty() || self.used + arch::THUNK_SIZE > CHUNK {
            self.chunks.push(new_chunk()?);
            self.used = 0;
        }
        let chunk = self.chunks.last().unwrap();
        let slot = (chunk.rw + self.used, chunk.rx + self.used);
        self.used += arch::THUNK_SIZE;
        Ok(slot)
    }
}

fn new_chunk() -> io::Result<Chunk> {
    if let Ok(chunk) = memfd_chunk() {
        return Ok(chunk);
    }
    // SAFETY: a fresh private mapping.
    let rwx = unsafe {
        sys::mmap(
            0,
            CHUNK,
            sys::PROT_READ | sys::PROT_WRITE | sys::PROT_EXEC,
            sys::MAP_PRIVATE | sys::MAP_ANONYMOUS,
            -1,
            0,
        )
    }?;
    Ok(Chunk { rw: rwx, rx: rwx })
}

fn memfd_chunk() -> io::Result<Chunk> {
    let fd = sys::memfd_create(c"dldota-thunks")?;
    let views = (|| {
        sys::ftruncate(fd, CHUNK)?;
        // SAFETY: fresh shared mappings of our own memfd.
        let rw = unsafe {
            sys::mmap(
                0,
                CHUNK,
                sys::PROT_READ | sys::PROT_WRITE,
                sys::MAP_SHARED,
                fd,
                0,
            )
        }?;
        // SAFETY: as above.
        match unsafe {
            sys::mmap(
                0,
                CHUNK,
                sys::PROT_READ | sys::PROT_EXEC,
                sys::MAP_SHARED,
                fd,
                0,
            )
        } {
            Ok(rx) => Ok(Chunk { rw, rx }),
            Err(e) => {
                // SAFETY: just mapped, unused.
                let _ = unsafe { sys::munmap(rw, CHUNK) };
                Err(e)
            }
        }
    })();
    let _ = sys::close(fd);
    views
}
