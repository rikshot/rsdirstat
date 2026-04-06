use std::ffi::CString;
use std::path::Path;

use rsdirstat_core::volume::VolumeInfo;

pub fn list_volumes() -> Vec<VolumeInfo> {
    let mut volumes = Vec::new();
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return volumes;
    };

    for line in mounts.lines() {
        let mut parts = line.split_whitespace();
        let Some(device) = parts.next() else { continue };
        let Some(mount_point) = parts.next() else { continue };
        let fs_type = parts.next().unwrap_or("");

        // Skip pseudo-filesystems
        if !device.starts_with('/') {
            continue;
        }
        // Skip snap loopback mounts
        if mount_point.starts_with("/snap/") {
            continue;
        }

        let Ok(c_path) = CString::new(mount_point) else {
            continue;
        };

        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            continue;
        }

        let block_size = stat.f_frsize as u64;
        let total = stat.f_blocks as u64 * block_size;
        let free = stat.f_bavail as u64 * block_size;

        let name = if mount_point == "/" {
            "Root".to_string()
        } else {
            Path::new(mount_point)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(mount_point)
                .to_string()
        };

        volumes.push(VolumeInfo {
            name,
            mount_point: mount_point.to_string(),
            total_bytes: total,
            used_bytes: total.saturating_sub(free),
            fs_type: fs_type.to_string(),
        });
    }

    volumes.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    volumes
}
