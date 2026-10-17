use std::ffi::c_void;
use std::num::NonZeroUsize;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::entry::{Entry, EntryKind, filetime_to_unix, may_be_ea_size, reparse_tag_by_id};
use crate::{from_wide, mft, ntfs, wide};
use anyhow::{Context, Result};
use rsdirstat_core::scan::{
    DirJob, DirSink, Mft, ScanEvent, ScanOptions, Started, cpu_count, node_id, plain_path, run_scan,
};
use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, GetLastError, INVALID_HANDLE_VALUE, MAX_PATH};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileIdBothDirectoryInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
    OPEN_EXISTING,
};

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

/// Whether the 64-bit file ids that directory listings carry identify the files on `handle`'s
/// volume: only on NTFS, where every file has one, unique on its volume. ReFS ids are 128-bit, and
/// FAT and some network shares have no real ids, so there two different files could share one, and
/// deduplicating hardlinks by them would hide one.
fn has_unique_file_ids(handle: &OwnedHandle) -> bool {
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
    ok != 0 && from_wide(&filesystem) == "NTFS"
}

fn get_volume_and_id(handle: &OwnedHandle) -> Option<(u32, u64)> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0 {
        return None;
    }
    let file_id = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
    Some((info.dwVolumeSerialNumber, file_id))
}

/// The scan root: its canonical path, open, and its volume's serial number and file id.
fn open_root(root: &Path) -> Result<(PathBuf, OwnedHandle, u32, u64)> {
    let path = std::fs::canonicalize(root).context("failed to canonicalize root path")?;
    let handle = open_dir(&wide(&path)).context("failed to open root directory")?;
    let (volume_serial, file_id) = get_volume_and_id(&handle).context("failed to get root directory info")?;
    Ok((path, handle, volume_serial, file_id))
}

/// Scan `root`, stopping promptly once `cancel` is set (the server sets it when it shuts down
/// mid-scan).
pub fn scan(
    root: &Path,
    options: ScanOptions,
    tx: std::sync::mpsc::Sender<ScanEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let (path, handle, volume_serial, file_id) = open_root(root)?;
    let id = node_id(volume_serial as u64, file_id);
    let root = WinDir {
        unique_file_ids: has_unique_file_ids(&handle),
        handle,
        volume: volume_serial,
    };
    let threads = options.threads.map_or_else(cpu_count, NonZeroUsize::get);
    let scan = Started::new(path, tx);
    if options.mft == Mft::Require {
        let volume = mft::Volume::open(&root.handle)?;
        return volume.scan(&scan, &root.handle, file_id, volume_serial, threads, &cancel);
    }
    if options.mft == Mft::Auto
        && root.unique_file_ids
        && ntfs::is_volume_root(file_id)
        && let Ok(volume) = mft::Volume::open(&root.handle)
    {
        match volume.scan(&scan, &root.handle, file_id, volume_serial, threads, &cancel) {
            Ok(()) => return Ok(()),
            Err(e) => eprintln!(
                "Can't read the MFT for {}, walking it instead: {e:#}",
                plain_path(scan.root()).display()
            ),
        }
    }
    run_scan(scan, root, id, cancel, threads, move |dir, buffer, sink| {
        read_dir(dir, buffer, sink, volume_serial, options.cross_filesystems)
    });
    Ok(())
}

/// An open directory, with what its files are told apart by.
struct WinDir {
    handle: OwnedHandle,
    /// The serial number of its volume. File ids are unique only within a volume.
    volume: u32,
    /// See `has_unique_file_ids`.
    unique_file_ids: bool,
}

/// The subdirectory that `info` lists in `parent`, opened by its `path`, with its node id. `None` if
/// it can't be opened, or is on another volume than `root_volume` while `cross_filesystems` is off.
fn open_child(
    parent: &WinDir,
    info: &FILE_ID_BOTH_DIR_INFO,
    path: &str,
    root_volume: u32,
    cross_filesystems: bool,
) -> Option<(u64, WinDir)> {
    let handle = open_dir(&wide(path))?;
    // The volume serial and file id, for node identity (so the same file id on two volumes can't
    // collide) and the cross-filesystem check. Only a reparse point can lead to another volume
    // (`classify` skips mount points and junctions; DFS links are another kind), so a plain
    // directory is on its parent's, and where listed ids are unique the listing's is its own.
    // Asking for them is ~7% of a scan, so only the others are.
    let listed = (parent.volume, info.FileId as u64);
    let plain = info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0;
    let found = if plain && parent.unique_file_ids {
        Some(listed)
    } else {
        get_volume_and_id(&handle)
    };
    if !cross_filesystems && found.is_none_or(|(volume, _)| volume != root_volume) {
        return None;
    }
    let (volume, file_id) = found.unwrap_or(listed);
    let unique_file_ids = if volume == parent.volume {
        parent.unique_file_ids
    } else {
        has_unique_file_ids(&handle)
    };
    let child = WinDir {
        handle,
        volume,
        unique_file_ids,
    };
    Some((node_id(volume as u64, file_id), child))
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
            // ERROR_NO_MORE_FILES is the normal end of the directory; anything else is a real failure
            // that shouldn't pass silently for one.
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
                // When the reparse bit is set, the enumeration overlays the reparse tag onto EaSize,
                // unless it holds the size of the entry's extended attributes: then the file says.
                let mut reparse_tag = info.EaSize;
                if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    && may_be_ea_size(reparse_tag)
                    && dir.handle.unique_file_ids
                {
                    reparse_tag = reparse_tag_by_id(&dir.handle.handle, info.FileId).unwrap_or(reparse_tag);
                }
                match Entry::listed(info.FileAttributes, reparse_tag, info.EndOfFile, info.AllocationSize).kind() {
                    EntryKind::Directory => {
                        let path = sink.child_path(&name);
                        if let Some((id, child)) =
                            open_child(&dir.handle, &info, &path, root_volume_serial, cross_filesystems)
                        {
                            sink.dir(id, &name, child);
                        }
                    }
                    EntryKind::File { size } => {
                        // The listing carries no link count, and getting one would mean opening
                        // every file, so every file goes through the hardlink check. Hardlinks are
                        // common on a system drive: most of WinSxS is linked into System32.
                        let hardlink = dir
                            .handle
                            .unique_file_ids
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
