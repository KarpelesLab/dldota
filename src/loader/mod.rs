//! Mapping and booting the system dynamic loader.

mod elf;
mod stack;
mod stub;

use std::path::PathBuf;

use crate::error::{Error, Result};

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
