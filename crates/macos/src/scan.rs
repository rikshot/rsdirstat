use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use dashmap::DashSet;
use rsdirstat_core::protocol::ScanEvent;
use rsdirstat_core::scan::{WorkQueue, raise_fd_limit};

const VREG: u32 = 1;
const VDIR: u32 = 2;

const BUF_SIZE: usize = 1024 * 1024;

static SCAN_ATTRS: libc::attrlist = libc::attrlist {
    bitmapcount: libc::ATTR_BIT_MAP_COUNT,
    reserved: 0,
    commonattr: libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_MODTIME
        | libc::ATTR_CMN_FILEID,
    volattr: 0,
    dirattr: 0,
    fileattr: libc::ATTR_FILE_TOTALSIZE,
    forkattr: 0,
};

struct WorkItem {
    fd: OwnedFd,
    file_id: u64,
    parent_id: u64,
    name: String,
    path: String,
}

struct RootInfo {
    path: std::path::PathBuf,
    ino: u64,
    dev: u64,
    name: String,
    fd: OwnedFd,
}

fn open_root(root: &Path) -> Result<RootInfo> {
    raise_fd_limit();
    let path = std::fs::canonicalize(root).context("failed to canonicalize root path")?;
    let meta = std::fs::metadata(&path).context("failed to stat root path")?;
    let dev = meta.dev();
    let ino = meta.ino();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let c_path = CString::new(path.as_os_str().as_bytes()).context("path contains null byte")?;
    let raw_fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if raw_fd < 0 {
        anyhow::bail!("failed to open root directory");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    Ok(RootInfo {
        path,
        ino,
        dev,
        name,
        fd,
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
        set.insert(root_info.ino);
        Arc::new(set)
    };

    let work = Arc::new(WorkQueue::new());
    work.push(vec![WorkItem {
        fd: root_info.fd,
        file_id: root_info.ino,
        parent_id: 0,
        name: root_info.name.clone(),
        path: root_info.path.display().to_string(),
    }]);

    let active_dirs: Arc<Vec<Mutex<String>>> = Arc::new((0..num_threads).map(|_| Mutex::new(String::new())).collect());

    let _handles: Vec<_> = (0..num_threads)
        .enumerate()
        .map(|(tid, _)| {
            let work = Arc::clone(&work);
            let tx = tx.clone();
            let visited = Arc::clone(&visited);
            let root_dev_i32 = root_info.dev as i32;
            let active = Arc::clone(&active_dirs);
            thread::spawn(move || {
                let mut buffer = vec![0u8; BUF_SIZE];

                while let Some(item) = work.take() {
                    *active[tid].lock().unwrap() = item.path.clone();
                    scan_directory(
                        item.fd,
                        item.file_id,
                        item.parent_id,
                        &item.name,
                        root_dev_i32,
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
    fd: OwnedFd,
    dir_file_id: u64,
    parent_id: u64,
    dir_name: &str,
    root_dev: i32,
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
        let count = unsafe {
            libc::getattrlistbulk(
                fd.as_raw_fd(),
                &SCAN_ATTRS as *const libc::attrlist as *mut libc::c_void,
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
                libc::FSOPT_NOFOLLOW as u64,
            )
        };

        if count <= 0 {
            break;
        }

        let mut offset = 0usize;
        for _ in 0..count {
            if offset + 4 > buffer.len() {
                break;
            }

            let entry_length = u32::from_ne_bytes(buffer[offset..offset + 4].try_into().unwrap()) as usize;
            if entry_length == 0 || offset + entry_length > buffer.len() {
                break;
            }

            let entry = &buffer[offset..offset + entry_length];
            if let Some(parsed) = parse_entry(entry)
                && parsed.name != "."
                && parsed.name != ".."
                && (cross_filesystems || parsed.dev_id == root_dev)
            {
                match parsed.obj_type {
                    VDIR => {
                        if let Ok(c_name) = CString::new(parsed.name) {
                            let raw_fd = unsafe {
                                libc::openat(
                                    fd.as_raw_fd(),
                                    c_name.as_ptr(),
                                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                                )
                            };
                            if raw_fd >= 0 {
                                let child_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
                                if visited.insert(parsed.file_id) {
                                    new_work.push(WorkItem {
                                        fd: child_fd,
                                        file_id: parsed.file_id,
                                        parent_id: dir_file_id,
                                        name: parsed.name.to_string(),
                                        path: if dir_path.is_empty() {
                                            String::new()
                                        } else {
                                            format!("{}/{}", dir_path.trim_end_matches('/'), parsed.name)
                                        },
                                    });
                                }
                            }
                        }
                    }
                    VREG => {
                        dir_total += parsed.file_size;
                        dir_mtime = dir_mtime.max(parsed.mtime);
                        if parsed.file_size > 0 {
                            let _ = tx.send(ScanEvent::File {
                                parent: dir_file_id,
                                name: parsed.name.to_string(),
                                size: parsed.file_size,
                                mtime: parsed.mtime,
                            });
                        }
                    }
                    _ => {}
                }
            }

            offset += entry_length;
        }
    }

    drop(fd);

    let _ = tx.send(ScanEvent::Dir {
        id: dir_file_id,
        parent: parent_id,
        name: dir_name.to_string(),
        size: dir_total,
        mtime: dir_mtime,
    });

    work.push(new_work);
}

struct ParsedEntry<'a> {
    name: &'a str,
    dev_id: i32,
    obj_type: u32,
    file_id: u64,
    file_size: u64,
    mtime: i64,
}

fn parse_entry(entry: &[u8]) -> Option<ParsedEntry<'_>> {
    let mut pos = 4;

    if pos + 20 > entry.len() {
        return None;
    }
    pos += 20;

    if pos + 8 > entry.len() {
        return None;
    }
    let name_offset = i32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?) as usize;
    let name_base = pos;
    pos += 8;

    if pos + 4 > entry.len() {
        return None;
    }
    let dev_id = i32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    pos += 4;

    if pos + 4 > entry.len() {
        return None;
    }
    let obj_type = u32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    pos += 4;

    if pos + 16 > entry.len() {
        return None;
    }
    let mtime = i64::from_ne_bytes(entry[pos..pos + 8].try_into().ok()?);
    pos += 16;

    if pos + 8 > entry.len() {
        return None;
    }
    let file_id = u64::from_ne_bytes(entry[pos..pos + 8].try_into().ok()?);
    pos += 8;

    let file_size = if obj_type == VREG && pos + 8 <= entry.len() {
        u64::from_ne_bytes(entry[pos..pos + 8].try_into().ok()?)
    } else {
        0
    };

    let name_start = name_base + name_offset;
    if name_start >= entry.len() {
        return None;
    }
    let name = CStr::from_bytes_until_nul(&entry[name_start..]).ok()?.to_str().ok()?;

    Some(ParsedEntry {
        name,
        dev_id,
        obj_type,
        file_id,
        file_size,
        mtime,
    })
}
