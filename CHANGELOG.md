# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/KarpelesLab/dldota/releases/tag/v0.1.0) - 2026-10-26

### Added

- Initial scaffolding: `Context`/`Library`/`Symbol` Rust API, C ABI
  (`include/dldota.h`, `libdldota.a`), system dynamic loader discovery, and
  design notes (`docs/design.md`). The loader bootstrap itself is not
  implemented yet.
