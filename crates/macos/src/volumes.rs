use std::ffi::{CStr, CString};

use rsdirstat_core::volume::VolumeInfo;

pub fn list_volumes() -> Vec<VolumeInfo> {
    let mut volumes = Vec::new();
    let mounts = mount_table();

    let Ok(entries) = std::fs::read_dir("/Volumes") else {
        return volumes;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        // "Macintosh HD" is a symlink to "/". Read it as one, rather than resolving the path: that
        // would look into whatever is mounted there, and a network share whose server is gone
        // doesn't answer.
        let mount_point = if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
            match std::fs::read_link(&path) {
                Ok(target) if target.is_absolute() => target,
                _ => continue,
            }
        } else {
            path.clone()
        };
        let Some(mount_str) = mount_point.to_str() else {
            continue;
        };
        // Only a mount point is a volume. A folder left behind by an unclean eject, or Time
        // Machine's `.timemachine` and `com.apple.TimeMachine.localsnapshots` folders, lie on the
        // Data volume and would show its size again.
        let Some(mount) = mounts
            .iter()
            .find(|mount| c_chars(&mount.f_mntonname) == mount_str.as_bytes())
        else {
            continue;
        };
        // A local disk's sizes as they are now; a network share's as the kernel last had them.
        let fresh = (mount.f_flags & libc::MNT_LOCAL as u32 != 0)
            .then(|| CString::new(mount_str).ok().and_then(|path| statfs(&path)))
            .flatten();
        let fs = fresh.as_ref().unwrap_or(mount);
        let block_size = u64::from(fs.f_bsize);

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Unknown")
            .to_string();

        volumes.push(VolumeInfo::new(
            name,
            mount_str.to_string(),
            fs.f_blocks.saturating_mul(block_size),
            // Free, reserved blocks included, so what is left counts as used as `df` shows it.
            fs.f_bfree.saturating_mul(block_size),
            String::from_utf8_lossy(&c_chars(&fs.f_fstypename)).into_owned(),
        ));
    }

    volumes.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    volumes
}

/// The mounted filesystems, as the kernel last knew them (`MNT_NOWAIT`): asking each one would wait
/// on a network share whose server is gone, for minutes or for good, and hold up the picker. Their
/// block counts are 64-bit, unlike `statvfs`'s on macOS.
fn mount_table() -> Vec<libc::statfs> {
    loop {
        let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        let Ok(count) = usize::try_from(count) else {
            return Vec::new();
        };
        // Room for a few mounted meanwhile.
        let mut table = vec![libc::statfs::default(); count + 4];
        let Ok(bytes) = libc::c_int::try_from(table.len() * size_of::<libc::statfs>()) else {
            return Vec::new();
        };
        let filled = unsafe { libc::getfsstat(table.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
        let Ok(filled) = usize::try_from(filled) else {
            return Vec::new();
        };
        if filled < table.len() {
            table.truncate(filled);
            return table;
        }
        // It filled all the room made: more were mounted meanwhile, so ask again.
    }
}

fn statfs(path: &CStr) -> Option<libc::statfs> {
    let mut buf = libc::statfs::default();
    (unsafe { libc::statfs(path.as_ptr(), &mut buf) } == 0).then_some(buf)
}

/// The bytes of one of statfs's NUL-terminated name fields.
fn c_chars(chars: &[libc::c_char]) -> Vec<u8> {
    chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_volumes_finds_root() {
        let volumes = list_volumes();
        assert!(!volumes.is_empty(), "should find at least one volume");
        // On macOS, / should appear (via the /Volumes/Macintosh HD symlink)
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

    /// Every volume is a mount point, so one is listed once and a plain folder not at all.
    #[test]
    fn volumes_are_mount_points() {
        let mounts: Vec<Vec<u8>> = mount_table().iter().map(|mount| c_chars(&mount.f_mntonname)).collect();
        for volume in list_volumes() {
            assert!(
                mounts.contains(&volume.mount_point.as_bytes().to_vec()),
                "{} isn't mounted",
                volume.mount_point
            );
        }
    }
}
