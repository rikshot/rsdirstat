use std::ffi::CStr;
use std::num::NonZeroUsize;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use libc::{
    AT_STATX_DONT_SYNC, DT_DIR, DT_REG, DT_UNKNOWN, STATX_INO, STATX_MNT_ID, STATX_MODE, STATX_MTIME, STATX_NLINK,
    STATX_SIZE, STATX_TYPE, statx as Statx,
};
use rsdirstat_core::scan::{
    DirJob, DirSink, ScanEvent, ScanOptions, Started, UnixDir, cpu_count, node_id, open_root, run_scan,
};

/// What a file's entry needs: its size, its mtime, and its link count (for hardlink dedup).
const FILE_STATX_MASK: u32 = STATX_SIZE | STATX_MTIME | STATX_NLINK;
/// What a directory's entry needs: its identity (with the device, always reported) and its mount.
const DIR_STATX_MASK: u32 = STATX_INO | STATX_MNT_ID;

/// The device an entry is on.
fn device(sx: &Statx) -> u64 {
    libc::makedev(sx.stx_dev_major, sx.stx_dev_minor)
}

/// A directory's node id: its inode with its device. From the directory's own `statx` rather than
/// the entry's `d_ino`, which for a mount point is the directory it covers, so a bind mount gets
/// the same id as the directory it shows and is read only once.
fn dir_id(sx: &Statx) -> u64 {
    node_id(device(sx), sx.stx_ino)
}

/// Which mounted filesystem an entry is on, for staying on the root's: its mount id, or, on kernels
/// before 5.8, which don't report one, its device (as `du -x` compares, so there a btrfs subvolume
/// counts as another filesystem).
fn filesystem(sx: &Statx) -> u64 {
    if sx.stx_mask & STATX_MNT_ID != 0 {
        sx.stx_mnt_id
    } else {
        device(sx)
    }
}

/// `statx` the entry `name` of `dirfd`, from cached attributes where the filesystem keeps them.
/// Through the raw syscall, which unlike glibc's wrapper needs no glibc 2.28.
fn statx(dirfd: i32, name: &CStr, flags: libc::c_int, mask: u32) -> Option<Statx> {
    let mut buf: Statx = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::syscall(
            libc::SYS_statx,
            dirfd,
            name.as_ptr(),
            flags | AT_STATX_DONT_SYNC,
            mask,
            &mut buf as *mut Statx,
        )
    };
    if ret == 0 { Some(buf) } else { None }
}

/// `statx` the entry `name` of `dirfd`. A directory that is a mount point reports the mounted
/// filesystem's root, without triggering an automount.
fn statx_path(dirfd: i32, name: &CStr, mask: u32) -> Option<Statx> {
    statx(dirfd, name, libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT, mask)
}

/// `statx` the open file `fd`.
fn statx_fd(fd: i32, mask: u32) -> Option<Statx> {
    statx(fd, c"", libc::AT_EMPTY_PATH, mask)
}

/// Worker threads per CPU by default. A scan of files not yet cached waits on the disk, and more
/// requests in flight hide that: on a Ryzen 9 5900HS with an NVMe drive, the ext4 root scans cold
/// in 0.39 s with 32 threads against 0.58 s with 16, while warm 32 cost 0.02 s (0.14 s, 0.12 s).
const THREADS_PER_CPU: usize = 2;

/// Scan `root`, stopping promptly once `cancel` is set (the server sets it when it shuts down
/// mid-scan).
pub fn scan(
    root: &Path,
    options: ScanOptions,
    tx: std::sync::mpsc::Sender<ScanEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let path = std::fs::canonicalize(root).context("failed to canonicalize root path")?;
    let fd = open_root(&path).context("failed to open root directory")?;
    let sx = statx_fd(fd.as_raw_fd(), DIR_STATX_MASK).context("failed to statx root directory")?;
    let root_filesystem = filesystem(&sx);
    let root = UnixDir::Root(Arc::new(fd));
    run_scan(
        Started::new(path, tx),
        root,
        dir_id(&sx),
        cancel,
        options
            .threads
            .map_or_else(|| cpu_count() * THREADS_PER_CPU, NonZeroUsize::get),
        move |dir, buffer, sink| read_dir(dir, buffer, sink, root_filesystem, options.cross_filesystems),
    );
    Ok(())
}

/// Read one directory with raw `getdents64`, then `statx` each file for its size and mtime, and
/// each subdirectory for its identity and mount. False if it can't be opened.
fn read_dir(
    dir: &DirJob<UnixDir>,
    buffer: &mut [u8],
    sink: &mut DirSink<'_, UnixDir>,
    root_filesystem: u64,
    cross_filesystems: bool,
) -> bool {
    let Some(fd) = dir.open() else { return false };
    let dir_fd = fd.as_raw_fd();
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
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            // Linux names are bytes, not necessarily UTF-8 (an old Latin-1 archive, say). Such an
            // entry is still counted, under a lossy name for display; it's opened by its real one.
            let display_name = name.to_string_lossy();

            // Some filesystems leave the type unknown (DT_UNKNOWN). Resolve it with a statx that
            // also asks for everything a file or directory needs, so either costs one statx.
            let (entry_type, stat) = if d_type == DT_UNKNOWN {
                let mask = STATX_TYPE | STATX_MODE | FILE_STATX_MASK | DIR_STATX_MASK;
                let Some(sx) = statx_path(dir_fd, name, mask) else {
                    continue;
                };
                match sx.stx_mode as u32 & libc::S_IFMT {
                    libc::S_IFDIR => (DT_DIR, Some(sx)),
                    libc::S_IFREG => (DT_REG, Some(sx)),
                    _ => continue,
                }
            } else {
                (d_type, None)
            };

            match entry_type {
                DT_DIR => {
                    // By name, so a mount point outside the root's filesystem is skipped without
                    // being opened. The directory is opened only when a worker gets to it.
                    let Some(sx) = stat.or_else(|| statx_path(dir_fd, name, DIR_STATX_MASK)) else {
                        continue;
                    };
                    if !cross_filesystems && filesystem(&sx) != root_filesystem {
                        continue;
                    }
                    let child = UnixDir::Child(Arc::clone(&fd), name.to_owned());
                    sink.dir(dir_id(&sx), &display_name, child);
                }
                DT_REG => {
                    if let Some(sx) = stat.or_else(|| statx_path(dir_fd, name, FILE_STATX_MASK)) {
                        // Hardlink identity: the entry's inode (d_ino) with the file's device.
                        let hardlink = (sx.stx_nlink > 1).then(|| node_id(device(&sx), d_ino));
                        sink.file(display_name, sx.stx_size, sx.stx_mtime.tv_sec, hardlink);
                    }
                }
                _ => {}
            }
        }

        if !sink.checkpoint() {
            break;
        }
    }
    true
}
