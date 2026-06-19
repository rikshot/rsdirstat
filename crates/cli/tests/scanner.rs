use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::mpsc;

use rsdirstat_core::tree::DirTree;
use rsdirstat_protocol::ScanEvent;

#[cfg(target_os = "linux")]
use rsdirstat_linux as scanner;
#[cfg(target_os = "macos")]
use rsdirstat_macos as scanner;
#[cfg(target_os = "windows")]
use rsdirstat_windows as scanner;

fn scan_to_tree(root: &Path) -> (DirTree, Vec<(u64, String, u64)>) {
    let (tx, rx) = mpsc::channel();
    let root = root.to_path_buf();
    let handle = std::thread::spawn(move || scanner::scan::scan(&root, false, tx).unwrap());

    let mut tree = DirTree::new();
    let mut files = Vec::new();

    while let Ok(event) = rx.recv() {
        match event {
            ScanEvent::ScanStart { .. } => {}
            ScanEvent::Dir {
                id,
                parent,
                name,
                size,
                mtime,
            } => tree.insert_dir(id, parent, &name, size, mtime),
            ScanEvent::File {
                parent,
                name,
                size,
                mtime,
            } => {
                tree.insert_file(parent, &name, size, mtime);
                files.push((parent, name, size));
            }
            ScanEvent::ScanDone => break,
        }
    }

    handle.join().unwrap();
    tree.recompute_sizes();
    (tree, files)
}

fn create_test_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::create_dir(root.join("src")).unwrap();
    fs::create_dir(root.join("docs")).unwrap();
    fs::create_dir(root.join("src").join("nested")).unwrap();

    fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();
    fs::write(root.join("src").join("lib.rs"), vec![0u8; 1024]).unwrap();
    fs::write(root.join("src").join("nested").join("deep.rs"), "mod deep;").unwrap();
    fs::write(root.join("docs").join("readme.txt"), "hello world").unwrap();
    fs::write(root.join("root_file.bin"), vec![0u8; 4096]).unwrap();

    dir
}

#[test]
fn scanner_finds_all_directories() {
    let dir = create_test_dir();
    let (tree, _) = scan_to_tree(dir.path());

    assert!(tree.root_id.is_some());

    let names: Vec<&str> = tree.nodes.values().map(|n| &*n.name).collect();
    assert!(names.contains(&"src"), "missing src in {names:?}");
    assert!(names.contains(&"docs"), "missing docs in {names:?}");
    assert!(names.contains(&"nested"), "missing nested in {names:?}");
}

#[test]
fn scanner_finds_all_files() {
    let dir = create_test_dir();
    let (_, files) = scan_to_tree(dir.path());

    let file_names: Vec<&str> = files.iter().map(|(_, name, _)| name.as_str()).collect();
    assert!(file_names.contains(&"main.rs"), "missing main.rs in {file_names:?}");
    assert!(file_names.contains(&"lib.rs"), "missing lib.rs in {file_names:?}");
    assert!(file_names.contains(&"deep.rs"), "missing deep.rs in {file_names:?}");
    assert!(
        file_names.contains(&"readme.txt"),
        "missing readme.txt in {file_names:?}"
    );
    assert!(
        file_names.contains(&"root_file.bin"),
        "missing root_file.bin in {file_names:?}"
    );
}

#[test]
fn scanner_reports_correct_file_sizes() {
    let dir = create_test_dir();
    let (_, files) = scan_to_tree(dir.path());

    let by_name: HashMap<&str, u64> = files.iter().map(|(_, name, size)| (name.as_str(), *size)).collect();

    assert_eq!(by_name["lib.rs"], 1024);
    assert_eq!(by_name["root_file.bin"], 4096);
    assert_eq!(by_name["main.rs"], "fn main() {}".len() as u64);
}

#[test]
fn scanner_tree_sizes_are_consistent() {
    let dir = create_test_dir();
    let (tree, _) = scan_to_tree(dir.path());

    let root_id = tree.root_id.unwrap();
    let root_size = tree.recursive_sizes[&root_id];

    // Root size must be at least the sum of known file sizes
    let min_expected =
        4096 + 1024 + "fn main() {}".len() as u64 + "mod deep;".len() as u64 + "hello world".len() as u64;
    assert!(
        root_size >= min_expected,
        "root size {root_size} < minimum expected {min_expected}"
    );
}

#[test]
fn scanner_parent_child_relationships() {
    let dir = create_test_dir();
    let (tree, _) = scan_to_tree(dir.path());

    let root_id = tree.root_id.unwrap();
    let root = &tree.nodes[&root_id];

    // Root should have children (src, docs) and possibly the root file's parent
    assert!(!root.children.is_empty());

    // Find src dir and verify it has nested as a child
    let src_id = root
        .children
        .iter()
        .find(|&&id| &*tree.nodes[&id].name == "src")
        .unwrap();
    let src = &tree.nodes[src_id];
    assert!(
        src.children.iter().any(|&id| &*tree.nodes[&id].name == "nested"),
        "src should have nested as a child"
    );
}

#[test]
fn scanner_handles_empty_directory() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("empty")).unwrap();

    let (tree, files) = scan_to_tree(dir.path());

    assert!(tree.root_id.is_some());
    let names: Vec<&str> = tree.nodes.values().map(|n| &*n.name).collect();
    assert!(names.contains(&"empty"));
    assert!(files.is_empty());
}

#[test]
fn scanner_reports_positive_mtime() {
    let dir = create_test_dir();
    let (tree, files) = scan_to_tree(dir.path());

    for node in tree.nodes.values() {
        if node.parent != 0 {
            assert!(
                node.mtime > 0,
                "dir '{}' has non-positive mtime {}",
                node.name,
                node.mtime
            );
        }
    }

    for (_, name, _) in &files {
        // Files are in node.files, check via the tree
        for node in tree.nodes.values() {
            for file in &node.files {
                if &*file.name == name.as_str() {
                    assert!(file.mtime > 0, "file '{}' has non-positive mtime {}", name, file.mtime);
                }
            }
        }
    }
}

#[test]
fn scanner_unicode_filenames() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("données")).unwrap();
    fs::write(dir.path().join("données").join("日本語.txt"), "test").unwrap();
    fs::write(dir.path().join("café.rs"), vec![0u8; 512]).unwrap();

    let (tree, files) = scan_to_tree(dir.path());

    let dir_names: Vec<&str> = tree.nodes.values().map(|n| &*n.name).collect();
    assert!(dir_names.contains(&"données"), "missing unicode dir in {dir_names:?}");

    let file_names: Vec<&str> = files.iter().map(|(_, name, _)| name.as_str()).collect();
    assert!(
        file_names.contains(&"日本語.txt"),
        "missing unicode file in {file_names:?}"
    );
    assert!(
        file_names.contains(&"café.rs"),
        "missing unicode file in {file_names:?}"
    );
}

// Hardlink dedup is implemented on Unix (cheap via statx/getattrlistbulk link counts); the Windows
// scanner does not dedup (link count would require a per-file open), so this is Unix-only.
#[cfg(unix)]
#[test]
fn scanner_dedups_hardlinks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("original.bin"), vec![0u8; 10_240]).unwrap();
    fs::hard_link(root.join("original.bin"), root.join("hardlink.bin")).unwrap();

    let (tree, _) = scan_to_tree(root);
    let root_id = tree.root_id.unwrap();
    let root_size = tree.recursive_sizes[&root_id];
    assert_eq!(
        root_size, 10_240,
        "hardlinked file should be counted once, got root size {root_size}"
    );
}
