pub use rsdirstat_protocol::VolumeInfo;

/// Total and available bytes of the filesystem mounted at `path`, via `statvfs`.
#[cfg(unix)]
// The field widths vary by platform (u32 on macOS and 32-bit Linux, u64 elsewhere).
#[allow(clippy::useless_conversion)]
pub fn statvfs_usage(path: &std::ffi::CStr) -> Option<(u64, u64)> {
    let mut stat = libc::statvfs::default();
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block_size = u64::from(stat.f_frsize);
    let total = u64::from(stat.f_blocks) * block_size;
    let free = u64::from(stat.f_bavail) * block_size;
    Some((total, free))
}
