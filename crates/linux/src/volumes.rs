use std::collections::BTreeSet;
use std::ffi::CString;
use std::path::Path;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use rsdirstat_core::volume::VolumeInfo;

use crate::mountinfo::Mount;

/// How long the picker waits for the mounts to report their sizes. One of a network share whose
/// server is gone can take minutes to fail, or never on a hard mount, and would hold up the list.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The filesystems of this machine's own disks, whose sizes are asked for inline: their `statvfs`
/// answers without waiting on anyone. Any other (a network share; FUSE, whose daemon answers) may
/// not, so it is asked on a thread of its own.
const LOCAL_FILESYSTEMS: [&str; 19] = [
    "ext2", "ext3", "ext4", "xfs", "btrfs", "bcachefs", "zfs", "f2fs", "jfs", "reiserfs", "vfat", "exfat", "ntfs3",
    "hfsplus", "iso9660", "udf", "squashfs", "erofs", "overlay",
];

/// The mount points whose probe hasn't returned yet. Another would only be one more thread stuck on
/// the same dead mount, every time the picker is shown.
static PROBING: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

pub fn list_volumes() -> Vec<VolumeInfo> {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };

    // A mount that may not answer is asked on a thread of its own, all of them at once, so a stuck one
    // holds up neither the others nor the list: it is left out, and its thread left to the syscall.
    // One still stuck from an earlier list is left out at once.
    let probes: Vec<_> = mounts
        .lines()
        .filter_map(|line| {
            let mount = Mount::parse(line)?;
            if !is_volume(&mount) {
                return None;
            }
            let mount_point = unescape_octal(mount.mount_point);
            let fs_type = mount.fs_type;

            let (tx, rx) = mpsc::channel();
            if LOCAL_FILESYSTEMS.contains(&fs_type) {
                let _ = tx.send(usage(&mount_point));
            } else if PROBING.lock().unwrap().insert(mount_point.clone()) {
                let path = mount_point.clone();
                std::thread::spawn(move || {
                    let usage = usage(&path);
                    PROBING.lock().unwrap().remove(&path);
                    let _ = tx.send(usage);
                });
            }
            Some((mount_point, fs_type.to_string(), rx))
        })
        .collect();

    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut volumes: Vec<VolumeInfo> = probes
        .into_iter()
        .filter_map(|(mount_point, fs_type, rx)| {
            let (total, free) = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .ok()??;
            let name = if mount_point == "/" {
                "Root".to_string()
            } else {
                Path::new(&mount_point)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&mount_point)
                    .to_string()
            };
            Some(VolumeInfo::new(name, mount_point, total, free, fs_type))
        })
        .collect();

    volumes.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    volumes
}

/// Whether `mount` is a volume to offer. Not a pseudo-filesystem (proc, sysfs, tmpfs, cgroup2, ...),
/// whose source isn't a device path. Some real filesystems have none either: the root filesystem
/// whatever backs it (composefs's overlay on Fedora Atomic, a container's overlay; not the `rootfs`
/// old kernels list besides the real root), and ZFS datasets (`pool/dataset`). Nor a snap's loopback
/// mount, nor a bind mount of a folder of a filesystem (the Firefox snap's `host-hunspell`, a
/// container's files), which shows part of a volume offered already. A btrfs subvolume mounts a
/// folder of its filesystem too, but is a volume of its own (Fedora's `/` and `/home`).
fn is_volume(mount: &Mount) -> bool {
    let Mount {
        root,
        mount_point,
        fs_type,
        source,
        ..
    } = *mount;
    (source.starts_with('/') || (mount_point == "/" && fs_type != "rootfs") || fs_type == "zfs")
        && (root == "/" || fs_type == "btrfs")
        && !mount_point.starts_with("/snap/")
}

/// /proc/self/mountinfo escapes a space, tab, newline or backslash in a mount point as octal
/// (`\040`).
fn unescape_octal(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let o1 = bytes[i + 1].wrapping_sub(b'0');
            let o2 = bytes[i + 2].wrapping_sub(b'0');
            let o3 = bytes[i + 3].wrapping_sub(b'0');
            if o1 < 4 && o2 < 8 && o3 < 8 {
                out.push(o1 * 64 + o2 * 8 + o3);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Total and free bytes of the filesystem mounted at `path`, via `statvfs`. Free counts the blocks
/// reserved for the superuser too (5% of an ext4 volume by default), which are no one's data: the
/// used bytes left are what `df` shows as used. None for a file bind-mounted from another
/// filesystem (a container's /etc/hosts).
// The field widths vary by platform (u32 on 32-bit Linux, u64 elsewhere).
#[allow(clippy::useless_conversion)]
fn usage(path: &str) -> Option<(u64, u64)> {
    let c_path = CString::new(path).ok()?;
    if !std::fs::metadata(path).is_ok_and(|meta| meta.is_dir()) {
        return None;
    }
    let mut stat = libc::statvfs::default();
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block_size = u64::from(stat.f_frsize);
    let total = u64::from(stat.f_blocks) * block_size;
    let free = u64::from(stat.f_bfree) * block_size;
    Some((total, free))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volumes_are_real_filesystems() {
        let volume = |line: &str| is_volume(&Mount::parse(line).unwrap());
        for line in [
            "29 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw",
            "26 1 0:24 / / rw,relatime shared:1 - zfs rpool/ROOT/ubuntu_e8yh0v rw,xattr,posixacl",
            "31 26 0:42 / /home rw,relatime shared:2 - zfs rpool/USERDATA/home_x rw",
            "27 1 0:25 / / ro,relatime shared:1 - overlay composefs ro,lowerdir+=/run/ostree",
            "40 29 8:1 / /mnt/My\\040Disk rw - ext4 /dev/sda1 rw",
            "41 29 0:50 / /mnt/share rw - cifs //nas.lan/share rw",
            // Fedora: both are subvolumes of one btrfs filesystem.
            "60 1 0:33 /root / rw,relatime shared:1 - btrfs /dev/nvme0n1p3 rw,subvol=/root",
            "61 60 0:33 /home /home rw,relatime shared:2 - btrfs /dev/nvme0n1p3 rw,subvol=/home",
        ] {
            assert!(volume(line), "{line}");
        }
        for line in [
            "22 29 0:21 / /proc rw - proc proc rw",
            "25 29 0:23 / /run rw - tmpfs tmpfs rw",
            "30 23 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw",
            "1 1 0:1 / / rw - rootfs rootfs rw",
            "50 29 7:3 / /snap/core22/1380 ro - squashfs /dev/loop3 ro",
            "70 29 0:60 / /var/lib/docker/overlay2/x/merged rw - overlay overlay rw",
            "345 33 259:5 /usr/share/hunspell /var/snap/firefox/common/host-hunspell ro,noexec shared:1 - ext4 /dev/nvme0n1p5 rw",
        ] {
            assert!(!volume(line), "{line}");
        }
    }

    #[test]
    fn mount_points_are_unescaped() {
        assert_eq!(unescape_octal("/mnt/My\\040Disk"), "/mnt/My Disk");
        assert_eq!(unescape_octal("/mnt/a\\134b"), "/mnt/a\\b");
    }
}
