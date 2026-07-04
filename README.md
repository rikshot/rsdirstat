# rsdirstat

[![CI](https://github.com/rikshot/rsdirstat/actions/workflows/ci.yml/badge.svg)](https://github.com/rikshot/rsdirstat/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/rikshot/rsdirstat/branch/main/graph/badge.svg)](https://codecov.io/gh/rikshot/rsdirstat)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-informational)

Fast, cross-platform disk usage analyzer with an interactive treemap GUI.

## Features

- **Multi-threaded scanning** using platform-native APIs for maximum speed
  - macOS: `getattrlistbulk` for batch directory + metadata reads
  - Linux: `getdents64` + `statx` via raw syscalls
  - Windows: `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`
- **Interactive treemap GUI** served as a local web app over WebSocket
  - Squarified treemap layout with zoom navigation
  - Color by file type or modification time
  - Filter by extension, size, or name
  - Click to navigate, right-click to reveal in file manager
- **CLI mode** for quick top-N reports

## Installation

```sh
cargo install rsdirstat
```

This installs two binaries: **`rsdirstat`** (the GUI server, with the web frontend
embedded — a single self-contained binary, no extra files needed) and
**`rsdirstat-cli`** (the terminal top-N reporter). Building from source requires
[Rust](https://rustup.rs/) 1.88+ (edition 2024); see [Building](#building) to build
the frontend bundle from a checkout.

## Usage

### GUI

```sh
rsdirstat                 # no path: pick a volume in the browser, then scan
rsdirstat [path]          # scan a path, opens the treemap in your browser
rsdirstat --all [path]    # cross filesystem boundaries
rsdirstat --port 8080 [path]   # fixed port (default: random)
rsdirstat --no-open [path]     # don't auto-open the browser
```

### CLI

```sh
rsdirstat-cli [path]          # top 10 directories by size
rsdirstat-cli --files [path]  # top 10 files by size
rsdirstat-cli --top 20 [path] # show more results
rsdirstat-cli --all [path]    # cross filesystem boundaries
```

## Building

The browser frontend is a Rust/WASM app built with [trunk](https://trunkrs.dev), which
drives `wasm-bindgen` and `wasm-opt` for you. A release build **embeds** the bundle into the
`rsdirstat` binary (via `rust-embed`), so the result is a single self-contained executable —
no separate files to deploy.

```sh
rustup target add wasm32-unknown-unknown
cargo install trunk

# trunk runs from the project root (Trunk.toml lives there) and writes the bundle to
# crates/rsdirstat/dist (git-ignored). Build it first, then the release binary embeds it:
trunk build --release
cargo build --release
```

For frontend development, debug builds read the bundle from `crates/rsdirstat/dist` on disk
instead of embedding it, so run `trunk watch` (rebuilds on change) alongside
`cargo run -p rsdirstat` and just refresh the browser.

Only the native platform scanner is compiled. Cross-compilation:

```sh
# Linux (musl)
cargo build --release --target x86_64-unknown-linux-musl

# Windows (MSVC via cargo-xwin)
cargo xwin build --release --target x86_64-pc-windows-msvc
```

## Project Structure

```
crates/
  core/       Shared library: treemap layout, work queue
  protocol/   Shared wire protocol for server and wasm frontend
  rsdirstat/  App crate — `rsdirstat` (axum + WebSocket GUI) and `rsdirstat-cli` binaries
  wasm/       Browser frontend compiled to WebAssembly (embedded into rsdirstat)
  macos/      macOS scanner
  linux/      Linux scanner
  windows/    Windows scanner
```

## License

[MIT](LICENSE)
