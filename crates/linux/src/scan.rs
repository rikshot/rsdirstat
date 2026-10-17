use std::collections::HashSet;
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

/// What a file's entry needs: its size, its mtime, and its link count and inode (for hardlink
/// dedup).
const FILE_STATX_MASK: u32 = STATX_SIZE | STATX_MTIME | STATX_NLINK | STATX_INO;
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

/// Whether `sx` is a directory where something is mounted when it's looked into, but isn't yet: an
/// NFS export crossing into another of the server's filesystems, an SMB DFS link, a FUSE submount.
/// It is reported on the filesystem it is on, so it would pass for the root's; opening it would
/// mount the other one.
fn is_automount_point(sx: &Statx) -> bool {
    sx.stx_attributes & libc::STATX_ATTR_AUTOMOUNT as u64 != 0
}

/// `statx` the entry `name` of `dirfd`, from cached attributes where the filesystem keeps them.
/// Through the raw syscall, which unlike glibc's wrapper needs no glibc 2.28.
fn statx(dirfd: i32, name: &CStr, flags: libc::c_int, mask: u32) -> std::io::Result<Statx> {
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
    if ret == 0 {
        Ok(buf)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `statx` the entry `name` of `dirfd`. A directory that is a mount point reports the mounted
/// filesystem's root, without triggering an automount.
fn statx_path(dirfd: i32, name: &CStr, mask: u32) -> std::io::Result<Statx> {
    statx(dirfd, name, libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT, mask)
}

/// `statx` the open file `fd`.
fn statx_fd(fd: i32, mask: u32) -> std::io::Result<Statx> {
    statx(fd, c"", libc::AT_EMPTY_PATH, mask)
}

/// The kernel's pseudo filesystems, by type. Their files take no disk space, and the sizes they
/// report are no data on any disk: /proc/kcore's is the kernel's address space (128 TiB on x86-64),
/// every sysfs attribute's a page, and a PCI device's `resource` files' the device's memory windows.
const PSEUDO_FILESYSTEMS: [&str; 15] = [
    "proc",
    "sysfs",
    "debugfs",
    "tracefs",
    "securityfs",
    "cgroup",
    "cgroup2",
    "bpf",
    "pstore",
    "configfs",
    "efivarfs",
    "binfmt_misc",
    "selinuxfs",
    "smackfs",
    "resctrl",
];

/// The mounts of pseudo filesystems, as `filesystem` tells mounts apart: by mount id, and by device
/// for kernels without one.
#[derive(Default)]
struct PseudoMounts {
    ids: HashSet<u64>,
    devices: HashSet<u64>,
}

impl PseudoMounts {
    /// The pseudo filesystems mounted now, from /proc/self/mountinfo: by name, so asking costs no
    /// call into any mounted filesystem, which on a network share whose server is gone would block.
    fn mounted() -> Self {
        let mut mounts = Self::default();
        let Ok(table) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return mounts;
        };
        for line in table.lines() {
            // `36 35 0:5 / /proc rw,nosuid shared:13 - proc proc rw`: the id, the parent's, the
            // device, ..., optional fields, then `-` and the filesystem type.
            let fields: Vec<&str> = line.split(' ').collect();
            let Some(dash) = fields.iter().position(|&field| field == "-") else {
                continue;
            };
            if !fields
                .get(dash + 1)
                .is_some_and(|kind| PSEUDO_FILESYSTEMS.contains(kind))
            {
                continue;
            }
            if let Some(id) = fields.first().and_then(|id| id.parse().ok()) {
                mounts.ids.insert(id);
            }
            if let Some((major, minor)) = fields.get(2).and_then(|device| device.split_once(':'))
                && let (Ok(major), Ok(minor)) = (major.parse(), minor.parse())
            {
                mounts.devices.insert(libc::makedev(major, minor));
            }
        }
        mounts
    }

    /// Whether `sx` is on one of them.
    fn holds(&self, sx: &Statx) -> bool {
        if sx.stx_mask & STATX_MNT_ID != 0 {
            self.ids.contains(&sx.stx_mnt_id)
        } else {
            self.devices.contains(&device(sx))
        }
    }
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
    let path = std::fs::canonicalize(root).with_context(|| format!("can't scan {}", root.display()))?;
    let fd = open_root(&path).with_context(|| format!("can't open {}", path.display()))?;
    let sx = statx_fd(fd.as_raw_fd(), DIR_STATX_MASK).with_context(|| format!("can't statx {}", path.display()))?;
    let root_filesystem = filesystem(&sx);
    // Only crossing into other filesystems can lead into one.
    let pseudo = if options.cross_filesystems {
        PseudoMounts::mounted()
    } else {
        PseudoMounts::default()
    };
    let root = UnixDir::Root(Arc::new(fd));
    run_scan(
        Started::new(path, tx),
        root,
        dir_id(&sx),
        cancel,
        options
            .threads
            .map_or_else(|| cpu_count() * THREADS_PER_CPU, NonZeroUsize::get),
        move |dir, buffer, sink| read_dir(dir, buffer, sink, root_filesystem, options.cross_filesystems, &pseudo),
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
    pseudo: &PseudoMounts,
) -> bool {
    let Some(fd) = dir.open(sink) else { return false };
    let dir_fd = fd.as_raw_fd();
    // An entry that couldn't be stat'ed for a reason other than its being gone, reported once for
    // the directory: one that can be listed but not searched fails them all.
    let mut entry_failed = false;
    let mut stat_failed = |sink: &DirSink<'_, UnixDir>, name: &CStr, error: std::io::Error| {
        if error.raw_os_error() != Some(libc::ENOENT) && !entry_failed {
            entry_failed = true;
            sink.read_failed(format_args!("statx {}: {error}", name.to_string_lossy()));
        }
    };
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
            let error = std::io::Error::last_os_error();
            // ENOENT: the directory was removed while it was read, so it ends here.
            if error.raw_os_error() != Some(libc::ENOENT) {
                sink.read_failed(format_args!("getdents64: {error}"));
            }
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
                let sx = match statx_path(dir_fd, name, mask) {
                    Ok(sx) => sx,
                    Err(error) => {
                        stat_failed(sink, name, error);
                        continue;
                    }
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
                    let sx = match stat.map_or_else(|| statx_path(dir_fd, name, DIR_STATX_MASK), Ok) {
                        Ok(sx) => sx,
                        // A folder that can't be looked at, like one that can't be opened, is left out
                        // quietly: no permission (another user's FUSE mount refuses even root), or a
                        // FUSE filesystem whose daemon is gone.
                        Err(error)
                            if matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM | libc::ENOTCONN)) =>
                        {
                            continue;
                        }
                        Err(error) => {
                            stat_failed(sink, name, error);
                            continue;
                        }
                    };
                    if cross_filesystems {
                        // Even across filesystems, not into the kernel's pseudo ones (/proc, /sys, ...).
                        if filesystem(&sx) != root_filesystem && pseudo.holds(&sx) {
                            continue;
                        }
                    } else if filesystem(&sx) != root_filesystem || is_automount_point(&sx) {
                        continue;
                    }
                    let child = UnixDir::Child(Arc::clone(&fd), name.to_owned());
                    sink.dir(dir_id(&sx), &display_name, child);
                }
                DT_REG => match stat.map_or_else(|| statx_path(dir_fd, name, FILE_STATX_MASK), Ok) {
                    Ok(sx) => {
                        // Hardlink identity: the file's own inode with its device, as `du` uses. Not
                        // the entry's `d_ino`, which a FUSE filesystem may leave the same unknown
                        // value for every entry, collapsing every hardlinked file into one.
                        let hardlink = (sx.stx_nlink > 1).then(|| node_id(device(&sx), sx.stx_ino));
                        sink.file(display_name, sx.stx_size, sx.stx_mtime.tv_sec, hardlink);
                    }
                    Err(error) => stat_failed(sink, name, error),
                },
                _ => {}
            }
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

    /// /proc is a pseudo filesystem; the root's isn't.
    #[test]
    fn pseudo_filesystems_are_known_by_their_mounts() {
        let pseudo = PseudoMounts::mounted();
        let stat = |path: &CStr| statx(libc::AT_FDCWD, path, 0, DIR_STATX_MASK).unwrap();
        assert!(pseudo.holds(&stat(c"/proc")));
        assert!(!pseudo.holds(&stat(c"/")));
    }
}
