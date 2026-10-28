//! Unloading a context: run the foreign world's destructors, then unmap it.

use std::ffi::c_void;

use crate::context::Inner;
use crate::error::{Error, Result};
use crate::loader::{self, Flavor, LoadedObject};
use crate::sys;

const DT_NULL: usize = 0;
const DT_FINI: usize = 13;
const DT_FINI_ARRAY: usize = 26;
const DT_FINI_ARRAYSZ: usize = 28;

impl Inner {
    /// # Safety
    /// No `Library`, `Symbol` or `Callback` of this context may be used
    /// afterwards, on success.
    pub(crate) unsafe fn unload(&self) -> Result<()> {
        self.gate.acquire();
        // SAFETY: forwarded; the gate keeps host threads out of foreign code.
        let r = unsafe { self.teardown() };
        self.gate.release();
        r
    }

    /// Call the foreign function `f(arg)` from the host.
    fn call_foreign(&self, f: usize, arg: usize) -> Result<()> {
        let thunk = self
            .thunks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .outbound(self.gate, f)?;
        // SAFETY: a foreign `void (*)(void *)` (or `void (*)(void)`, which
        // ignores the argument), through a thunk that switches the TP.
        unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(usize)>(thunk)(arg) };
        Ok(())
    }

    unsafe fn teardown(&self) -> Result<()> {
        let live = self.rt.live_threads();
        if live > 0 {
            return Err(Error::Busy(format!(
                "{live} thread(s) created by foreign code still running"
            )));
        }
        let booted = &self.booted;
        debug!("unloading");

        if let Some(fflush) = self.fflush {
            // SAFETY: fflush(NULL) flushes every foreign stream.
            unsafe { fflush(std::ptr::null_mut::<c_void>()) };
        }
        while let Some((func, arg)) = self.rt.pop_exit(None) {
            self.call_foreign(func, arg)?;
        }

        // SAFETY: still loaded; finalizers below don't unmap anything.
        let objects = unsafe { loader::loaded_objects(booted) };
        match booted.flavor {
            // ld.so's _dl_fini runs every object's finalizers in dependency order.
            Flavor::Glibc if booted.rtld_fini != 0 => self.call_foreign(booted.rtld_fini, 0)?,
            Flavor::Glibc => {}
            // musl only runs finalizers from exit(): run them ourselves.
            Flavor::Musl => {
                for obj in objects.iter().rev() {
                    // SAFETY: the object is still mapped and was initialized.
                    unsafe { self.run_finalizers(obj)? };
                }
            }
        }

        let spans: Vec<(usize, usize)> = objects
            .iter()
            .filter_map(|o| o.span)
            .chain(booted.regions.iter().copied())
            .collect();
        restore_signals(&booted.signals, &spans);

        // SAFETY: nothing runs foreign code anymore: no live foreign threads,
        // the gate is held, and the Context is being consumed.
        unsafe {
            for &(start, len) in &spans {
                let _ = sys::munmap(start, len);
            }
            let thunks = self.thunks.lock().unwrap_or_else(|e| e.into_inner());
            for (start, len) in thunks.regions() {
                let _ = sys::munmap(start, len);
            }
        }
        self.rt.heap.release_all();
        debug!("unloaded {} objects", objects.len());
        if crate::debug_enabled() {
            let now = loader::anon_mappings();
            for &(start, end) in &booted.boot_anon {
                if now.iter().any(|&(s, e)| s < end && start < e) {
                    debug!(
                        "still mapped after unload: {start:#x}-{end:#x} ({} KiB)",
                        (end - start) / 1024
                    );
                }
            }
        }
        Ok(())
    }

    /// Run an object's `DT_FINI_ARRAY` (in reverse) and `DT_FINI`.
    unsafe fn run_finalizers(&self, obj: &LoadedObject) -> Result<()> {
        let (mut fini, mut array, mut array_size) = (0, 0, 0);
        let mut dynamic = obj.dynamic as *const usize;
        // SAFETY: the object's dynamic section, terminated by DT_NULL.
        unsafe {
            while *dynamic != DT_NULL {
                match *dynamic {
                    DT_FINI => fini = *dynamic.add(1),
                    DT_FINI_ARRAY => array = *dynamic.add(1),
                    DT_FINI_ARRAYSZ => array_size = *dynamic.add(1),
                    _ => {}
                }
                dynamic = dynamic.add(2);
            }
        }
        if array != 0 {
            let entries = (obj.base + array) as *const usize;
            for i in (0..array_size / size_of::<usize>()).rev() {
                // SAFETY: within the relocated fini array.
                let f = unsafe { *entries.add(i) };
                if f != 0 && f != usize::MAX {
                    self.call_foreign(f, 0)?;
                }
            }
        }
        if fini != 0 {
            self.call_foreign(obj.base + fini, 0)?;
        }
        Ok(())
    }
}

/// Put back the pre-boot disposition of every signal whose handler lies in
/// memory about to be unmapped.
fn restore_signals(saved: &[Option<sys::KSigaction>], spans: &[(usize, usize)]) {
    for (sig, old) in saved.iter().enumerate() {
        let (Some(old), Some(cur)) = (old, sys::sigaction_get(sig)) else {
            continue;
        };
        if spans
            .iter()
            .any(|&(start, len)| (start..start + len).contains(&cur.handler))
        {
            debug!("restoring handler of signal {sig}");
            // SAFETY: reinstating what the process had before boot.
            unsafe { sys::sigaction_set(sig, old) };
        }
    }
}
