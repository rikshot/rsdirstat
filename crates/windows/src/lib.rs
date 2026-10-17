//! The Windows scanner. On any other OS this crate is empty, so the workspace builds everywhere and
//! each OS's build of `rsdirstat` picks its own scanner.
#![cfg(windows)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::System::Diagnostics::Debug::{GetErrorMode, SEM_FAILCRITICALERRORS, SetErrorMode};

mod entry;
mod mft;
mod ntfs;
pub mod reveal;
pub mod scan;
pub mod volumes;

/// Have Windows fail the calls that would open its "There is no disk in the drive" box (an empty
/// card-reader slot, a drive pulled mid-scan) back to their callers instead, for the whole process:
/// the box holds the call until someone answers it. Microsoft advises every application to, at
/// startup: before the command line's path, the volume list or a scan touches a drive.
pub fn fail_critical_errors() {
    unsafe { SetErrorMode(GetErrorMode() | SEM_FAILCRITICALERRORS) };
}

/// `s` as a NUL-terminated UTF-16 string, for the Windows API.
fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// A fixed-size, NUL-terminated UTF-16 buffer up to its first NUL.
fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
