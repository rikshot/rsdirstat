use std::ffi::CString;

use rsdirstat_core::volume::VolumeInfo;

pub fn list_volumes() -> Vec<VolumeInfo> {
    let mut volumes = Vec::new();

    let Ok(entries) = std::fs::read_dir("/Volumes") else {
        return volumes;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let mount_point = std::fs::canonicalize(&path).unwrap_or(path.clone());
        let Some(mount_str) = mount_point.to_str() else {
            continue;
        };
        let Ok(c_path) = CString::new(mount_str) else { continue };

        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            continue;
        }

        let block_size = stat.f_frsize as u64;
        let total = stat.f_blocks as u64 * block_size;
        let free = stat.f_bavail as u64 * block_size;

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Unknown")
            .to_string();

        volumes.push(VolumeInfo {
            name,
            mount_point: mount_str.to_string(),
            total_bytes: total,
            used_bytes: total.saturating_sub(free),
            fs_type: fs_type_for(&c_path),
        });
    }

    volumes.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    volumes
}

fn fs_type_for(c_path: &CString) -> String {
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
        return String::new();
    }
    let bytes: Vec<u8> = buf
        .f_fstypename
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
