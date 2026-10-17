pub use rsdirstat_protocol::VolumeInfo;

/// Total and free bytes of the filesystem mounted at `path`. Free counts the blocks reserved for the
/// superuser too, which are no one's data: the used bytes left are what `df` shows as used.
#[cfg(target_os = "macos")]
pub fn statvfs_usage(path: &std::ffi::CStr) -> Option<(u64, u64)> {
    // statfs, not statvfs: macOS's statvfs casts statfs's 64-bit block counts to its 32-bit
    // fsblkcnt_t (no EOVERFLOW), so past 2^32 blocks (16 TiB of 4 KiB blocks) they wrap.
    let mut stat = libc::statfs::default();
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block_size = u64::from(stat.f_bsize);
    Some((
        stat.f_blocks.saturating_mul(block_size),
        stat.f_bfree.saturating_mul(block_size),
    ))
}

/// Total and free bytes of the filesystem mounted at `path`, via `statvfs`. Free counts the blocks
/// reserved for the superuser too (5% of an ext4 volume by default), which are no one's data: the
/// used bytes left are what `df` shows as used.
#[cfg(all(unix, not(target_os = "macos")))]
// The field widths vary by platform (u32 on 32-bit Linux, u64 elsewhere).
#[allow(clippy::useless_conversion)]
pub fn statvfs_usage(path: &std::ffi::CStr) -> Option<(u64, u64)> {
    let mut stat = libc::statvfs::default();
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block_size = u64::from(stat.f_frsize);
    let total = u64::from(stat.f_blocks) * block_size;
    let free = u64::from(stat.f_bfree) * block_size;
    Some((total, free))
}
