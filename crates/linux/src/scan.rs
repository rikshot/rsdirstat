use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use dashmap::DashSet;
use rsdirstat_core::scan::{WorkQueue, node_id, raise_fd_limit};
use rsdirstat_protocol::ScanEvent;

const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;

const BUF_SIZE: usize = 1024 * 1024;

const STATX_TYPE: u32 = 0x0001;
const STATX_MODE: u32 = 0x0002;
const STATX_NLINK: u32 = 0x0004;
const STATX_SIZE: u32 = 0x0200;
const STATX_MTIME: u32 = 0x0040;
const STATX_MNT_ID: u32 = 0x1000;
const AT_STATX_DONT_SYNC: libc::c_int = 0x4000;

/// Combine the statx device major/minor into a single value for node identity. This need not match
/// the kernel's `dev_t` encoding — only be consistent within a scan.
fn makedev(major: u32, minor: u32) -> u64 {
    ((major as u64) << 32) | minor as u64
}

#[repr(C)]
struct StatxTimestamp {
    tv_sec: i64,
    tv_nsec: u32,
    _pad: i32,
}

#[repr(C)]
struct Statx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    _spare0: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    stx_atime: StatxTimestamp,
    stx_btime: StatxTimestamp,
    stx_ctime: StatxTimestamp,
    stx_mtime: StatxTimestamp,
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
    stx_mnt_id: u64,
    stx_dio_mem_align: u32,
    stx_dio_offset_align: u32,
    _spare3: [u64; 12],
}

fn statx_path(dirfd: i32, name: *const libc::c_char, mask: u32) -> Option<Statx> {
    let mut buf: Statx = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::syscall(
            libc::SYS_statx,
            dirfd,
            name,
            libc::AT_SYMLINK_NOFOLLOW | AT_STATX_DONT_SYNC,
            mask,
            &mut buf as *mut Statx,
        )
    };
    if ret == 0 { Some(buf) } else { None }
}

fn statx_fd(fd: i32, mask: u32) -> Option<Statx> {
    let mut buf: Statx = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::syscall(
            libc::SYS_statx,
            fd,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | AT_STATX_DONT_SYNC,
            mask,
            &mut buf as *mut Statx,
        )
    };
    if ret == 0 { Some(buf) } else { None }
}

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
    mnt_id: u64,
    name: String,
    fd: OwnedFd,
}

fn open_root(root: &Path) -> Result<RootInfo> {
    raise_fd_limit();
    let path = std::fs::canonicalize(root).context("failed to canonicalize root path")?;
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
    let sx = statx_fd(fd.as_raw_fd(), STATX_MNT_ID).context("failed to statx root directory")?;
    Ok(RootInfo {
        path,
        ino: sx.stx_ino,
        dev: makedev(sx.stx_dev_major, sx.stx_dev_minor),
        mnt_id: sx.stx_mnt_id,
        name,
        fd,
    })
}

pub fn scan(root: &Path, cross_filesystems: bool, tx: std::sync::mpsc::Sender<ScanEvent>) -> Result<()> {
    scan_cancellable(root, cross_filesystems, tx, Arc::new(AtomicBool::new(false)))
}

/// Scan, aborting promptly if `cancel` is set. The server trips this when a scan is superseded by a
/// rescan or a new `ScanPath` so the old worker threads stop walking the filesystem.
pub fn scan_cancellable(
    root: &Path,
    cross_filesystems: bool,
    tx: std::sync::mpsc::Sender<ScanEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let root_info = open_root(root)?;

    let _ = tx.send(ScanEvent::ScanStart {
        path: root_info.name.clone(),
    });

    let num_threads = thread::available_parallelism().map(|n| n.get()).unwrap_or(4);

    let root_id = node_id(root_info.dev, root_info.ino);
    let visited = {
        let set = DashSet::new();
        set.insert(root_id);
        Arc::new(set)
    };
    // Hardlinked files (nlink > 1) are deduped across the whole scan by their (dev, ino) identity.
    let visited_files: Arc<DashSet<u64>> = Arc::new(DashSet::new());

    let work = Arc::new(WorkQueue::new());
    work.push(vec![WorkItem {
        fd: root_info.fd,
        file_id: root_id,
        parent_id: 0,
        name: root_info.name.clone(),
        path: root_info.path.display().to_string(),
    }]);

    let active_dirs: Arc<Vec<Mutex<String>>> = Arc::new((0..num_threads).map(|_| Mutex::new(String::new())).collect());
    let root_mnt_id = root_info.mnt_id;

    let _handles: Vec<_> = (0..num_threads)
        .enumerate()
        .map(|(tid, _)| {
            let work = Arc::clone(&work);
            let tx = tx.clone();
            let visited = Arc::clone(&visited);
            let visited_files = Arc::clone(&visited_files);
            let active = Arc::clone(&active_dirs);
            let cancel = Arc::clone(&cancel);
            thread::spawn(move || {
                let mut buffer = vec![0u8; BUF_SIZE];

                while let Some(item) = work.take() {
                    if cancel.load(Ordering::Relaxed) {
                        work.cancel(); // unwind every worker: clears the queue and wakes blocked takers
                        break;
                    }
                    *active[tid].lock().unwrap() = item.path.clone();
                    scan_directory(
                        item.fd,
                        item.file_id,
                        item.parent_id,
                        &item.name,
                        root_mnt_id,
                        cross_filesystems,
                        &mut buffer,
                        &work,
                        &tx,
                        &visited,
                        &visited_files,
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
    root_mnt_id: u64,
    cross_filesystems: bool,
    buffer: &mut [u8],
    work: &WorkQueue<WorkItem>,
    tx: &std::sync::mpsc::Sender<ScanEvent>,
    visited: &DashSet<u64>,
    visited_files: &DashSet<u64>,
    dir_path: &str,
) {
    let mut new_work = Vec::new();
    let mut dir_total: u64 = 0;
    let mut dir_mtime: i64 = 0;

    loop {
        let nread = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                fd.as_raw_fd(),
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
            )
        };

        if nread <= 0 {
            break;
        }

        let nread = nread as usize;
        let mut offset = 0usize;

        while offset < nread {
            if offset + 20 > nread {
                break;
            }

            // linux_dirent64: d_ino(8) + d_off(8) + d_reclen(2) + d_type(1) + d_name(...)
            let d_ino = u64::from_ne_bytes(buffer[offset..offset + 8].try_into().unwrap());
            let d_reclen = u16::from_ne_bytes(buffer[offset + 16..offset + 18].try_into().unwrap()) as usize;
            let d_type = buffer[offset + 18];

            if d_reclen == 0 || offset + d_reclen > nread {
                break;
            }

            let name = match CStr::from_bytes_until_nul(&buffer[offset + 19..offset + d_reclen]) {
                Ok(cstr) => cstr,
                Err(_) => {
                    offset += d_reclen;
                    continue;
                }
            };

            let name_str = match name.to_str() {
                Ok(s) if s != "." && s != ".." => s,
                _ => {
                    offset += d_reclen;
                    continue;
                }
            };

            // Resolve DT_UNKNOWN via statx
            let entry_type = if d_type == 0 {
                match statx_path(fd.as_raw_fd(), name.as_ptr(), STATX_TYPE | STATX_MODE) {
                    Some(sx) => match sx.stx_mode as u32 & libc::S_IFMT {
                        libc::S_IFDIR => DT_DIR,
                        libc::S_IFREG => DT_REG,
                        _ => 0,
                    },
                    None => {
                        offset += d_reclen;
                        continue;
                    }
                }
            } else {
                d_type
            };

            match entry_type {
                DT_DIR => {
                    let raw_fd = unsafe {
                        libc::openat(
                            fd.as_raw_fd(),
                            name.as_ptr(),
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                        )
                    };
                    if raw_fd >= 0 {
                        let child_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

                        // statx the child for its device (node identity) and mount id (the
                        // filesystem-boundary check). stx_dev is always populated; stx_mnt_id needs
                        // the mask.
                        let sx = statx_fd(child_fd.as_raw_fd(), STATX_MNT_ID);
                        if !cross_filesystems {
                            match &sx {
                                Some(s) if s.stx_mnt_id == root_mnt_id => {}
                                _ => {
                                    offset += d_reclen;
                                    continue;
                                }
                            }
                        }

                        let child_dev = sx.as_ref().map_or(0, |s| makedev(s.stx_dev_major, s.stx_dev_minor));
                        let child_id = node_id(child_dev, d_ino);
                        if visited.insert(child_id) {
                            new_work.push(WorkItem {
                                fd: child_fd,
                                file_id: child_id,
                                parent_id: dir_file_id,
                                name: name_str.to_string(),
                                path: if dir_path.is_empty() {
                                    String::new()
                                } else {
                                    format!("{}/{}", dir_path.trim_end_matches('/'), name_str)
                                },
                            });
                        }
                    }
                }
                DT_REG => {
                    if let Some(sx) = statx_path(fd.as_raw_fd(), name.as_ptr(), STATX_SIZE | STATX_MTIME | STATX_NLINK)
                    {
                        // Skip hardlinks already counted under another name (matches `du`). Use the
                        // entry's inode (d_ino) with the file's device for identity.
                        let dev = makedev(sx.stx_dev_major, sx.stx_dev_minor);
                        let counted = sx.stx_nlink <= 1 || visited_files.insert(node_id(dev, d_ino));
                        if counted {
                            let file_size = sx.stx_size;
                            let mtime = sx.stx_mtime.tv_sec;
                            dir_total += file_size;
                            dir_mtime = dir_mtime.max(mtime);
                            if file_size > 0 {
                                let _ = tx.send(ScanEvent::File {
                                    parent: dir_file_id,
                                    name: name_str.to_string(),
                                    size: file_size,
                                    mtime,
                                });
                            }
                        }
                    }
                }
                _ => {}
            }

            offset += d_reclen;
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
