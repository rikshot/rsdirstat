//! The Windows scanner. On any other OS this crate is empty, so the workspace builds everywhere and
//! each OS's build of `rsdirstat` picks its own scanner.
#![cfg(windows)]

pub mod scan;
pub mod volumes;
