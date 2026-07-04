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

Requires [Rust](https://rustup.rs/) (edition 2024).

The CLI is self-contained:

```sh
cargo install --path crates/cli
```

The GUI server additionally needs the WASM frontend bundle (see [Building](#building)).
It loads the bundle from `crates/wasm/dist` when run from a checkout, or from a `dist/`
directory next to the binary when deployed:

```sh
trunk build --release              # produces crates/wasm/dist
cargo install --path crates/server # then run alongside a dist/ directory
```

## Usage

### GUI

```sh
rsdirstat-server                 # no path: pick a volume in the browser, then scan
rsdirstat-server [path]          # scan a path, opens the treemap in your browser
rsdirstat-server --all [path]    # cross filesystem boundaries
rsdirstat-server --port 8080 [path]   # fixed port (default: random)
rsdirstat-server --no-open [path]     # don't auto-open the browser
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
drives `wasm-bindgen` and `wasm-opt` for you:

```sh
rustup target add wasm32-unknown-unknown
cargo install trunk

# All trunk commands run from the project root (Trunk.toml lives there).
# Build the frontend bundle (into crates/wasm/dist, git-ignored), then the server:
trunk build --release
cargo build --release
```

The server serves the bundle from `crates/wasm/dist` in a dev checkout, or from a `dist/`
directory next to the binary when deployed.

For frontend development, run `trunk watch` (rebuilds `dist/` on change) alongside
`cargo run -p rsdirstat-server`, and refresh the browser.

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
  cli/        Command-line interface
  server/     Web-based treemap server (axum + WebSocket)
  wasm/       Browser frontend compiled to WebAssembly
  macos/      macOS scanner
  linux/      Linux scanner
  windows/    Windows scanner
```

## License

[MIT](LICENSE)
