use std::path::Path;

use anyhow::Result;
use rsdirstat_core::protocol::ScanEvent;

pub fn scan(root: &Path, cross_filesystems: bool, tx: std::sync::mpsc::Sender<ScanEvent>) -> Result<()> {
    imp::scan(root, cross_filesystems, tx)
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    pub fn scan(_root: &Path, _cross_filesystems: bool, _tx: std::sync::mpsc::Sender<ScanEvent>) -> Result<()> {
        anyhow::bail!("Linux scanner not available on this platform")
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use anyhow::{Context, Result};
    use dashmap::DashSet;
    use rsdirstat_core::protocol::ScanEvent;
    use rsdirstat_core::scan::{WorkQueue, raise_fd_limit};

    const DT_DIR: u8 = 4;
    const DT_REG: u8 = 8;

    const BUF_SIZE: usize = 1024 * 1024;

    const STATX_TYPE: u32 = 0x0001;
    const STATX_MODE: u32 = 0x0002;
    const STATX_SIZE: u32 = 0x0200;
    const STATX_MTIME: u32 = 0x0040;
    const STATX_MNT_ID: u32 = 0x1000;
    const AT_STATX_DONT_SYNC: libc::c_int = 0x4000;

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
        let sx = statx_fd(fd.as_raw_fd(), STATX_MNT_ID)
            .context("failed to statx root directory")?;
        Ok(RootInfo { path, ino: sx.stx_ino, mnt_id: sx.stx_mnt_id, name, fd })
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

        let active_dirs: Arc<Vec<Mutex<String>>> =
            Arc::new((0..num_threads).map(|_| Mutex::new(String::new())).collect());
        let root_mnt_id = root_info.mnt_id;

        let _handles: Vec<_> = (0..num_threads)
            .enumerate()
            .map(|(tid, _)| {
                let work = Arc::clone(&work);
                let tx = tx.clone();
                let visited = Arc::clone(&visited);
                let active = Arc::clone(&active_dirs);
                thread::spawn(move || {
                    let mut buffer = vec![0u8; BUF_SIZE];

                    while let Some(guard) = work.take() {
                        let item = guard.into_inner();
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
                            &item.path,
                        );
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
                let d_reclen =
                    u16::from_ne_bytes(buffer[offset + 16..offset + 18].try_into().unwrap()) as usize;
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

                            if !cross_filesystems {
                                if let Some(sx) = statx_fd(child_fd.as_raw_fd(), STATX_MNT_ID) {
                                    if sx.stx_mnt_id != root_mnt_id {
                                        offset += d_reclen;
                                        continue;
                                    }
                                } else {
                                    offset += d_reclen;
                                    continue;
                                }
                            }

                            if visited.insert(d_ino) {
                                new_work.push(WorkItem {
                                    fd: child_fd,
                                    file_id: d_ino,
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
                        if let Some(sx) = statx_path(fd.as_raw_fd(), name.as_ptr(), STATX_SIZE | STATX_MTIME) {
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
}
