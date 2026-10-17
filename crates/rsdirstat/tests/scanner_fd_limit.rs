//! A tree wider than the process's file descriptor limit is still scanned in full. Its own test
//! binary, because it lowers the limit for the whole process for good.
#![cfg(unix)]

mod common;

use std::fs;

use rsdirstat_core::scan::cpu_count;

#[test]
fn a_tree_wider_than_the_fd_limit_is_scanned_in_full() {
    // Far below the subdirectory count, but enough for the workers' own descriptors.
    let limit = (cpu_count() as u64 * 8).max(256);
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

    let tree = common::scan_to_tree(dir.path());
    assert_eq!(tree.nodes.len() as u64, dirs + 1, "every subdirectory is found");
    assert_eq!(tree.nodes[&tree.root_id.unwrap()].size, dirs, "and read");
}
