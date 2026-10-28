# dldota

[![CI](https://github.com/KarpelesLab/dldota/actions/workflows/ci.yml/badge.svg)](https://github.com/KarpelesLab/dldota/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/dldota.svg)](https://crates.io/crates/dldota)
[![docs.rs](https://img.shields.io/docsrs/dldota)](https://docs.rs/dldota)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Load the system's shared libraries from a **statically linked** program.

dldota maps the system's own dynamic loader (`ld-linux-*.so`, `ld-musl-*.so`)
into the process, boots it as the kernel would, and exposes its
`dlopen`/`dlsym`. Calls into loaded code go through trampolines that switch the
thread pointer, so the host runtime and the system libc can coexist.

Status: working on x86_64 (glibc and musl systems): boot, `dlopen`/`dlsym`,
calls and callbacks with thread-pointer switching, a per-context heap that
serves every foreign allocation, and unload. aarch64 is not implemented yet.
See [docs/design.md](docs/design.md).

- Rust: `dldota::Context`
- C: `include/dldota.h` + `libdldota.a` (build with
  `cargo build --release --target x86_64-unknown-linux-musl`, or the `-gnu`
  target when linking into a static glibc program)

Tests also run from a fully static, libc-free host built with
[fullrust](https://github.com/KarpelesLab/fullrust), against both glibc and
musl system loaders: `scripts/fullrust-tests.sh`.

Supported: Linux on x86_64 and aarch64. MSRV 1.95.
