# dldota

Load the system's shared libraries from a **statically linked** program.

dldota maps the system's own dynamic loader (`ld-linux-*.so`, `ld-musl-*.so`)
into the process, boots it as the kernel would, and exposes its
`dlopen`/`dlsym`. Calls into loaded code go through trampolines that switch the
thread pointer, so the host runtime and the system libc can coexist.

Status: scaffolding. See [docs/design.md](docs/design.md).

- Rust: `dldota::Context`
- C: `include/dldota.h` + `libdldota.a` (build with
  `cargo build --release --target x86_64-unknown-linux-musl`, or the `-gnu`
  target when linking into a static glibc program)

Supported: Linux on x86_64 and aarch64. MSRV 1.95.
