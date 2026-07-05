# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.1.2]: https://github.com/rikshot/rsdirstat/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/rikshot/rsdirstat/releases/tag/v0.1.1
[0.1.0]: https://crates.io/crates/rsdirstat/0.1.0
