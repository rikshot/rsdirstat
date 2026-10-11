//! A tree wider than the process's file descriptor limit is still scanned in full. Its own test
//! binary, because it lowers the limit for the whole process for good.
#![cfg(unix)]

use std::fs;
use std::sync::mpsc;

use rsdirstat_core::tree::DirTree;

#[cfg(target_os = "linux")]
use rsdirstat_linux as scanner;
#[cfg(target_os = "macos")]
use rsdirstat_macos as scanner;

#[test]
fn a_tree_wider_than_the_fd_limit_is_scanned_in_full() {
    // Far below the subdirectory count, but enough for the workers' own descriptors.
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let limit = (threads as u64 * 8).max(256);
    let dirs = limit * 4;

    let dir = tempfile::tempdir().unwrap();
    for index in 0..dirs {
        let sub = dir.path().join(format!("d{index}"));
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("f"), "x").unwrap();
    }

    // The hard limit too, so the scanner can't raise it back.
    let rlim = libc::rlimit {
        rlim_cur: limit as libc::rlim_t,
        rlim_max: limit as libc::rlim_t,
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) }, 0);

    let (tx, rx) = mpsc::channel();
    let root = dir.path().to_path_buf();
    let handle = std::thread::spawn(move || scanner::scan::scan(&root, false, tx).unwrap());
    let mut tree = DirTree::new();
    for event in rx {
        tree.apply(event);
    }
    handle.join().unwrap();

    assert!(tree.complete);
    assert_eq!(tree.nodes.len() as u64, dirs + 1, "every subdirectory is found");
    assert_eq!(tree.nodes[&tree.root_id.unwrap()].size, dirs, "and read");
}
