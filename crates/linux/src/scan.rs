use std::ffi::CStr;
use std::os::unix::io::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use rsdirstat_core::scan::{DirJob, DirSink, ScanEvent, ScanOutcome, node_id, open_dir, open_dir_at, run_scan};

const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;

const STATX_TYPE: u32 = 0x0001;
const STATX_MODE: u32 = 0x0002;
const STATX_NLINK: u32 = 0x0004;
const STATX_SIZE: u32 = 0x0200;
const STATX_MTIME: u32 = 0x0040;
const STATX_MNT_ID: u32 = 0x1000;
/// What a file's entry needs: its size, its mtime, and its link count (for hardlink dedup).
const FILE_STATX_MASK: u32 = STATX_SIZE | STATX_MTIME | STATX_NLINK;
const AT_STATX_DONT_SYNC: libc::c_int = 0x4000;

/// Combine the statx device major/minor into a single value for node identity. This need not match
/// the kernel's `dev_t` encoding — only be consistent within a scan.
fn makedev(major: u32, minor: u32) -> u64 {
    ((major as u64) << 32) | minor as u64
}

#[repr(C)]
#[derive(Default)]
struct StatxTimestamp {
    tv_sec: i64,
    tv_nsec: u32,
    _pad: i32,
}

#[repr(C)]
#[derive(Default)]
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
    let mut buf = Statx::default();
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
    let mut buf = Statx::default();
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

pub fn scan(root: &Path, cross_filesystems: bool, tx: std::sync::mpsc::Sender<ScanEvent>) -> Result<ScanOutcome> {
    scan_cancellable(root, cross_filesystems, tx, Arc::default())
}

/// Scan, aborting promptly if `cancel` is set (the server sets it when it shuts down mid-scan).
pub fn scan_cancellable(
    root: &Path,
    cross_filesystems: bool,
    tx: std::sync::mpsc::Sender<ScanEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<ScanOutcome> {
    let path = std::fs::canonicalize(root).context("failed to canonicalize root path")?;
    let fd = open_dir(&path).context("failed to open root directory")?;
    let sx = statx_fd(fd.as_raw_fd(), STATX_MNT_ID).context("failed to statx root directory")?;
    let id = node_id(makedev(sx.stx_dev_major, sx.stx_dev_minor), sx.stx_ino);
    let root_mnt_id = sx.stx_mnt_id;
    Ok(run_scan(&path, fd, id, tx, cancel, move |dir, buffer, sink| {
        read_dir(dir, buffer, sink, root_mnt_id, cross_filesystems)
    }))
}

/// Read one directory with raw `getdents64`, then `statx` each file for its size and mtime, and
/// each subdirectory for its device and mount.
fn read_dir(
    dir: &DirJob<OwnedFd>,
    buffer: &mut [u8],
    sink: &mut DirSink<'_, OwnedFd>,
    root_mnt_id: u64,
    cross_filesystems: bool,
) {
    let dir_fd = dir.handle.as_raw_fd();
    loop {
        let nread = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                dir_fd,
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
            )
        };

        if nread < 0 {
            eprintln!(
                "getdents64 failed for {}: {}",
                dir.name,
                std::io::Error::last_os_error()
            );
            break;
        }
        if nread == 0 {
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
            let record = &buffer[offset..offset + d_reclen];
            offset += d_reclen;

            let Ok(name) = CStr::from_bytes_until_nul(&record[19..]) else {
                continue;
            };
            let name_str = match name.to_str() {
                Ok(s) if s != "." && s != ".." => s,
                _ => continue,
            };

            // Some filesystems leave the type unknown (DT_UNKNOWN). Resolve it with a statx that
            // also asks for everything a file needs, so a file costs one statx either way.
            let (entry_type, file_stat) = if d_type == 0 {
                let Some(sx) = statx_path(dir_fd, name.as_ptr(), STATX_TYPE | STATX_MODE | FILE_STATX_MASK) else {
                    continue;
                };
                match sx.stx_mode as u32 & libc::S_IFMT {
                    libc::S_IFDIR => (DT_DIR, None),
                    libc::S_IFREG => (DT_REG, Some(sx)),
                    _ => continue,
                }
            } else {
                (d_type, None)
            };

            match entry_type {
                DT_DIR => {
                    let Some(child) = open_dir_at(&dir.handle, name) else {
                        continue;
                    };
                    // statx the child for its device (node identity) and mount id (the
                    // filesystem-boundary check). stx_dev is always populated; stx_mnt_id needs
                    // the mask.
                    let sx = statx_fd(child.as_raw_fd(), STATX_MNT_ID);
                    if !cross_filesystems && sx.as_ref().is_none_or(|s| s.stx_mnt_id != root_mnt_id) {
                        continue;
                    }
                    let child_dev = sx.as_ref().map_or(0, |s| makedev(s.stx_dev_major, s.stx_dev_minor));
                    sink.dir(node_id(child_dev, d_ino), name_str, sink.child_path(name_str), child);
                }
                DT_REG => {
                    if let Some(sx) = file_stat.or_else(|| statx_path(dir_fd, name.as_ptr(), FILE_STATX_MASK)) {
                        // Hardlink identity: the entry's inode (d_ino) with the file's device.
                        let hardlink =
                            (sx.stx_nlink > 1).then(|| node_id(makedev(sx.stx_dev_major, sx.stx_dev_minor), d_ino));
                        sink.file(name_str, sx.stx_size, sx.stx_mtime.tv_sec, hardlink);
                    }
                }
                _ => {}
            }
        }

        if !sink.checkpoint() {
            break;
        }
    }
}
