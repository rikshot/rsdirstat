# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - Unreleased

### Added

- On Windows, scanning a whole NTFS drive as administrator reads the drive's file index (the
  MFT) straight off the disk instead of walking its folders: several times faster, with all of
  `C:` in about 2 s where the walk took 8 on the test machine. Without administrator rights, on
  other file systems and on network shares, rsdirstat walks the folders as before; `--no-mft`
  always walks.
  ([#6](https://github.com/rikshot/rsdirstat/pull/6))
- `--threads N`, on both `rsdirstat` and `rsdirstat-cli`, scans with that many threads.
- Each browser tab has its own view: zooming, depth, colours and filters in one tab no longer
  change the others.
- A scan that couldn't read everything now says so. Folders that can't be read for a reason
  other than permission are printed with their paths, and the status bar and the CLI mark the
  results incomplete. So does a scan cut short because a folder stopped answering (a dead
  network mount).
- The volume picker's cards can be chosen from the keyboard.

### Changed

- The address `rsdirstat` opens now carries a token, made anew each run
  (`http://localhost:<port>/?token=…`, see Security). With `--no-open`, open the address it
  prints; a bookmark of the bare address no longer reaches a new run.
- Faster scans by default: macOS uses at most 4 threads, as more only queue up in the kernel
  (`/System/Library` in about 2 s instead of 5.6), and Linux two per CPU, which hides disk waits
  (a cold ext4 root in 0.46 s instead of 0.60). Plain folders on Windows scan about 7% faster.
- A scan waits 30 s, instead of 3, for storage that has stopped answering before giving up, so
  a disk spinning up or a NAS waking up no longer cuts it short.
- A path given on the command line that can't be scanned (missing, a file, or a folder that
  can't be read) is an error at startup, instead of opening the browser on the volume picker.
- `rsdirstat-cli` prints sizes as the page does (`150 MiB`, `1.00 KiB`), with units up to PiB.
- The extension filter also takes `.mp4`, `*.mp4` and extensions with several dots (`tar.gz`).
  The name filter ignores case beyond ASCII too.
- With a filter on, the status bar's total, the tooltip's share of the view and the Age colours
  count only the matching files.
- In Age mode, a folder drawn as one block is coloured by its newest file anywhere under it, not
  only by the files directly in it, and *(other files)* blocks by their own age.
- Rust 1.89 or later is needed to build from source.
- The library crates (`rsdirstat-core`, `rsdirstat-protocol` and the three scanner crates) change
  their APIs, hence 0.2.0: one scan driver in core (`run_scan`, `DirSink`, `ScanOptions`,
  `ScanOutcome`) that the platforms supply directory reads to, `ScanEvent` moved to core with one
  event per directory, and a leaner wire format between the server and the page.

### Fixed

- Scans no longer leave data out without saying so:
  - A wide tree no longer runs out of file descriptors on macOS and Linux, which silently dropped
    whole subtrees.
  - A huge folder on slow storage no longer trips the stall detector, which cut the rest of the
    scan off.
  - Linux keeps names that aren't UTF-8, and everything under them.
  - Windows counts deduplicated, WOF-compressed and tiered files, a name with an unpaired
    surrogate and what's under it, and every folder of a volume without usable file ids
    (sshfs-win, FAT).
- Nothing is counted twice:
  - Windows counts hardlinks on NTFS once. Most of WinSxS is linked into System32, so a scan of
    `C:\` counted gigabytes twice.
  - A scan of `/` or `/System` on macOS no longer counts the data volume twice through its
    firmlinks.
  - Linux reads a bind mount once, and tells hardlinks apart by their inode even on FUSE file
    systems that report the same one for every entry.
- Other file systems are left alone without `--all`: mount points inside the scanned folder are no
  longer opened (which could wake a sleeping network share or hang on a dead one) and automount
  points aren't triggered. With `--all`, Linux still leaves out `/proc`, `/sys` and the like.
- Cloud files:
  - Windows counts online-only Dropbox and OneDrive files as 0, where it counted them in full.
  - macOS no longer fetches a cloud folder's contents from the network to list it.
- The volume picker:
  - On macOS it lists only real volumes, sized correctly past 16 TiB.
  - On Linux it keeps ZFS datasets and composefs roots, leaves out bind mounts of a folder (such
    as the Firefox snap's `host-hunspell` on Ubuntu), and a dead network mount no longer holds up
    the list.
  - Windows no longer shows a "There is no disk in the drive" box for an empty card reader.
  - Used space no longer counts the blocks a file system reserves for the superuser.
- Right-click reveal on Windows works for paths given on the command line, and for paths with
  commas in them. Elsewhere, each reveal no longer leaves a defunct process behind.
- A scan that can't open its folder within 30 s, or stalls on it, returns to the volume picker
  instead of showing "Scanning..." forever.
- After **Change**, or a scan that fails, resizing the window no longer brings the old treemap
  back over the picker.
- After a reload in Chromium or WebKit, the treemap matches the toolbar's filters.
- A tab opened during or after a scan shows the scan's own time, not the time since it connected.
- Every tab opened in `--wait` mode shows the Start button, not only the first.
- On Windows, the path bar no longer reads `C:\/Windows/...` under a drive root, and joins paths
  with `\` for any scan.
- The zoom animation moves each file to where it goes, instead of morphing unrelated files into
  each other. Items of the same size keep their places from one layout to the next, and a folder
  whose contents are all too small to draw is one block, not an empty dark box.
- The tooltip updates as the treemap changes under a resting pointer, the breadcrumb shows its
  end when it overflows, and the treemap redraws sharply after moving to a display of another
  pixel density.
- The page no longer leaks memory with each rescan and each keystroke in a filter.
- Big scans: layouts are 21-29% smaller and faster to make, a scan's results go into the tree
  about twice as fast, and resizing a filtered view no longer filters the whole tree again.
- The server exits within 2 s of being stopped, even with a request or a scan stuck on a dead
  network mount, and no longer shuts down under a tab that connects as the last one closes.
  Ctrl+Break stops it on Windows, like Ctrl-C.
- `rsdirstat-cli | head` no longer panics, and `--files` uses about half the memory.

### Security

- Other users of the machine, and programs that can't read your files, can no longer use a
  running rsdirstat. It listened only on `127.0.0.1`, but anything on the machine could connect
  to the port to read the file listing, start scans or reveal paths. Every request now needs the
  run's token, which the page gets from its address and keeps as a cookie. The browser opens the
  address through a file only you can read, so the token never appears on a command line that
  other users can see.
- Pages on other websites can no longer reach the server through DNS rebinding (every request
  must be addressed to `localhost` or a loopback IP), nor with cross-site requests such as an
  `<img>` pointed at the API (refused by `Sec-Fetch-Site` and `Origin`).
- The page can't be shown in another site's frame, where clicks on it could be disguised
  (clickjacking).
- A message from the page can no longer make the server allocate 256 MiB, and a viewport it
  sends is clamped, so a malformed one can't make a layout take arbitrarily long.
- On Windows, reveal runs Explorer from the Windows directory, not whatever `explorer.exe` is
  found first next to `rsdirstat.exe`.

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

[0.2.0]: https://github.com/rikshot/rsdirstat/compare/v0.1.3...v0.2.0
[0.1.3]: https://github.com/rikshot/rsdirstat/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/rikshot/rsdirstat/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/rikshot/rsdirstat/releases/tag/v0.1.1
[0.1.0]: https://crates.io/crates/rsdirstat/0.1.0
