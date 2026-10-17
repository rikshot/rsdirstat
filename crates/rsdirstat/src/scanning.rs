//! What both binaries share about scanning: this platform's scanner, and how to scan as the command
//! line says.

use std::num::NonZeroUsize;

use rsdirstat_core::scan::{Mft, ScanOptions};

#[cfg(target_os = "linux")]
pub use rsdirstat_linux as scanner;
#[cfg(target_os = "macos")]
pub use rsdirstat_macos as scanner;
#[cfg(target_os = "windows")]
pub use rsdirstat_windows as scanner;

#[derive(clap::Args)]
pub struct ScanArgs {
    /// Cross filesystem boundaries (not into Linux's /proc, /sys and the like; on Windows not into
    /// volumes mounted in folders, which are skipped like junctions)
    #[arg(long)]
    all: bool,

    /// Scan with this many threads, any number [default: one per CPU; two per CPU on Linux; at
    /// most 4 on macOS]
    #[arg(long)]
    threads: Option<NonZeroUsize>,

    /// On Windows, walk a whole drive's folders even as administrator, instead of reading NTFS's
    /// file index
    #[arg(long)]
    no_mft: bool,
}

impl From<ScanArgs> for ScanOptions {
    fn from(args: ScanArgs) -> Self {
        Self {
            cross_filesystems: args.all,
            threads: args.threads,
            mft: if args.no_mft { Mft::Off } else { Mft::Auto },
        }
    }
}
