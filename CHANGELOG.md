# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.3] - 2026-10-10

### Added

- `-V` / `--version` on both `rsdirstat` and `rsdirstat-cli`.
- Homebrew install for macOS and Linux: `brew install rikshot/tap/rsdirstat`. Each release
  now publishes a formula to the [rikshot/homebrew-tap](https://github.com/rikshot/homebrew-tap)
  tap.

### Changed

- The treemap's Depth setting now counts the levels it shows: `1` is a flat map of the
  current folder and `N` draws `N` levels. It used to draw one level more than it said, so
  there was no way to get a flat map. The default moves from 5 to 6, which draws as many
  folder levels as before. ([#5](https://github.com/rikshot/rsdirstat/issues/5))

### Fixed

- Folders that hold only files now open up and show their files when there is room, instead
  of staying solid blocks at every depth. Deep in a tree, where such folders are common, the
  treemap looked much shallower than the Depth setting.
  ([#5](https://github.com/rikshot/rsdirstat/issues/5))
- Hover, tooltips, right-click and click-to-navigate now act on the rectangle under the
  cursor. When rsdirstat was started with a path, they acted on whatever was about 60px below
  the cursor until the window was resized.
- Ctrl-C, SIGTERM and the automatic shutdown after the last tab closes now stop the server
  promptly during a scan, instead of waiting for the whole scan to finish.
- If the chosen path can't be scanned (it doesn't exist, can't be opened, or its volume was
  unmounted), the page now goes back to the volume picker. Previously the server considered
  the scan running forever: it refused Rescan and new paths, and tabs sat at "Waiting for
  scan..." until it was restarted.
- `--wait` mode shows its Start Scan button again. The page was blank, so the scan could
  never be started.
- The server now shuts down after the last tab closes even when a connection failed early,
  and no longer shuts down too soon after a quick disconnect, reconnect and disconnect.
- The published crates now include the `LICENSE` file, and the README's license link on
  crates.io no longer 404s.

### Security

- The WebSocket origin check now accepts only `localhost` and loopback IP addresses. It
  accepted any host starting with `127.`, so a web page served from a hostname such as
  `127.0.0.1.attacker.example` could drive a running rsdirstat: start scans and read back
  directory and file names and sizes.

## [0.1.2] - 2026-07-05

### Fixed

- Treemap no longer stalls to roughly one redraw per second while scanning large,
  deeply-nested trees (for example a full Windows `C:\` scan). The server was
  serializing and DEFLATE-compressing thousands of sub-pixel directory rects that
  the client never draws — a high-fan-out directory such as WinSxS produced tens of
  thousands of them — which dominated CPU and starved the browser of layout updates.
  The layout now drops rects the client won't draw before serializing, and
  per-message compression runs at level 1 instead of 6.

### Changed

- The internal `profiling` build profile uses thin LTO so `samply` symbolicates
  cleanly. No effect on release binaries.

## [0.1.1] - 2026-07-04

Release-pipeline shakedown — validated the tag-driven crates.io publish (OIDC
Trusted Publishing) and the prebuilt-binary release. No user-facing changes.

## [0.1.0] - 2026-07-04

### Added

- Initial release: a fast, cross-platform disk-usage analyzer with an interactive
  squarified-treemap GUI (served locally over WebSocket) and a `rsdirstat-cli`
  top-N reporter.
- Multi-threaded, platform-native scanning: `getattrlistbulk` on macOS,
  `getdents64` + `statx` on Linux, `GetFileInformationByHandleEx` on Windows.
- Treemap: zoom navigation, color by file type or modification time, filter by
  extension/size/name, click to navigate, right-click to reveal in the file manager.

[0.1.3]: https://github.com/rikshot/rsdirstat/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/rikshot/rsdirstat/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/rikshot/rsdirstat/releases/tag/v0.1.1
[0.1.0]: https://crates.io/crates/rsdirstat/0.1.0
