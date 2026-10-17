//! The Linux scanner. On any other OS this crate is empty, so the workspace builds everywhere and
//! each OS's build of `rsdirstat` picks its own scanner.
#![cfg(target_os = "linux")]

pub mod reveal;
pub mod scan;
pub mod volumes;
