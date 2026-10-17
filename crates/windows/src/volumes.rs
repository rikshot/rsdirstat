use rsdirstat_core::volume::VolumeInfo;
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};
use windows_sys::Win32::System::Diagnostics::Debug::{GetThreadErrorMode, SEM_FAILCRITICALERRORS, SetThreadErrorMode};

use crate::{from_wide, wide};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;

/// Fails this thread's critical errors back to the caller, instead of the system's "There is no disk
/// in the drive" box, until dropped. Without it, asking about an empty card-reader slot waits on that
/// box until someone answers it.
struct NoCriticalErrorBox(Option<u32>);

impl NoCriticalErrorBox {
    fn new() -> Self {
        let mut old = 0;
        let set = unsafe { SetThreadErrorMode(GetThreadErrorMode() | SEM_FAILCRITICALERRORS, &mut old) } != 0;
        Self(set.then_some(old))
    }
}

impl Drop for NoCriticalErrorBox {
    fn drop(&mut self) {
        if let Some(old) = self.0 {
            unsafe { SetThreadErrorMode(old, std::ptr::null_mut()) };
        }
    }
}

pub fn list_volumes() -> Vec<VolumeInfo> {
    let _no_box = NoCriticalErrorBox::new();
    let mut volumes = Vec::new();
    let drives = unsafe { GetLogicalDrives() };

    for i in 0..26u32 {
        if drives & (1 << i) == 0 {
            continue;
        }

        let mount = format!("{}:\\", (b'A' + i as u8) as char);
        let root = wide(&mount);

        let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
        if drive_type != DRIVE_FIXED && drive_type != DRIVE_REMOVABLE {
            continue;
        }

        // Both as this user sees them: with disk quotas, the total is the user's quota, and the
        // volume's free space would leave nothing of it used.
        let mut free_bytes: u64 = 0;
        let mut total_bytes: u64 = 0;
        if unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut free_bytes, &mut total_bytes, std::ptr::null_mut()) } == 0 {
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

        let (vol_name, fs_type) = if has_info {
            (from_wide(&name_buf), from_wide(&fs_buf))
        } else {
            (String::new(), String::new())
        };
        let label = if vol_name.is_empty() { "Local Disk" } else { &vol_name };
        let name = format!("{label} ({mount})");

        volumes.push(VolumeInfo::new(name, mount, total_bytes, free_bytes, fs_type));
    }

    volumes
}
