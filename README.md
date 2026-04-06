# rsdirstat

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

```sh
cargo install --path crates/cli
cargo install --path crates/gui
```

## Usage

### GUI

```sh
rsdirstat-gui [path]          # opens browser with treemap
rsdirstat-gui --all [path]    # cross filesystem boundaries
```

### CLI

```sh
rsdirstat [path]              # top 10 directories by size
rsdirstat --files [path]      # top 10 files by size
rsdirstat --top 20 [path]     # show more results
rsdirstat --all [path]        # cross filesystem boundaries
```

## Building

```sh
cargo build --release
```

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
  core/       Shared library: treemap layout, binary protocol, work queue
  cli/        Command-line interface
  gui/        Web-based treemap GUI (axum + WebSocket)
  macos/      macOS scanner
  linux/      Linux scanner
  windows/    Windows scanner
```

## License

[MIT](LICENSE)
