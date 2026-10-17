use std::ffi::c_void;
use std::hash::Hasher;
use std::num::NonZeroUsize;
use std::os::windows::ffi::OsStrExt;
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
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_DIRECTORY, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND,
    GetLastError, INVALID_HANDLE_VALUE, MAX_PATH,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileIdBothDirectoryInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
    OPEN_EXISTING,
};

/// Open the directory at `path`, a NUL-terminated UTF-16 path.
fn open_dir(path: &[u16]) -> std::io::Result<OwnedHandle> {
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
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Whether failing to open a directory with `error` leaves it out quietly, as the Unix scanners do:
/// no permission, or it's gone or no longer a directory.
fn expected_open_failure(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(ERROR_ACCESS_DENIED | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_DIRECTORY)
    )
}

/// Whether the 64-bit file ids that directory listings carry identify the files on `handle`'s
/// volume: only on NTFS, where every file has one, unique on its volume. ReFS ids are 128-bit, and
/// FAT and some network shares have no real ids, so there two different files could share one, and
/// deduplicating hardlinks, or telling folders apart, by them would hide one.
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

/// `id` as a file id, if it is one: 0 means a file system has no 64-bit ids, and all ones that the
/// file has no unique one (MS-FSCC), and some file systems give one of those to every file.
fn usable_file_id(id: u64) -> Option<u64> {
    (id != 0 && id != u64::MAX).then_some(id)
}

/// A folder's id where its volume's file ids don't tell folders apart: from its path (verbatim
/// UTF-16, without the NUL), which no other folder has. Deterministic, like a file id.
fn path_id(path: &[u16]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    for &unit in path {
        hasher.write_u16(unit);
    }
    hasher.finish()
}

/// The node id of a folder at `path` on `volume`, with file id `file_id` there: by that id where it
/// identifies the folder, and by its path otherwise (see `has_unique_file_ids`, `usable_file_id`).
/// Without that, every folder of a volume whose file ids are all 0 (sshfs-win, some redirectors)
/// would share one id, and all but the first would be left out as already read.
fn folder_id(volume: u32, file_id: u64, unique_file_ids: bool, path: &[u16]) -> u64 {
    match usable_file_id(file_id).filter(|_| unique_file_ids) {
        Some(file_id) => node_id(volume as u64, file_id),
        None => node_id(volume as u64, path_id(path)),
    }
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
    let path = std::fs::canonicalize(root).with_context(|| format!("can't scan {}", root.display()))?;
    let handle = open_dir(&wide(&path)).with_context(|| format!("can't open {}", plain_path(&path).display()))?;
    let (volume_serial, file_id) =
        get_volume_and_id(&handle).with_context(|| format!("can't identify {}", plain_path(&path).display()))?;
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
    let root = WinDir {
        unique_file_ids: has_unique_file_ids(&handle),
        handle,
        path: path.as_os_str().encode_wide().collect(),
        volume: volume_serial,
    };
    let id = folder_id(volume_serial, file_id, root.unique_file_ids, &root.path);
    let threads = options.threads.map_or_else(cpu_count, NonZeroUsize::get);
    // Opened before the scan starts: a volume that can't be read fails the scan before anything is
    // sent, as an unopenable root does.
    let required = match options.mft {
        Mft::Require => Some(mft::Volume::open(&root.handle)?),
        _ => None,
    };
    let scan = Started::new(path, tx);
    if let Some(volume) = required {
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
    /// Its path as Windows has it (verbatim, no NUL), for opening its subdirectories: joined from
    /// the names listings give, which needn't be valid UTF-16 (NTFS allows an unpaired surrogate),
    /// not from the lossy names the tree shows.
    path: Vec<u16>,
    /// The serial number of its volume. File ids are unique only within a volume.
    volume: u32,
    /// See `has_unique_file_ids`.
    unique_file_ids: bool,
}

/// The subdirectory that `info` lists in `parent` as `name`, opened by its path, with its node id.
/// `None` if it is on another volume than `root_volume` while `cross_filesystems` is off; the error
/// if it can't be opened.
fn open_child(
    parent: &WinDir,
    info: &FILE_ID_BOTH_DIR_INFO,
    name: &[u16],
    root_volume: u32,
    cross_filesystems: bool,
) -> std::io::Result<Option<(u64, WinDir)>> {
    let separator = u16::from(b'\\');
    let mut path = Vec::with_capacity(parent.path.len() + 1 + name.len() + 1);
    path.extend_from_slice(&parent.path);
    if path.last() != Some(&separator) {
        path.push(separator);
    }
    path.extend_from_slice(name);
    path.push(0);
    let handle = open_dir(&path)?;
    path.pop();
    // The volume serial and file id, for node identity (so the same file id on two volumes can't
    // collide) and the cross-filesystem check. Only a reparse point can lead to another volume
    // (`Entry::kind` skips junctions and volumes mounted in folders, which share their tag; DFS
    // links are another kind), so a plain directory is on its parent's, and where listed ids are
    // unique the listing's is its own. Asking for them is ~7% of a scan, so only the others are.
    let listed = (parent.volume, info.FileId as u64);
    let plain = info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0;
    let found = if plain && parent.unique_file_ids {
        Some(listed)
    } else {
        get_volume_and_id(&handle)
    };
    if !cross_filesystems && found.is_none_or(|(volume, _)| volume != root_volume) {
        return Ok(None);
    }
    let (volume, file_id) = found.unwrap_or(listed);
    let unique_file_ids = if volume == parent.volume {
        parent.unique_file_ids
    } else {
        has_unique_file_ids(&handle)
    };
    let id = folder_id(volume, file_id, unique_file_ids, &path);
    let child = WinDir {
        handle,
        path,
        volume,
        unique_file_ids,
    };
    Ok(Some((id, child)))
}

/// Read one directory with `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`, which returns
/// entries with their sizes, times and attributes in batches. False if it can't be listed at all
/// for lack of permission, which leaves it out like one that can't be opened.
fn read_dir(
    dir: &DirJob<WinDir>,
    buffer: &mut [u8],
    sink: &mut DirSink<'_, WinDir>,
    root_volume_serial: u32,
    cross_filesystems: bool,
) -> bool {
    // Each entry's name as listed, reused from one entry to the next.
    let mut name_units: Vec<u16> = Vec::new();
    let mut first = true;
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
            // ERROR_NO_MORE_FILES is the normal end of the directory, and ERROR_FILE_NOT_FOUND that
            // of an empty one without `.` and `..` (a FAT root); anything else is a real failure that
            // shouldn't pass silently for one.
            match unsafe { GetLastError() } {
                ERROR_NO_MORE_FILES | ERROR_FILE_NOT_FOUND => {}
                ERROR_ACCESS_DENIED if first && !dir.is_root() => return false,
                code => sink.read_failed(format_args!(
                    "listing it: {}",
                    std::io::Error::from_raw_os_error(code as i32)
                )),
            }
            break;
        }
        first = false;

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

            // The variable-length filename, decoded from its bytes: a `&[u16]` over them would
            // need them 2-aligned, which a `Vec<u8>` doesn't promise. The declared length is clamped
            // to what remains in the buffer: the kernel only writes whole records, but a
            // malformed/over-long FileNameLength must not read past the buffer end.
            let name_field_offset = offset + std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            let name_end = (name_field_offset + info.FileNameLength as usize).min(buffer.len());
            name_units.clear();
            name_units.extend(
                buffer
                    .get(name_field_offset..name_end)
                    .unwrap_or_default()
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&unit| u16::from_ne_bytes(unit)),
            );
            let name = String::from_utf16_lossy(&name_units);

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
                        match open_child(&dir.handle, &info, &name_units, root_volume_serial, cross_filesystems) {
                            Ok(Some((id, child))) => sink.dir(id, &name, child),
                            Ok(None) => {}
                            Err(error) if expected_open_failure(&error) => {}
                            Err(error) => sink.read_failed(format_args!("can't open {name}: {error}")),
                        }
                    }
                    EntryKind::File { size } => {
                        // The listing carries no link count, and getting one would mean opening
                        // every file, so every file goes through the hardlink check. Hardlinks are
                        // common on a system drive: most of WinSxS is linked into System32.
                        let hardlink = usable_file_id(info.FileId as u64)
                            .filter(|_| dir.handle.unique_file_ids)
                            .map(|file_id| node_id(dir.handle.volume as u64, file_id));
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

    /// Folders are told apart by their ids where those are real, and by their paths where the volume
    /// has none (or gives the "none" values): never all the same.
    #[test]
    fn folder_ids_without_real_file_ids_come_from_the_path() {
        let path = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
        let (a, b) = (path(r"\\?\Z:\a"), path(r"\\?\Z:\b"));
        assert_eq!(folder_id(7, 42, true, &a), node_id(7, 42));
        for unusable in [0, u64::MAX] {
            assert_ne!(folder_id(7, unusable, true, &a), folder_id(7, unusable, true, &b));
        }
        assert_ne!(
            folder_id(7, 42, false, &a),
            folder_id(7, 42, false, &b),
            "ids that aren't unique"
        );
        assert_eq!(
            folder_id(7, 0, false, &a),
            folder_id(7, 0, false, &a),
            "the same path, the same id"
        );
    }
}
