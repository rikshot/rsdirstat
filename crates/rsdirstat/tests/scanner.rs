use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::mpsc;

use rsdirstat_core::scan::ScanEvent;
use rsdirstat_core::tree::{DirTree, FileEntry};

#[cfg(target_os = "linux")]
use rsdirstat_linux as scanner;
#[cfg(target_os = "macos")]
use rsdirstat_macos as scanner;
#[cfg(target_os = "windows")]
use rsdirstat_windows as scanner;

fn scan_to_tree(root: &Path) -> DirTree {
    let (tx, rx) = mpsc::channel();
    let root = root.to_path_buf();
    let handle =
        std::thread::spawn(move || scanner::scan::scan(&root, Default::default(), tx, Default::default()).unwrap());
    // The scan's own failure first, before what it left out.
    handle.join().unwrap();
    tree_from(rx)
}

/// The tree a scan's `events` build, which must be complete.
fn tree_from(events: mpsc::Receiver<ScanEvent>) -> DirTree {
    let mut tree = DirTree::new();
    // `ScanDone` is the last event, and applying it recomputes the sizes.
    for event in events {
        tree.apply(event);
    }
    assert!(tree.complete, "the scan must read everything");
    tree
}

fn files(tree: &DirTree) -> impl Iterator<Item = &FileEntry> {
    tree.nodes.values().flat_map(|node| &node.files)
}

fn file_names(tree: &DirTree) -> Vec<&str> {
    files(tree).map(|file| &*file.name).collect()
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
    let tree = scan_to_tree(dir.path());

    assert!(tree.root_id.is_some());

    let names: Vec<&str> = tree.nodes.values().map(|n| &*n.name).collect();
    assert!(names.contains(&"src"), "missing src in {names:?}");
    assert!(names.contains(&"docs"), "missing docs in {names:?}");
    assert!(names.contains(&"nested"), "missing nested in {names:?}");
}

#[test]
fn scanner_finds_all_files() {
    let dir = create_test_dir();
    let tree = scan_to_tree(dir.path());

    let file_names = file_names(&tree);
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
    let tree = scan_to_tree(dir.path());

    let by_name: HashMap<&str, u64> = files(&tree).map(|file| (&*file.name, file.size)).collect();

    assert_eq!(by_name["lib.rs"], 1024);
    assert_eq!(by_name["root_file.bin"], 4096);
    assert_eq!(by_name["main.rs"], "fn main() {}".len() as u64);
}

#[test]
fn scanner_tree_sizes_are_consistent() {
    let dir = create_test_dir();
    let tree = scan_to_tree(dir.path());

    let root_id = tree.root_id.unwrap();
    let root_size = tree.nodes[&root_id].size;

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
    let tree = scan_to_tree(dir.path());

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

    let tree = scan_to_tree(dir.path());

    assert!(tree.root_id.is_some());
    let names: Vec<&str> = tree.nodes.values().map(|n| &*n.name).collect();
    assert!(names.contains(&"empty"));
    assert_eq!(files(&tree).count(), 0);
}

#[test]
fn scanner_reports_positive_mtime() {
    let dir = create_test_dir();
    let tree = scan_to_tree(dir.path());

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

    for file in files(&tree) {
        assert!(
            file.mtime > 0,
            "file '{}' has non-positive mtime {}",
            file.name,
            file.mtime
        );
    }
}

/// Linux names are bytes, not necessarily UTF-8: a file named in Latin-1 still counts (under a lossy
/// name), and so does everything under a directory named that way.
#[cfg(target_os = "linux")]
#[test]
fn scanner_counts_names_that_are_not_utf8() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().unwrap();
    let path = |name: &[u8]| dir.path().join(OsStr::from_bytes(name));
    fs::write(path(b"caf\xe9.bin"), vec![0u8; 100]).unwrap();
    fs::create_dir(path(b"donn\xe9es")).unwrap();
    fs::write(path(b"donn\xe9es").join("inside.bin"), vec![0u8; 20]).unwrap();

    let tree = scan_to_tree(dir.path());

    assert_eq!(tree.nodes[&tree.root_id.unwrap()].size, 120);
    let names = file_names(&tree);
    assert!(names.contains(&"caf\u{FFFD}.bin"), "{names:?}");
    assert!(names.contains(&"inside.bin"), "{names:?}");
}

#[test]
fn scanner_unicode_filenames() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("données")).unwrap();
    fs::write(dir.path().join("données").join("日本語.txt"), "test").unwrap();
    fs::write(dir.path().join("café.rs"), vec![0u8; 512]).unwrap();

    let tree = scan_to_tree(dir.path());

    let dir_names: Vec<&str> = tree.nodes.values().map(|n| &*n.name).collect();
    assert!(dir_names.contains(&"données"), "missing unicode dir in {dir_names:?}");

    let file_names = file_names(&tree);
    assert!(
        file_names.contains(&"日本語.txt"),
        "missing unicode file in {file_names:?}"
    );
    assert!(
        file_names.contains(&"café.rs"),
        "missing unicode file in {file_names:?}"
    );
}

/// A directory that can't be opened is left out, along with everything in it, while the rest of the
/// tree is scanned.
#[cfg(unix)]
#[test]
fn scanner_leaves_out_unreadable_directories() {
    use std::os::unix::fs::PermissionsExt;

    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root can open any directory");
        return;
    }
    let dir = create_test_dir();
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("secret.bin"), vec![0u8; 100]).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let tree = scan_to_tree(dir.path());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let dir_names: Vec<&str> = tree.nodes.values().map(|node| &*node.name).collect();
    assert!(!dir_names.contains(&"locked"), "{dir_names:?}");
    assert!(dir_names.contains(&"nested"), "the rest is scanned: {dir_names:?}");
}

/// On Windows a scan of a whole drive reads NTFS's file index (the MFT) where it can, as
/// administrator, which is how CI runs: read for any folder, it must find what walking the folders
/// finds. (Hardlinks are left out: the walk counts one in whichever of its folders a worker reaches
/// first.)
#[cfg(windows)]
#[test]
fn reading_the_mft_finds_what_the_folder_walk_finds() {
    use std::collections::BTreeMap;

    use rsdirstat_core::scan::ScanOptions;

    let dir = create_test_dir();
    let root = dir.path();
    fs::write(root.join("src").join("empty"), "").unwrap();
    fs::create_dir_all(root.join("a").join("b").join("c")).unwrap();
    fs::write(root.join("a").join("b").join("c").join("big.bin"), vec![1u8; 300_000]).unwrap();
    for i in 0..200 {
        fs::write(root.join("a").join(format!("file{i:03}.txt")), vec![2u8; i]).unwrap();
    }

    // Each folder by path: its id, size, newest file and files.
    type Folders = BTreeMap<String, (u64, u64, i64, Vec<(String, u64)>)>;
    let folders = |tree: &DirTree| -> Folders {
        tree.nodes
            .iter()
            .map(|(&id, node)| {
                let mut files: Vec<_> = node
                    .files
                    .iter()
                    .map(|file| (file.name.to_string(), file.size))
                    .collect();
                files.sort();
                let path = tree.full_path(id).unwrap().display().to_string();
                (path, (id, node.size, node.mtime, files))
            })
            .collect()
    };

    let (tx, rx) = mpsc::channel();
    let options = ScanOptions {
        no_mft: true,
        ..Default::default()
    };
    scanner::scan::scan(root, options, tx, Default::default()).unwrap();
    let walked = tree_from(rx);

    let (tx, rx) = mpsc::channel();
    if let Err(e) = scanner::scan::scan_mft(root, tx) {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI runs as administrator, so the MFT must be readable: {e:#}"
        );
        eprintln!("skipped: can't read the MFT here ({e:#}); run as administrator");
        return;
    }
    let read = tree_from(rx);
    assert_eq!(folders(&read), folders(&walked));
}

#[test]
fn scanner_dedups_hardlinks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("original.bin"), vec![0u8; 10_240]).unwrap();
    fs::hard_link(root.join("original.bin"), root.join("hardlink.bin")).unwrap();

    let tree = scan_to_tree(root);
    let root_id = tree.root_id.unwrap();
    let root_size = tree.nodes[&root_id].size;
    assert_eq!(
        root_size, 10_240,
        "hardlinked file should be counted once, got root size {root_size}"
    );
}
