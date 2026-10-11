# rsdirstat

[![CI](https://github.com/rikshot/rsdirstat/actions/workflows/ci.yml/badge.svg)](https://github.com/rikshot/rsdirstat/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/rikshot/rsdirstat/branch/main/graph/badge.svg)](https://codecov.io/gh/rikshot/rsdirstat)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/rikshot/rsdirstat/blob/main/LICENSE)
![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-informational)

Fast, cross-platform disk usage analyzer with an interactive treemap GUI.

<!-- Absolute URL: crates.io resolves relative paths against crates/rsdirstat/, where the image isn't. -->
![rsdirstat showing a treemap of the macOS Command Line Tools, with a tooltip on libclang.dylib](https://raw.githubusercontent.com/rikshot/rsdirstat/main/assets/screenshot.png)

## Features

- **Multi-threaded scanning** using platform-native APIs for maximum speed
  - macOS: `getattrlistbulk` for batch directory + metadata reads
  - Linux: `getdents64` + `statx` via raw syscalls
  - Windows: `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`
- **Interactive treemap GUI** served as a local web app over WebSocket
  - Squarified treemap layout with zoom navigation, updated live while the scan runs
  - Color by file type or modification time
  - Filter by extension, size, or name
  - Click to navigate, right-click to reveal in file manager
- **CLI mode** for quick top-N reports

## Installation

### Homebrew (macOS / Linux)

```sh
brew install rikshot/tap/rsdirstat
```

### Prebuilt binaries

No Rust toolchain required — the installer scripts drop a self-contained binary for your
platform onto your `PATH`:

**macOS / Linux**

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/rikshot/rsdirstat/releases/latest/download/rsdirstat-installer.sh | sh
```

**Windows (PowerShell)**

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/rikshot/rsdirstat/releases/latest/download/rsdirstat-installer.ps1 | iex"
```

Or download an archive for your platform directly from the
[latest release](https://github.com/rikshot/rsdirstat/releases/latest).

### With Cargo

```sh
cargo binstall rsdirstat   # fetches the prebuilt binaries from the GitHub release
cargo install rsdirstat    # compiles from source (requires Rust 1.89+, edition 2024)
```

Every method installs two binaries: **`rsdirstat`** (the GUI server, with the web frontend
embedded — a single self-contained binary, no extra files needed) and **`rsdirstat-cli`**
(the terminal top-N reporter). To build the frontend bundle from a checkout, see
[Building](#building).

## Usage

### GUI

```sh
rsdirstat                 # no path: pick a volume in the browser, then scan
rsdirstat [path]          # scan a path, opens the treemap in your browser
rsdirstat --all [path]    # cross filesystem boundaries
rsdirstat --port 8080 [path]   # fixed port (default: random)
rsdirstat --no-open [path]     # don't auto-open the browser
```

`rsdirstat` prints the address it serves (`Listening on http://localhost:<port>`) and opens
it in your browser. It stops on Ctrl-C, or by itself a few seconds after the last browser tab
is closed (reloading the page is fine).

The server only listens on `127.0.0.1` and only accepts WebSocket connections from pages
served by `localhost`, so neither other machines nor other websites open in your browser can
read the file listing. Nothing leaves your machine.

### The treemap

Every file is a rectangle sized by its share of the disk space; each folder is a box with a
header showing its name and total size, with its contents drawn inside. Files too small to
draw are grouped into one *(other files)* block per folder.

- **Hover** a rectangle to see its name, size, share of the current view and modification
  date; its path is shown at the bottom.
- **Click** a folder to zoom into it. The breadcrumb bar at the top shows where you are —
  click any parent to zoom back out.
- **Right-click** a file or folder to reveal it in Finder, Explorer or your file manager.
- **Depth** sets how many levels below the current folder are drawn (1–10, default 6).
  Folders below that depth, or too small to fit their contents, are drawn as a single
  block — click one to zoom in.
- **Color** by **Type** colors files by kind: images green, video blue, audio purple, text
  yellow, fonts pink, and most others (binaries, archives, documents, ...) red; a folder drawn
  as a single block gets a color from its name. **Age** colors files and folders by
  modification time, from red (oldest) to green (newest).
- **Filters** narrow the treemap to matching files, and folder sizes are recomputed from just
  those: **Ext** takes a comma-separated list of extensions (`mp4,mov`), **Name** matches part
  of the file name (case-insensitive), and **Min** hides files smaller than the given size.
  **Clear** resets them.
- **Rescan** scans the same path again; **Change** goes back to the volume picker.

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

Only the native platform scanner is compiled: each scanner crate builds only for its own OS.
Release binaries are built natively on each platform by the release workflow. Cross-compiling
needs a C cross-compiler for the target as well as the Rust target, because `mimalloc` and
`zstd` are C libraries.

## Development

The test suite runs with [cargo-nextest](https://nexte.st). The browser tests (`e2e_*`,
`visual_*`) drive the real server in Chromium, Firefox and WebKit through
[playwright-rs](https://crates.io/crates/playwright-rs), whose CLI must match the version in
`Cargo.lock`:

```sh
cargo install --locked cargo-nextest trunk
cargo install --locked playwright-rs@0.19.0 --features cli   # the version in Cargo.lock
playwright-rs install chromium firefox webkit   # add --with-deps on Linux

trunk build        # build the frontend once, before the browser tests
cargo nextest run
```

The visual tests compare screenshots against baselines in `crates/rsdirstat/tests/snapshots/`.
Those are git-ignored and recorded on the first run, so delete them to re-record after an
intentional rendering change.

On macOS 27, if the Firefox tests fail with `Could not find profile folder`, point
`CFFIXED_USER_HOME` at an empty directory: `CFFIXED_USER_HOME=$(mktemp -d) cargo nextest run`.

## Project Structure

```
crates/
  core/       Shared library: directory tree, scan work queue, treemap layout, filters, colors
  protocol/   Shared wire protocol for server and wasm frontend
  rsdirstat/  App crate — `rsdirstat` (axum + WebSocket GUI) and `rsdirstat-cli` binaries
  wasm/       Browser frontend compiled to WebAssembly (embedded into rsdirstat)
  macos/      macOS scanner
  linux/      Linux scanner
  windows/    Windows scanner
```

## License

[MIT](https://github.com/rikshot/rsdirstat/blob/main/LICENSE)
