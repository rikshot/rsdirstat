use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use dashmap::DashSet;
use rsdirstat_core::scan::WorkQueue;
use rsdirstat_protocol::ScanEvent;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileIdBothDirectoryInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, OPEN_EXISTING,
};

const BUF_SIZE: usize = 1024 * 1024;

// Convert Windows FILETIME (100ns since 1601-01-01) to Unix epoch seconds
fn filetime_to_unix(ft: i64) -> i64 {
    const EPOCH_DIFF: i64 = 11_644_473_600;
    (ft / 10_000_000) - EPOCH_DIFF
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn wide_str(s: &str) -> Vec<u16> {
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

fn get_volume_and_id(handle: &OwnedHandle) -> Option<(u32, u64)> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0 {
        return None;
    }
    let file_id = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
    Some((info.dwVolumeSerialNumber, file_id))
}

struct WorkItem {
    handle: OwnedHandle,
    file_id: u64,
    parent_id: u64,
    name: String,
    path: String,
}

struct RootInfo {
    file_id: u64,
    volume_serial: u32,
    name: String,
    handle: OwnedHandle,
    path: String,
}

// Strip the \\?\ extended-path prefix for display while keeping it for opens
fn strip_extended_prefix(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

fn open_root(root: &Path) -> Result<RootInfo> {
    let path = std::fs::canonicalize(root).context("failed to canonicalize root path")?;
    let path_str = path.display().to_string();
    let display_path = strip_extended_prefix(&path_str);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| display_path.to_string());
    let handle = open_dir(&wide_path(&path)).context("failed to open root directory")?;
    let (volume_serial, file_id) = get_volume_and_id(&handle).context("failed to get root directory info")?;
    Ok(RootInfo {
        file_id,
        volume_serial,
        name,
        handle,
        path: display_path.to_string(),
    })
}

pub fn scan(root: &Path, cross_filesystems: bool, tx: std::sync::mpsc::Sender<ScanEvent>) -> Result<()> {
    let root_info = open_root(root)?;

    let _ = tx.send(ScanEvent::ScanStart {
        path: root_info.name.clone(),
    });

    let num_threads = thread::available_parallelism().map(|n| n.get()).unwrap_or(4);

    let visited = {
        let set = DashSet::new();
        set.insert(root_info.file_id);
        Arc::new(set)
    };

    let work = Arc::new(WorkQueue::new());
    work.push(vec![WorkItem {
        handle: root_info.handle,
        file_id: root_info.file_id,
        parent_id: 0,
        name: root_info.name.clone(),
        path: root_info.path,
    }]);

    let active_dirs: Arc<Vec<Mutex<String>>> = Arc::new((0..num_threads).map(|_| Mutex::new(String::new())).collect());
    let root_volume_serial = root_info.volume_serial;

    let _handles: Vec<_> = (0..num_threads)
        .enumerate()
        .map(|(tid, _)| {
            let work = Arc::clone(&work);
            let tx = tx.clone();
            let visited = Arc::clone(&visited);
            let active = Arc::clone(&active_dirs);
            thread::spawn(move || {
                let mut buffer = vec![0u8; BUF_SIZE];

                while let Some(item) = work.take() {
                    *active[tid].lock().unwrap() = item.path.clone();
                    scan_directory(
                        item.handle,
                        item.file_id,
                        item.parent_id,
                        &item.name,
                        root_volume_serial,
                        cross_filesystems,
                        &mut buffer,
                        &work,
                        &tx,
                        &visited,
                        &item.path,
                    );
                    work.finish_one();
                }
            })
        })
        .collect();

    work.wait_with_stall_detection(|| {
        eprintln!("Stuck directories:");
        for dir in active_dirs.iter() {
            let name = dir.lock().unwrap();
            if !name.is_empty() {
                eprintln!("  {name}");
            }
        }
    });

    let _ = tx.send(ScanEvent::ScanDone);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn scan_directory(
    handle: OwnedHandle,
    dir_file_id: u64,
    parent_id: u64,
    dir_name: &str,
    root_volume_serial: u32,
    cross_filesystems: bool,
    buffer: &mut [u8],
    work: &WorkQueue<WorkItem>,
    tx: &std::sync::mpsc::Sender<ScanEvent>,
    visited: &DashSet<u64>,
    dir_path: &str,
) {
    let mut new_work = Vec::new();
    let mut dir_total: u64 = 0;
    let mut dir_mtime: i64 = 0;

    loop {
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle.as_raw_handle(),
                FileIdBothDirectoryInfo,
                buffer.as_mut_ptr() as *mut c_void,
                buffer.len() as u32,
            )
        };

        if ok == 0 {
            break;
        }

        let mut offset = 0usize;
        loop {
            if offset + std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>() > buffer.len() {
                break;
            }

            let info = unsafe { &*(buffer.as_ptr().add(offset) as *const FILE_ID_BOTH_DIR_INFO) };
            let name_len = (info.FileNameLength as usize) / 2;

            // Read variable-length filename
            let name_ptr = unsafe {
                buffer
                    .as_ptr()
                    .add(offset)
                    .add(std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName)) as *const u16
            };
            let name_slice = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };
            let name = String::from_utf16_lossy(name_slice);

            if name != "." && name != ".." {
                let file_id = info.FileId as u64;
                let attrs = info.FileAttributes;
                let is_dir = (attrs & FILE_ATTRIBUTE_DIRECTORY) != 0;
                let is_reparse = (attrs & FILE_ATTRIBUTE_REPARSE_POINT) != 0;

                if is_dir && !is_reparse {
                    let child_path = format!("{}\\{}", dir_path, name);
                    if let Some(child_handle) = open_dir(&wide_str(&child_path)) {
                        if !cross_filesystems {
                            if let Some((vol, _)) = get_volume_and_id(&child_handle) {
                                if vol != root_volume_serial {
                                    if info.NextEntryOffset == 0 {
                                        break;
                                    }
                                    offset += info.NextEntryOffset as usize;
                                    continue;
                                }
                            } else {
                                if info.NextEntryOffset == 0 {
                                    break;
                                }
                                offset += info.NextEntryOffset as usize;
                                continue;
                            }
                        }

                        if visited.insert(file_id) {
                            new_work.push(WorkItem {
                                handle: child_handle,
                                file_id,
                                parent_id: dir_file_id,
                                name: name.clone(),
                                path: child_path,
                            });
                        }
                    }
                } else if !is_dir && !is_reparse {
                    let file_size = info.EndOfFile as u64;
                    let mtime = filetime_to_unix(info.LastWriteTime);
                    dir_total += file_size;
                    dir_mtime = dir_mtime.max(mtime);
                    if file_size > 0 {
                        let _ = tx.send(ScanEvent::File {
                            parent: dir_file_id,
                            name,
                            size: file_size,
                            mtime,
                        });
                    }
                }
            }

            if info.NextEntryOffset == 0 {
                break;
            }
            offset += info.NextEntryOffset as usize;
        }
    }

    drop(handle);

    let _ = tx.send(ScanEvent::Dir {
        id: dir_file_id,
        parent: parent_id,
        name: dir_name.to_string(),
        size: dir_total,
        mtime: dir_mtime,
    });

    work.push(new_work);
}
