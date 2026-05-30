use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use dashmap::DashSet;
use rsdirstat_core::scan::{WorkQueue, raise_fd_limit};
use rsdirstat_protocol::ScanEvent;

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
    // DATALENGTH (logical data-fork length), not TOTALSIZE: the latter adds resource-fork bytes,
    // which Linux (stx_size) and Windows (EndOfFile) don't count, so the same tree would total
    // differently per platform. DATALENGTH matches their apparent-size semantics.
    fileattr: libc::ATTR_FILE_DATALENGTH,
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

        if count < 0 {
            eprintln!(
                "getattrlistbulk failed for {dir_name}: {}",
                std::io::Error::last_os_error()
            );
            break;
        }
        if count == 0 {
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

/// Parse one `getattrlistbulk` entry. The layout is dynamic: a leading `attribute_set_t`
/// (ATTR_CMN_RETURNED_ATTRS) reports which requested attributes were actually returned, and each
/// attribute's bytes are present only if its bit is set, in ascending bit order per group. We must
/// honor that bitmap rather than assume every field is present — a filesystem that omits one would
/// otherwise shift every later field and corrupt the parse.
// The final `field!` advances `pos` past the last attribute even though nothing reads it after —
// the uniform macro is clearer than special-casing the tail.
#[allow(unused_assignments)]
fn parse_entry(entry: &[u8]) -> Option<ParsedEntry<'_>> {
    // entry[0..4] is the u32 entry length (used by the caller for framing); skip it.
    let mut pos = 4;

    // ATTR_CMN_RETURNED_ATTRS: attribute_set_t = 5 × u32 (common, vol, dir, file, fork bitmaps).
    if pos + 20 > entry.len() {
        return None;
    }
    let returned_common = u32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    let returned_file = u32::from_ne_bytes(entry[pos + 12..pos + 16].try_into().ok()?);
    pos += 20;

    let mut name = "";
    let mut dev_id = 0i32;
    let mut obj_type = 0u32;
    let mut file_id = 0u64;
    let mut mtime = 0i64;
    let mut file_size = 0u64;

    // Read a fixed-width field only if its bit is set, advancing `pos` past the bytes it occupies.
    macro_rules! field {
        ($present:expr, $width:expr, $read:expr) => {
            if $present {
                if pos + $width > entry.len() {
                    return None;
                }
                $read;
                pos += $width;
            }
        };
    }

    // ATTR_CMN_NAME: attrreference_t (i32 data offset relative to its own start + u32 length).
    field!(returned_common & libc::ATTR_CMN_NAME != 0, 8, {
        let name_offset = i32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?) as usize;
        let name_start = pos + name_offset;
        if name_start >= entry.len() {
            return None;
        }
        name = CStr::from_bytes_until_nul(&entry[name_start..]).ok()?.to_str().ok()?;
    });
    field!(returned_common & libc::ATTR_CMN_DEVID != 0, 4, {
        dev_id = i32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    });
    field!(returned_common & libc::ATTR_CMN_OBJTYPE != 0, 4, {
        obj_type = u32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    });
    // ATTR_CMN_MODTIME: timespec (16 bytes); we only need tv_sec from the first 8.
    field!(returned_common & libc::ATTR_CMN_MODTIME != 0, 16, {
        mtime = i64::from_ne_bytes(entry[pos..pos + 8].try_into().ok()?);
    });
    field!(returned_common & libc::ATTR_CMN_FILEID != 0, 8, {
        file_id = u64::from_ne_bytes(entry[pos..pos + 8].try_into().ok()?);
    });
    // File attributes only appear for files, so this bit is naturally clear for directories.
    field!(returned_file & libc::ATTR_FILE_DATALENGTH != 0, 8, {
        file_size = u64::from_ne_bytes(entry[pos..pos + 8].try_into().ok()?);
    });

    Some(ParsedEntry {
        name,
        dev_id,
        obj_type,
        file_id,
        file_size,
        mtime,
    })
}
