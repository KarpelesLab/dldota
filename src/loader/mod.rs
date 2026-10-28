//! Mapping and booting the system dynamic loader.

mod elf;
mod stack;
mod stub;

use std::ffi::{CStr, CString};
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicPtr;

use crate::arch;
use crate::error::{Error, Result};
use crate::sys;

/// Symbols the stub imports; the GOT holds them in this order.
const IMPORTS: &[&CStr] = &[
    c"__libc_start_main",
    c"dlopen",
    c"dlsym",
    c"dlclose",
    c"dlerror",
    c"__errno_location",
];
/// `libdl.so.2` is an empty stub on glibc ≥ 2.34 and provides `dl*` before
/// that. musl maps both names to itself.
const NEEDED: &[&CStr] = &[c"libc.so.6", c"libdl.so.2"];

/// Shared with the arch boot code: field offsets are fixed.
#[repr(C)]
#[derive(Default)]
pub(crate) struct BootState {
    /// Host stack pointer after saving callee-saved registers.
    host_sp: usize,
    /// Host thread pointer to restore.
    host_tp: usize,
    /// GOT slot holding `__libc_start_main`.
    start_main_slot: usize,
    /// Out: the foreign thread pointer of the bootstrap TCB.
    foreign_tp: usize,
    /// Out: the fini function ld.so passed to the program entry.
    rtld_fini: usize,
}

/// The `BootState` of the boot in progress, for the entry code.
pub(crate) static BOOT_STATE: AtomicPtr<BootState> = AtomicPtr::new(std::ptr::null_mut());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flavor {
    Glibc,
    Musl,
}

/// A booted foreign loader.
pub(crate) struct Booted {
    pub flavor: Flavor,
    pub foreign_tp: usize,
    pub rtld_fini: usize,
    pub dlopen: usize,
    pub dlsym: usize,
    pub dlclose: usize,
    pub dlerror: usize,
    pub errno_location: usize,
    /// Everything we mapped: ld.so, the stub, the boot stack.
    pub regions: Vec<(usize, usize)>,
    /// Load bias of ld.so and of the stub (the main program).
    pub ld_base: usize,
    pub stub_base: usize,
    /// Where ld.so stores `&r_debug` (the head of its object list).
    pub debug_slot: usize,
    /// Signal dispositions before boot, indexed by signal number.
    pub signals: Vec<Option<sys::KSigaction>>,
}

/// The host page size.
pub(crate) fn page_size() -> usize {
    static PAGE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *PAGE.get_or_init(|| {
        stack::host_auxv()
            .iter()
            .find(|&&(k, _)| k == stack::AT_PAGESZ)
            .map_or(4096, |&(_, v)| v)
    })
}

/// Add our settings to `GLIBC_TUNABLES`, so a context can be unloaded:
/// - `rseq=0`: glibc's rseq area lives in its TCB, and the kernel would keep
///   writing to it after unload;
/// - `stack_cache_size=0`: glibc unmaps thread stacks when threads are joined
///   instead of caching them where unload can't find them.
fn tune_env(env: &mut Vec<CString>) {
    const RSEQ_OFF: &str = "glibc.pthread.rseq=0:glibc.pthread.stack_cache_size=0";
    match env
        .iter_mut()
        .find(|e| e.as_bytes().starts_with(b"GLIBC_TUNABLES="))
    {
        Some(e) => {
            let mut v = e.as_bytes().to_vec();
            if v.len() > b"GLIBC_TUNABLES=".len() {
                v.push(b':');
            }
            v.extend_from_slice(RSEQ_OFF.as_bytes());
            *e = CString::new(v).expect("no NUL in tunables");
        }
        None => env.push(CString::new(format!("GLIBC_TUNABLES={RSEQ_OFF}")).unwrap()),
    }
}

/// Map the interpreter and the stub, then run ld.so until it reaches our entry.
/// `defs` are the symbols the stub defines (interposes), as absolute addresses.
pub(crate) fn boot(
    interpreter: &Path,
    mut env: Vec<CString>,
    defs: &[(&CStr, usize)],
) -> Result<Booted> {
    if !arch::BOOT_SUPPORTED {
        return Err(Error::Unimplemented("booting on this architecture"));
    }
    // One boot at a time: the entry code finds its state through a static.
    static BOOT_LOCK: Mutex<()> = Mutex::new(());
    let _boot = BOOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let host_aux = stack::host_auxv();
    let page = page_size();
    let hwcap2 = host_aux
        .iter()
        .find(|&&(k, _)| k == stack::AT_HWCAP2)
        .map_or(0, |&(_, v)| v);
    arch::init(hwcap2);

    let flavor = match interpreter.file_name().map(|n| n.as_bytes()) {
        Some(n) if n.windows(4).any(|w| w == b"musl") => Flavor::Musl,
        _ => Flavor::Glibc,
    };
    if flavor == Flavor::Glibc {
        tune_env(&mut env);
    }
    debug!(
        "booting {} ({flavor:?}), page size {page}",
        interpreter.display()
    );

    let file = File::open(interpreter)?;
    let image = std::fs::read(interpreter)?;
    let ld = elf::map_image(&file, &image, page)?;
    drop(file);
    debug!("ld.so mapped at {:#x}, entry {:#x}", ld.region.0, ld.entry);

    let interp =
        CString::new(interpreter.as_os_str().as_bytes()).map_err(|_| Error::InterpreterNotFound)?;
    let stub = stub::build(page, &interp, NEEDED, IMPORTS, defs)?;
    debug!("stub at {:#x}, GOT at {:#x}", stub.region.0, stub.got);

    let argv0 = std::env::args_os()
        .next()
        .and_then(|a| CString::new(a.as_bytes()).ok())
        .unwrap_or_else(|| c"dldota".to_owned());
    let mut aux: Vec<(usize, usize)> = host_aux
        .iter()
        .copied()
        .filter(|(k, _)| stack::PASSTHROUGH.contains(k))
        .collect();
    aux.extend([
        (stack::AT_PHDR, stub.phdr),
        (stack::AT_PHENT, elf::PHDR_SIZE),
        (stack::AT_PHNUM, stub.phnum),
        (stack::AT_BASE, ld.region.0),
        (stack::AT_FLAGS, 0),
        (stack::AT_ENTRY, arch::entry_address()),
    ]);
    let boot_stack = stack::build(page, &argv0, &env, &aux)?;

    // Process state that libc initialization changes for the calling thread.
    let tid_address = sys::get_tid_address();
    let robust_list = sys::get_robust_list();
    let dispositions = (0..=64)
        .map(|sig| {
            if sig == 0 {
                None
            } else {
                sys::sigaction_get(sig)
            }
        })
        .collect();
    let signals = sys::sigprocmask_set(!0)?;

    let mut state = BootState {
        host_tp: arch::thread_pointer(),
        start_main_slot: stub.got,
        ..BootState::default()
    };
    BOOT_STATE.store(&mut state, std::sync::atomic::Ordering::SeqCst);
    debug!("entering ld.so");
    // SAFETY: ld.so is mapped, the stack built for it, and BOOT_STATE set.
    unsafe { arch::boot(&mut state, ld.entry, boot_stack.sp) };
    BOOT_STATE.store(std::ptr::null_mut(), std::sync::atomic::Ordering::SeqCst);

    // SAFETY: restoring exactly what this thread had before.
    unsafe {
        if let Some(addr) = tid_address {
            sys::set_tid_address(addr);
        }
        if let Some((head, len)) = robust_list {
            sys::set_robust_list(head, len);
        }
    }
    sys::sigprocmask_set(signals)?;
    debug!(
        "back from ld.so: foreign TP {:#x}, rtld_fini {:#x}",
        state.foreign_tp, state.rtld_fini
    );

    // SAFETY: the GOT holds one resolved pointer per import.
    let got = |i: usize| unsafe { (stub.got as *const usize).add(i).read() };
    let names = IMPORTS.iter().map(|n| n.to_string_lossy());
    if let Some((name, _)) = names.zip(0..).find(|&(_, i)| got(i) == 0) {
        return Err(Error::Dl(format!(
            "{name} not resolved by the system loader"
        )));
    }
    Ok(Booted {
        flavor,
        foreign_tp: state.foreign_tp,
        rtld_fini: state.rtld_fini,
        dlopen: got(1),
        dlsym: got(2),
        dlclose: got(3),
        dlerror: got(4),
        errno_location: got(5),
        regions: vec![ld.region, stub.region, boot_stack.region],
        ld_base: ld.region.0,
        stub_base: stub.region.0,
        debug_slot: stub.debug_slot,
        signals: dispositions,
    })
}

/// Binaries whose `PT_INTERP` names the system's dynamic loader.
const PROBE_BINARIES: &[&str] = &["/bin/sh", "/usr/bin/env", "/bin/ls"];

#[cfg(target_arch = "x86_64")]
const WELL_KNOWN: &[&str] = &[
    "/lib64/ld-linux-x86-64.so.2",
    "/lib/ld-linux-x86-64.so.2",
    "/lib/ld-musl-x86_64.so.1",
];
#[cfg(target_arch = "aarch64")]
const WELL_KNOWN: &[&str] = &["/lib/ld-linux-aarch64.so.1", "/lib/ld-musl-aarch64.so.1"];

/// Locate the system dynamic loader: `PT_INTERP` of a system binary of our
/// architecture first, then well-known paths.
pub(crate) fn find_interpreter() -> Result<PathBuf> {
    for probe in PROBE_BINARIES {
        let Ok(image) = std::fs::read(probe) else {
            continue;
        };
        if let Ok(Some(interp)) = elf::interp(&image) {
            let path = PathBuf::from(interp);
            if path.exists() {
                return Ok(path);
            }
        }
    }
    WELL_KNOWN
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .ok_or(Error::InterpreterNotFound)
}

/// An object in the foreign loader's list (`r_debug.r_map`).
pub(crate) struct LoadedObject {
    /// Load bias (`l_addr`).
    pub base: usize,
    /// Its dynamic section (`l_ld`).
    pub dynamic: usize,
    /// Mapped span of its PT_LOAD segments: `(start, len)`.
    pub span: Option<(usize, usize)>,
}

/// The objects the foreign loader has mapped, in load order, except ld.so,
/// the stub and the vDSO, which are not ours to unmap.
///
/// # Safety
/// The context must still be loaded.
pub(crate) unsafe fn loaded_objects(booted: &Booted) -> Vec<LoadedObject> {
    let vdso = stack::host_auxv()
        .iter()
        .find(|&&(k, _)| k == 33) // AT_SYSINFO_EHDR
        .map_or(0, |&(_, v)| v);
    let mut out = vec![];
    // SAFETY: ld.so stored &r_debug in the stub's DT_DEBUG; glibc's r_debug
    // and musl's `debug` share the layout {int version; link_map *map; ...},
    // and link_map/dso both start {base, name, dynamic, next, prev}.
    unsafe {
        let r_debug = *(booted.debug_slot as *const usize);
        if r_debug == 0 {
            debug!("no r_debug: cannot enumerate loaded objects");
            return out;
        }
        let mut map = *((r_debug + 8) as *const usize);
        while map != 0 {
            let words = map as *const usize;
            let (base, name, dynamic, next) = (*words, *words.add(1), *words.add(2), *words.add(3));
            let is_vdso = (vdso != 0 && base == vdso)
                || (name != 0
                    && CStr::from_ptr(name as *const _)
                        .to_bytes()
                        .windows(4)
                        .any(|w| w == b"vdso"));
            if base != booted.ld_base && base != booted.stub_base && !is_vdso {
                out.push(LoadedObject {
                    base,
                    dynamic,
                    span: mapped_span(base),
                });
            }
            map = next;
        }
    }
    out
}

/// PT_LOAD span of an object mapped at load bias `base`, from its in-memory
/// ELF header (mapped at `base` for objects linked at address 0).
unsafe fn mapped_span(base: usize) -> Option<(usize, usize)> {
    // SAFETY: the caller passes a mapped object; its first page is readable.
    let header = unsafe { std::slice::from_raw_parts(base as *const u8, 64) };
    if header[..4] != *b"\x7fELF" {
        return None;
    }
    let phoff = u64::from_le_bytes(header[32..40].try_into().ok()?) as usize;
    let phentsize = u16::from_le_bytes(header[54..56].try_into().ok()?) as usize;
    let phnum = u16::from_le_bytes(header[56..58].try_into().ok()?) as usize;
    // SAFETY: program headers live in the first mapped page(s).
    let image = unsafe { std::slice::from_raw_parts(base as *const u8, phoff + phnum * phentsize) };
    let page = page_size();
    let loads = elf::phdrs(image)
        .ok()?
        .into_iter()
        .filter(|p| p.p_type == elf::PT_LOAD);
    let (mut lo, mut hi) = (usize::MAX, 0);
    for p in loads {
        lo = lo.min(p.p_vaddr as usize & !(page - 1));
        hi = hi.max((p.p_vaddr + p.p_memsz) as usize);
    }
    (lo < hi).then(|| (base + lo, hi.next_multiple_of(page) - lo))
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_system_interpreter() {
        let path = super::find_interpreter().expect("no interpreter on this system");
        assert!(path.is_absolute());
    }
}
