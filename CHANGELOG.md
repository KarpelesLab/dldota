# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/KarpelesLab/dldota/compare/v0.1.0...v0.2.0) - 2026-10-28

### Added

- [**breaking**] mark Error as non_exhaustive
- unload, with foreign thread tracking and exit handlers
- per-context foreign heap, interposed as the malloc family
- boot the system ld.so and call through thread-pointer trampolines (x86_64)
- per-context allocator and full unload (design, API, tests)
- callback API, with tests for host callbacks from foreign code

### Fixed

- abort if anything unwinds while holding a RawLock
- restore UnwindSafe/RefUnwindSafe on Context and Library
- keep musl's loader allocator off the program break

### Other

- wait for joined threads to leave /proc/self/task
- cargo fmt
- enable unload refusal test; document what unload can't reclaim
- log anonymous mappings ld.so creates during boot and that survive unload
- enable foreign heap test on x86_64; fix zstd nbWorkers check
- enable host callback tests on x86_64
- enable zlib, zstd and concurrent-call tests on x86_64
- foreign worker threads (zstd) and concurrent host calls (zlib)
- run tests from a libc-free fullrust static host in CI

## [0.1.0](https://github.com/KarpelesLab/dldota/releases/tag/v0.1.0) - 2026-10-26

### Added

- Initial scaffolding: `Context`/`Library`/`Symbol` Rust API, C ABI
  (`include/dldota.h`, `libdldota.a`), system dynamic loader discovery, and
  design notes (`docs/design.md`). The loader bootstrap itself is not
  implemented yet.
