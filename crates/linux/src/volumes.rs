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
        let Some(raw_mount) = parts.next() else { continue };
        let fs_type = parts.next().unwrap_or("");

        // Skip pseudo-filesystems
        if !device.starts_with('/') {
            continue;
        }

        let mount_point = unescape_octal(raw_mount);

        // Skip snap loopback mounts
        if mount_point.starts_with("/snap/") {
            continue;
        }

        let Ok(c_path) = CString::new(mount_point.as_str()) else {
            continue;
        };

        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            continue;
        }

        #[allow(clippy::useless_conversion)]
        let block_size: u64 = stat.f_frsize.into();
        #[allow(clippy::useless_conversion)]
        let total: u64 = u64::from(stat.f_blocks) * block_size;
        #[allow(clippy::useless_conversion)]
        let free: u64 = u64::from(stat.f_bavail) * block_size;

        let name = if mount_point == "/" {
            "Root".to_string()
        } else {
            Path::new(&mount_point)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&mount_point)
                .to_string()
        };

        volumes.push(VolumeInfo {
            name,
            mount_point,
            total_bytes: total,
            used_bytes: total.saturating_sub(free),
            fs_type: fs_type.to_string(),
        });
    }

    volumes.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    volumes
}

/// /proc/mounts encodes spaces as \040, tabs as \011, and non-ASCII bytes as octal.
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
