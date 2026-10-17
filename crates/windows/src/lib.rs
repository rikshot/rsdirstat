//! The Windows scanner. On any other OS this crate is empty, so the workspace builds everywhere and
//! each OS's build of `rsdirstat` picks its own scanner.
#![cfg(windows)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

mod entry;
mod mft;
mod ntfs;
pub mod reveal;
pub mod scan;
pub mod volumes;

/// `s` as a NUL-terminated UTF-16 string, for the Windows API.
fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// A fixed-size, NUL-terminated UTF-16 buffer up to its first NUL.
fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
