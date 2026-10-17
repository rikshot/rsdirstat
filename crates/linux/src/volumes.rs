use std::collections::BTreeSet;
use std::ffi::CString;
use std::path::Path;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use rsdirstat_core::volume::VolumeInfo;

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
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };

    // A mount that may not answer is asked on a thread of its own, all of them at once, so a stuck one
    // holds up neither the others nor the list: it is left out, and its thread left to the syscall.
    // One still stuck from an earlier list is left out at once.
    let probes: Vec<_> = mounts
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let device = parts.next()?;
            let raw_mount = parts.next()?;
            let fs_type = parts.next().unwrap_or("");

            if !is_volume(device, raw_mount, fs_type) {
                return None;
            }
            let mount_point = unescape_octal(raw_mount);

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

/// Whether the /proc/mounts line of `device` mounted at `mount_point` (as escaped there) with
/// `fs_type` is a volume to offer. Not a pseudo-filesystem (proc, sysfs, tmpfs, cgroup2, ...), whose
/// source isn't a device path. Some real filesystems have none either: the root filesystem whatever
/// backs it (composefs's overlay on Fedora Atomic, a container's overlay; not the `rootfs` old
/// kernels list besides the real root), and ZFS datasets (`pool/dataset`). Nor a snap's loopback
/// mount.
fn is_volume(device: &str, mount_point: &str, fs_type: &str) -> bool {
    (device.starts_with('/') || (mount_point == "/" && fs_type != "rootfs") || fs_type == "zfs")
        && !mount_point.starts_with("/snap/")
}

/// /proc/mounts escapes a space, tab, newline or backslash in a mount point as octal (`\040`).
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
        let volume = |line: &str| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            is_volume(parts[0], parts[1], parts[2])
        };
        for line in [
            "/dev/nvme0n1p2 / ext4 rw,relatime 0 0",
            "rpool/ROOT/ubuntu_e8yh0v / zfs rw,relatime,xattr,posixacl 0 0",
            "rpool/USERDATA/home_x /home zfs rw,relatime 0 0",
            "composefs / overlay ro,relatime 0 0",
            "/dev/sda1 /mnt/My\\040Disk ext4 rw 0 0",
            "//nas.lan/share /mnt/share cifs rw 0 0",
        ] {
            assert!(volume(line), "{line}");
        }
        for line in [
            "proc /proc proc rw 0 0",
            "tmpfs /run tmpfs rw 0 0",
            "cgroup2 /sys/fs/cgroup cgroup2 rw 0 0",
            "rootfs / rootfs rw 0 0",
            "/dev/loop3 /snap/core22/1380 squashfs ro 0 0",
            "overlay /var/lib/docker/overlay2/x/merged overlay rw 0 0",
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
