use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use rsdirstat_core::scan::{DirJob, DirSink, ScanEvent, node_id, run_scan};
use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, GetLastError, INVALID_HANDLE_VALUE, MAX_PATH};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileIdBothDirectoryInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    GetVolumeInformationByHandleW, OPEN_EXISTING,
};

// Reparse tags for cloud placeholders created by the Cloud Files API (OneDrive, and other providers
// built on it). They share the base 0x9000001A; a provider index occupies the CLOUD_MASK (0xF000)
// nibble, spanning 0x9000001A..=0x9000F01A. Values mirror windows-sys' IO_REPARSE_TAG_CLOUD /
// IO_REPARSE_TAG_CLOUD_MASK (in Win32_System_SystemServices), defined here so we needn't pull that
// whole bindings module in for two integers.
const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001A;
const IO_REPARSE_TAG_CLOUD_MASK: u32 = 0x0000_F000;

/// The reparse tag bit marking a name surrogate: an entry that stands for another file or directory
/// (a symlink, a junction, a volume mount point), as the SDK's `IsReparseTagNameSurrogate` tests.
const REPARSE_TAG_NAME_SURROGATE: u32 = 0x2000_0000;

// Convert Windows FILETIME (100ns since 1601-01-01) to Unix epoch seconds
fn filetime_to_unix(ft: i64) -> i64 {
    const EPOCH_DIFF: i64 = 11_644_473_600;
    (ft / 10_000_000) - EPOCH_DIFF
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

pub(crate) fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_dir(path: &[u16]) -> Option<OwnedHandle> {
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    Some(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Whether the files on `handle`'s volume can be told apart as hardlinks by the 64-bit file id that
/// directory listings carry: only on NTFS, where every file has one, unique on its volume. ReFS ids
/// are 128-bit, and FAT and some network shares have no real ids, so there two different files could
/// share one, and deduplicating them would hide one.
fn dedupes_hardlinks(handle: &OwnedHandle) -> bool {
    let mut filesystem = [0u16; MAX_PATH as usize + 1];
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            handle.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    };
    let len = filesystem.iter().position(|&c| c == 0).unwrap_or(filesystem.len());
    ok != 0 && String::from_utf16_lossy(&filesystem[..len]) == "NTFS"
}

fn get_volume_and_id(handle: &OwnedHandle) -> Option<(u32, u64)> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0 {
        return None;
    }
    let file_id = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
    Some((info.dwVolumeSerialNumber, file_id))
}

pub fn scan(root: &Path, cross_filesystems: bool, tx: std::sync::mpsc::Sender<ScanEvent>) -> Result<()> {
    scan_cancellable(root, cross_filesystems, tx, Arc::default())
}

/// Scan, aborting promptly if `cancel` is set (the server sets it when it shuts down mid-scan).
pub fn scan_cancellable(
    root: &Path,
    cross_filesystems: bool,
    tx: std::sync::mpsc::Sender<ScanEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let path = std::fs::canonicalize(root).context("failed to canonicalize root path")?;
    let handle = open_dir(&wide_path(&path)).context("failed to open root directory")?;
    let (volume_serial, file_id) = get_volume_and_id(&handle).context("failed to get root directory info")?;
    let id = node_id(volume_serial as u64, file_id);
    let root = WinDir {
        dedupe_hardlinks: dedupes_hardlinks(&handle),
        handle,
        volume: volume_serial,
    };
    run_scan(&path, root, id, tx, cancel, move |dir, buffer, sink| {
        read_dir(dir, buffer, sink, volume_serial, cross_filesystems)
    });
    Ok(())
}

/// An open directory, with what its files are told apart by.
struct WinDir {
    handle: OwnedHandle,
    /// The serial number of its volume. File ids are unique only within a volume.
    volume: u32,
    /// See `dedupes_hardlinks`.
    dedupe_hardlinks: bool,
}

/// What to do with one enumerated directory entry.
#[derive(Debug, PartialEq, Eq)]
enum EntryKind {
    /// A directory to descend into (a plain directory, or a cloud-placeholder directory).
    Directory,
    /// A file contributing `size` bytes — 0 for an online-only (dehydrated) cloud placeholder.
    File { size: u64 },
    /// A symlink / junction / mount point (a name surrogate): neither counted nor traversed, to
    /// avoid double-counting and traversal loops.
    Skip,
}

/// A cloud-placeholder reparse tag shares the base `IO_REPARSE_TAG_CLOUD` with a provider index in
/// the `IO_REPARSE_TAG_CLOUD_MASK` nibble; mask it out and compare to the base.
fn is_cloud_reparse_tag(tag: u32) -> bool {
    tag & !IO_REPARSE_TAG_CLOUD_MASK == IO_REPARSE_TAG_CLOUD
}

/// Decide how to treat a directory entry from its attributes, reparse tag, and sizes.
///
/// Only reparse points that stand for another name (symlinks, junctions, mount points) are skipped.
/// The others are the file or directory itself, with its data kept differently, and are counted like
/// any other: cloud placeholders (OneDrive et al., files *and* folders), deduplicated files, files
/// compressed by Windows (WOF, as Compact OS uses), and files tiered by Azure File Sync.
///
/// `reparse_tag` is only meaningful when `FILE_ATTRIBUTE_REPARSE_POINT` is set — the directory
/// enumeration overlays it onto the entry's `EaSize` field (per [MS-FSCC]).
fn classify(attrs: u32, reparse_tag: u32, end_of_file: i64, allocation_size: i64) -> EntryKind {
    let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
    let is_reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;

    // Name surrogates redirect elsewhere — skip them to avoid double-counting and traversal loops.
    if is_reparse && reparse_tag & REPARSE_TAG_NAME_SURROGATE != 0 {
        return EntryKind::Skip;
    }

    if is_dir {
        return EntryKind::Directory;
    }

    // An online-only (unmaterialized) cloud placeholder occupies 0 bytes on disk — AllocationSize 0
    // — even though EndOfFile still reports its full "online" length; count it as 0. On-disk
    // allocation is a provider- and hydration-policy-agnostic signal, unlike the assorted
    // FILE_ATTRIBUTE_RECALL_ON_* flags different providers set. A materialized placeholder — and any
    // other file, including a sparse, deduplicated, compressed or tiered one — counts at its logical
    // size, matching the rest of the tree and the macOS dataless-placeholder handling.
    let unmaterialized_cloud = is_reparse && is_cloud_reparse_tag(reparse_tag) && allocation_size == 0;
    EntryKind::File {
        size: if unmaterialized_cloud { 0 } else { end_of_file as u64 },
    }
}

/// Read one directory with `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`, which returns
/// entries with their sizes, times and attributes in batches.
fn read_dir(
    dir: &DirJob<WinDir>,
    buffer: &mut [u8],
    sink: &mut DirSink<'_, WinDir>,
    root_volume_serial: u32,
    cross_filesystems: bool,
) -> bool {
    loop {
        let ok = unsafe {
            GetFileInformationByHandleEx(
                dir.handle.handle.as_raw_handle(),
                FileIdBothDirectoryInfo,
                buffer.as_mut_ptr() as *mut c_void,
                buffer.len() as u32,
            )
        };

        if ok == 0 {
            // ERROR_NO_MORE_FILES is the normal end-of-directory; anything else is a real failure
            // we shouldn't silently treat as completion (parity with the macOS/Linux scanners).
            let err = unsafe { GetLastError() };
            if err != ERROR_NO_MORE_FILES {
                eprintln!("GetFileInformationByHandleEx failed for {}: error {err}", dir.name);
            }
            break;
        }

        let mut offset = 0usize;
        loop {
            if offset + std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>() > buffer.len() {
                break;
            }

            // read_unaligned: `buffer` is a Vec<u8> (align 1) but FILE_ID_BOTH_DIR_INFO has 8-byte
            // fields, so forming a `&` reference to it is UB even though entries are in practice
            // 8-aligned. Copy the (Copy) header out by value; the variable-length name is read
            // separately below from the still-in-buffer bytes.
            let info = unsafe { std::ptr::read_unaligned(buffer.as_ptr().add(offset) as *const FILE_ID_BOTH_DIR_INFO) };

            // Read the variable-length filename. Clamp the declared length to what actually remains
            // in the buffer: the kernel only writes whole records, but a malformed/over-long
            // FileNameLength must not make from_raw_parts read past the buffer end.
            let name_field_offset = offset + std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            let avail_u16 = buffer.len().saturating_sub(name_field_offset) / 2;
            let name_len = ((info.FileNameLength as usize) / 2).min(avail_u16);
            let name_ptr = unsafe { buffer.as_ptr().add(name_field_offset) as *const u16 };
            let name_slice = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };
            let name = String::from_utf16_lossy(name_slice);

            if name != "." && name != ".." {
                // When the reparse bit is set, the enumeration overlays the reparse tag onto EaSize.
                match classify(info.FileAttributes, info.EaSize, info.EndOfFile, info.AllocationSize) {
                    EntryKind::Directory => {
                        let path = sink.child_path(&name);
                        if let Some(child) = open_dir(&wide_str(&path)) {
                            // Authoritative volume serial + file id for node identity; needed for
                            // every dir (not just the cross-fs check) so the same file id on two
                            // volumes can't collide.
                            let vid = get_volume_and_id(&child);
                            let same_volume = vid.is_some_and(|(vol, _)| vol == root_volume_serial);
                            if cross_filesystems || same_volume {
                                let (vol, fid) = vid.unwrap_or((dir.handle.volume, info.FileId as u64));
                                let child = WinDir {
                                    dedupe_hardlinks: if vol == dir.handle.volume {
                                        dir.handle.dedupe_hardlinks
                                    } else {
                                        dedupes_hardlinks(&child)
                                    },
                                    handle: child,
                                    volume: vol,
                                };
                                sink.dir(node_id(vol as u64, fid), &name, path, child);
                            }
                        }
                    }
                    EntryKind::File { size } => {
                        // The listing carries no link count, and getting one would mean opening
                        // every file, so every file goes through the hardlink check. Hardlinks are
                        // common on a system drive: most of WinSxS is linked into System32.
                        let hardlink = dir
                            .handle
                            .dedupe_hardlinks
                            .then(|| node_id(dir.handle.volume as u64, info.FileId as u64));
                        sink.file(name, size, filetime_to_unix(info.LastWriteTime), hardlink);
                    }
                    EntryKind::Skip => {}
                }
            }

            if info.NextEntryOffset == 0 {
                break;
            }
            offset += info.NextEntryOffset as usize;
        }

        if !sink.checkpoint() {
            break;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: u32 = FILE_ATTRIBUTE_DIRECTORY;
    const REPARSE: u32 = FILE_ATTRIBUTE_REPARSE_POINT;
    // A cloud tag with a non-zero provider index in the CLOUD_MASK nibble, to prove masking works.
    const CLOUD_TAG_3: u32 = 0x9000_301A;
    const SYMLINK_TAG: u32 = 0xA000_000C; // IO_REPARSE_TAG_SYMLINK
    const MOUNT_POINT_TAG: u32 = 0xA000_0003; // IO_REPARSE_TAG_MOUNT_POINT (junction)
    const LX_SYMLINK_TAG: u32 = 0xA000_001D; // IO_REPARSE_TAG_LX_SYMLINK (WSL)
    const DEDUP_TAG: u32 = 0x8000_0013; // IO_REPARSE_TAG_DEDUP
    const WOF_TAG: u32 = 0x8000_0017; // IO_REPARSE_TAG_WOF
    const STORAGE_SYNC_TAG: u32 = 0x8000_001E; // IO_REPARSE_TAG_STORAGE_SYNC (Azure File Sync)

    #[test]
    fn regular_file_counts_logical_size() {
        // attrs 0, some data on disk (allocation 4096).
        assert_eq!(classify(0, 0, 4096, 4096), EntryKind::File { size: 4096 });
    }

    #[test]
    fn regular_directory_recurses() {
        assert_eq!(classify(DIR, 0, 0, 0), EntryKind::Directory);
    }

    #[test]
    fn sparse_regular_file_is_not_zeroed() {
        // A non-cloud sparse file has 0 bytes allocated but a large logical length; under the tool's
        // apparent-size semantics it must still count at EndOfFile, not be mistaken for a placeholder.
        assert_eq!(classify(0, 0, 1_000_000, 0), EntryKind::File { size: 1_000_000 });
    }

    #[test]
    fn online_only_cloud_file_counts_zero() {
        // Dehydrated placeholder: cloud reparse tag, full online length in EndOfFile, 0 on disk.
        let entry = classify(REPARSE, IO_REPARSE_TAG_CLOUD, 10_000_000, 0);
        assert_eq!(entry, EntryKind::File { size: 0 });
    }

    #[test]
    fn hydrated_cloud_file_counts_logical_size() {
        // Locally-available placeholder: cloud reparse tag, data on disk (allocation > 0).
        assert_eq!(classify(REPARSE, CLOUD_TAG_3, 173, 4096), EntryKind::File { size: 173 });
    }

    #[test]
    fn cloud_directory_recurses() {
        // Folders are placeholders too; we must descend rather than skip the whole subtree.
        assert_eq!(classify(DIR | REPARSE, CLOUD_TAG_3, 0, 0), EntryKind::Directory);
    }

    #[test]
    fn symlink_and_junction_are_skipped() {
        assert_eq!(classify(REPARSE, SYMLINK_TAG, 0, 0), EntryKind::Skip);
        assert_eq!(classify(DIR | REPARSE, MOUNT_POINT_TAG, 0, 0), EntryKind::Skip);
        assert_eq!(classify(REPARSE, LX_SYMLINK_TAG, 0, 0), EntryKind::Skip);
    }

    #[test]
    fn reparse_points_holding_their_own_data_count_at_logical_size() {
        // Their data lives elsewhere (a chunk store, a compressed stream, the cloud), so nothing is
        // allocated to the file itself; they are still files, not redirections.
        for tag in [DEDUP_TAG, WOF_TAG, STORAGE_SYNC_TAG] {
            assert_eq!(
                classify(REPARSE, tag, 1_000_000, 0),
                EntryKind::File { size: 1_000_000 },
                "{tag:#x}"
            );
        }
    }

    #[test]
    fn cloud_tag_detection_masks_provider_nibble() {
        assert!(is_cloud_reparse_tag(IO_REPARSE_TAG_CLOUD));
        assert!(is_cloud_reparse_tag(0x9000_301A));
        assert!(is_cloud_reparse_tag(0x9000_F01A));
        assert!(!is_cloud_reparse_tag(SYMLINK_TAG));
        assert!(!is_cloud_reparse_tag(MOUNT_POINT_TAG));
        assert!(!is_cloud_reparse_tag(0));
    }
}
