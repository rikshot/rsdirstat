use std::ffi::CString;

use rsdirstat_core::volume::{VolumeInfo, statvfs_usage};

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

        let Some((total, free)) = statvfs_usage(&c_path) else {
            continue;
        };

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Unknown")
            .to_string();

        volumes.push(VolumeInfo::new(
            name,
            mount_str.to_string(),
            total,
            free,
            fs_type_for(&c_path),
        ));
    }

    volumes.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    volumes
}

fn fs_type_for(c_path: &CString) -> String {
    let mut buf = libc::statfs::default();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_volumes_finds_root() {
        let volumes = list_volumes();
        assert!(!volumes.is_empty(), "should find at least one volume");
        // On macOS, / should appear (via /Volumes/Macintosh HD firmlink)
        let has_root = volumes.iter().any(|v| v.mount_point == "/");
        assert!(
            has_root,
            "should find root volume, got: {:?}",
            volumes.iter().map(|v| &v.mount_point).collect::<Vec<_>>()
        );
    }

    #[test]
    fn volumes_have_valid_sizes() {
        for vol in list_volumes() {
            assert!(vol.total_bytes > 0, "{}: total should be > 0", vol.name);
            assert!(vol.used_bytes <= vol.total_bytes, "{}: used > total", vol.name);
        }
    }

    #[test]
    fn root_volume_has_fs_type() {
        let volumes = list_volumes();
        let root = volumes.iter().find(|v| v.mount_point == "/").unwrap();
        assert!(!root.fs_type.is_empty(), "root should have a fs type");
    }
}
