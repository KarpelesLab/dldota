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
#[allow(dead_code)] // regions and rtld_fini are for unload
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

/// Add `glibc.pthread.rseq=0` to `GLIBC_TUNABLES`: glibc's rseq area lives in
/// its TCB, and the kernel would keep writing to it after unload.
fn tune_env(env: &mut Vec<CString>) {
    const RSEQ_OFF: &str = "glibc.pthread.rseq=0";
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

#[cfg(test)]
mod tests {
    #[test]
    fn finds_system_interpreter() {
        let path = super::find_interpreter().expect("no interpreter on this system");
        assert!(path.is_absolute());
    }
}
