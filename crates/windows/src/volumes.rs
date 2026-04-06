use rsdirstat_core::volume::VolumeInfo;
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;

pub fn list_volumes() -> Vec<VolumeInfo> {
    let mut volumes = Vec::new();
    let drives = unsafe { GetLogicalDrives() };

    for i in 0..26u32 {
        if drives & (1 << i) == 0 {
            continue;
        }

        let letter = (b'A' + i as u8) as char;
        let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain(Some(0)).collect();

        let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
        if drive_type != DRIVE_FIXED && drive_type != DRIVE_REMOVABLE {
            continue;
        }

        let mut free_bytes: u64 = 0;
        let mut total_bytes: u64 = 0;
        if unsafe { GetDiskFreeSpaceExW(root.as_ptr(), std::ptr::null_mut(), &mut total_bytes, &mut free_bytes) } == 0 {
            continue;
        }

        let mut name_buf = [0u16; 256];
        let mut fs_buf = [0u16; 64];
        let has_info = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                name_buf.as_mut_ptr(),
                name_buf.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs_buf.as_mut_ptr(),
                fs_buf.len() as u32,
            )
        } != 0;

        let vol_name = if has_info {
            String::from_utf16_lossy(&name_buf).trim_end_matches('\0').to_string()
        } else {
            String::new()
        };
        let fs_type = if has_info {
            String::from_utf16_lossy(&fs_buf).trim_end_matches('\0').to_string()
        } else {
            String::new()
        };

        let mount = format!("{letter}:\\");
        let name = if vol_name.is_empty() {
            format!("Local Disk ({mount})")
        } else {
            format!("{vol_name} ({mount})")
        };

        volumes.push(VolumeInfo {
            name,
            mount_point: mount,
            total_bytes,
            used_bytes: total_bytes.saturating_sub(free_bytes),
            fs_type,
        });
    }

    volumes
}
