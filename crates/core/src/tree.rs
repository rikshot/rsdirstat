use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::path::PathBuf;

use rsdirstat_protocol::BreadcrumbEntry;

use crate::color::{hash_name, hue_for_extension};
use crate::scan::{ScanEvent, ScanOutcome, ScannedFile};

pub struct FileEntry {
    pub name: Box<str>,
    pub size: u64,
    pub hue: u16,
    pub mtime: i64,
}

/// A map keyed by node id.
pub type NodeMap<V> = HashMap<u64, V, BuildHasherDefault<NodeIdHasher>>;

/// Hashes a node id by passing it through. Ids come from `scan::node_id`, already a 64-bit mix,
/// so hashing them again (SipHash is ~4-5× slower per lookup) would be wasted work.
#[derive(Default)]
pub struct NodeIdHasher(u64);

impl Hasher for NodeIdHasher {
    fn write(&mut self, bytes: &[u8]) {
        // Only reached for keys other than a lone u64; fold them in (FNV-1a) to stay correct.
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0100_0000_01B3);
        }
    }

    fn write_u64(&mut self, id: u64) {
        self.0 = id;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[derive(Default)]
pub struct DirNode {
    pub parent: u64,
    pub name: Box<str>,
    /// Total size of this directory's own files.
    pub direct_size: u64,
    /// Total size of everything under this directory: its files and all its subdirectories.
    /// Grows as directories are added during a scan; exact after `recompute_totals`.
    pub size: u64,
    pub children: Vec<u64>,
    pub files: Vec<FileEntry>,
    pub hue: u16,
    /// Newest modification time (Unix seconds) of this directory's own files; 0 with none.
    pub direct_mtime: i64,
    /// Newest modification time of anything under this directory, which Age mode colours it by: a
    /// folder holding only subfolders is as recent as the newest file in them. Kept up like `size`.
    pub mtime: i64,
    /// The tree's `changes` when anything under this directory last changed, so a view of it is
    /// known to be the same while it stays.
    pub changed: u64,
}

/// What a directory holds of the files a filter matches: their total size, and the newest of their
/// modification times (0 with none), which Age mode colours a folder by while the filter is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FilteredTotal {
    pub size: u64,
    pub mtime: i64,
}

impl FilteredTotal {
    pub(crate) fn add(self, size: u64, mtime: i64) -> Self {
        Self {
            size: self.size.saturating_add(size),
            mtime: self.mtime.max(mtime),
        }
    }
}

#[derive(Clone, Default, PartialEq)]
pub struct FilterConfig {
    pub extensions: Vec<Box<str>>,
    pub min_size: u64,
    pub name_pattern: String,
}

impl FilterConfig {
    pub fn is_active(&self) -> bool {
        !self.extensions.is_empty() || self.min_size > 0 || !self.name_pattern.is_empty()
    }

    pub fn matches_file(&self, name: &str, size: u64) -> bool {
        self.matcher().matches(name, size)
    }

    /// `matches_file` with what it needs of the filter prepared once, for checking many files.
    pub fn matcher(&self) -> FileMatcher<'_> {
        FileMatcher {
            filter: self,
            lowercase_pattern: (!self.name_pattern.is_ascii()).then(|| self.name_pattern.to_lowercase()),
        }
    }
}

/// A `FilterConfig` ready to check files with (see `FilterConfig::matcher`).
pub struct FileMatcher<'a> {
    filter: &'a FilterConfig,
    /// The name pattern in lower case, if it isn't ASCII; an ASCII one is compared byte by byte.
    lowercase_pattern: Option<String>,
}

impl FileMatcher<'_> {
    /// The cheap checks first: the name's case-insensitive search is most of the cost.
    pub fn matches(&self, name: &str, size: u64) -> bool {
        let filter = self.filter;
        size >= filter.min_size
            && (filter.extensions.is_empty()
                || filter.extensions.iter().any(|extension| has_extension(name, extension)))
            && self.name_matches(name)
    }

    /// Whether `name` contains the pattern, ignoring case: byte by byte for an ASCII pattern, the
    /// usual kind, and by Unicode lower case otherwise, so `kesä` finds `KESÄ.jpg`.
    fn name_matches(&self, name: &str) -> bool {
        match &self.lowercase_pattern {
            // An ASCII name's lower case is ASCII too, so it holds the pattern's only if that is
            // ASCII (as the Kelvin sign's `k` is), and then byte by byte, without lower-casing it.
            Some(lowercase) if name.is_ascii() => lowercase.is_ascii() && contains_ignoring_ascii_case(name, lowercase),
            Some(lowercase) => name.to_lowercase().contains(lowercase.as_str()),
            None => contains_ignoring_ascii_case(name, &self.filter.name_pattern),
        }
    }
}

/// Whether `text` contains `pattern`, ignoring ASCII case.
fn contains_ignoring_ascii_case(text: &str, pattern: &str) -> bool {
    let pattern = pattern.as_bytes();
    pattern.is_empty()
        || text
            .as_bytes()
            .windows(pattern.len())
            .any(|window| window.eq_ignore_ascii_case(pattern))
}

/// Whether `name` ends in `.` and `extension`, ignoring ASCII case. An extension can have dots of
/// its own (`tar.gz`), which comparing it with the text after the name's last dot would never match.
fn has_extension(name: &str, extension: &str) -> bool {
    let (name, extension) = (name.as_bytes(), extension.as_bytes());
    name.len() > extension.len()
        && name[name.len() - extension.len() - 1] == b'.'
        && name[name.len() - extension.len()..].eq_ignore_ascii_case(extension)
}

pub struct DirTree {
    pub nodes: NodeMap<DirNode>,
    pub root_id: Option<u64>,
    /// The scan root as people see it (from `ScanStart`); `full_path` joins names onto it.
    pub root_path: PathBuf,
    pub mtime_range: (i64, i64),
    /// How many scans have started on this tree (each `ScanStart` applied replaces its contents),
    /// so a change means it holds a different scan.
    pub scans: u64,
    /// How many scanner events have been applied, so an unchanged count means an unchanged tree.
    pub changes: u64,
    /// Whether the scan read everything: set by a `ScanDone` for a complete scan, so false while it
    /// runs, and after one that left something out (a stall, a read that failed) or never reported how
    /// it ended.
    pub complete: bool,
    hue_cache: HashMap<Box<str>, u16>,
}

impl Default for DirTree {
    fn default() -> Self {
        Self::new()
    }
}

impl DirTree {
    pub fn new() -> Self {
        Self {
            nodes: NodeMap::default(),
            root_id: None,
            root_path: PathBuf::new(),
            mtime_range: (i64::MAX, 0),
            scans: 0,
            changes: 0,
            complete: false,
            hue_cache: HashMap::new(),
        }
    }

    /// Fold one scanner event into the tree; `ScanDone` finalizes the recursive sizes.
    pub fn apply(&mut self, event: ScanEvent) {
        self.changes += 1;
        match event {
            ScanEvent::ScanStart { root } => {
                *self = Self {
                    root_path: root,
                    scans: self.scans + 1,
                    changes: self.changes,
                    ..Self::new()
                };
            }
            ScanEvent::Dir {
                id,
                parent,
                name,
                mtime,
                files,
            } => {
                let size = ScannedFile::total(&files);
                self.insert_files(id, files);
                self.insert_dir(id, parent, name, size, mtime);
            }
            ScanEvent::ScanDone { outcome } => {
                self.recompute_totals();
                self.complete = outcome == ScanOutcome::Complete;
            }
        }
    }

    /// `name` is taken by value so a scanner's `String` moves in without a second allocation.
    pub fn insert_dir(&mut self, id: u64, parent: u64, name: impl Into<Box<str>>, size: u64, mtime: i64) {
        self.insert_dir_deferred(id, parent, name, size, mtime);
        self.propagate(id, size, mtime);
    }

    /// `insert_dir`, but leaving the totals of the directories above it to `recompute_totals`
    /// (which `ScanDone` runs), for a caller with no use for them before then.
    pub fn insert_dir_deferred(&mut self, id: u64, parent: u64, name: impl Into<Box<str>>, size: u64, mtime: i64) {
        let name = name.into();
        let hue = hash_name(&name);

        if parent == 0 {
            if self.root_id.is_none() {
                self.root_id = Some(id);
            }
        } else {
            self.nodes.entry(parent).or_default().children.push(id);
        }

        self.update_mtime_range(mtime);

        // A child's events can precede its own `Dir` event, so the node may already exist (holding
        // those children/files); either way only the directory's own fields are (re)set here.
        let node = self.nodes.entry(id).or_default();
        node.parent = parent;
        node.name = name;
        node.direct_size = size;
        node.direct_mtime = mtime;
        node.hue = hue;
    }

    /// A directory's scanned files, added with one lookup of its node.
    fn insert_files(&mut self, parent: u64, files: Vec<ScannedFile>) {
        if files.is_empty() {
            return;
        }
        let entries: Vec<FileEntry> = files
            .into_iter()
            .map(|file| {
                self.update_mtime_range(file.mtime);
                FileEntry {
                    hue: self.cached_extension_hue(&file.name),
                    name: file.name.into_boxed_str(),
                    size: file.size,
                    mtime: file.mtime,
                }
            })
            .collect();
        let node = self.nodes.entry(parent).or_default();
        if node.files.is_empty() {
            node.files = entries;
        } else {
            node.files.extend(entries);
        }
    }

    fn update_mtime_range(&mut self, mtime: i64) {
        if mtime > 0 {
            self.mtime_range.0 = self.mtime_range.0.min(mtime);
            self.mtime_range.1 = self.mtime_range.1.max(mtime);
        }
    }

    fn cached_extension_hue(&mut self, name: &str) -> u16 {
        let extension = match name.rsplit_once('.') {
            Some((_, e)) if !e.is_empty() && e.len() <= 10 => e,
            _ => return 0,
        };
        if let Some(&cached) = self.hue_cache.get(extension) {
            return cached;
        }
        let hue = hue_for_extension(extension);
        self.hue_cache.insert(extension.into(), hue);
        hue
    }

    /// Add `size` to the size of `node_id` and of every directory above it, bring their newest
    /// modification time up to `mtime`, and mark them changed.
    fn propagate(&mut self, node_id: u64, size: u64, mtime: i64) {
        let mut current = node_id;
        while let Some(node) = self.nodes.get_mut(&current) {
            node.size = node.size.saturating_add(size);
            node.mtime = node.mtime.max(mtime);
            node.changed = self.changes;
            if node.parent == 0 {
                break;
            }
            current = node.parent;
        }
    }

    /// The subtree under `root` in pre-order: every directory before its children.
    fn subtree_order(&self, root: u64) -> Vec<u64> {
        let mut stack = vec![root];
        let mut order = Vec::new();
        while let Some(id) = stack.pop() {
            order.push(id);
            if let Some(node) = self.nodes.get(&id) {
                stack.extend(&node.children);
            }
        }
        order
    }

    /// Recompute every directory's `size` and `mtime` bottom-up from the `direct_` ones. The
    /// running totals kept during a scan rely on each directory arriving before its subdirectories
    /// (as the scanner sends them); this makes them exact whatever the order.
    pub fn recompute_totals(&mut self) {
        let Some(root) = self.root_id else { return };
        for id in self.subtree_order(root).into_iter().rev() {
            let Some(node) = self.nodes.get(&id) else { continue };
            let totals = node
                .children
                .iter()
                .filter_map(|child| self.nodes.get(child))
                .fold((node.direct_size, node.direct_mtime), |(size, mtime), child| {
                    (size.saturating_add(child.size), mtime.max(child.mtime))
                });
            if let Some(node) = self.nodes.get_mut(&id) {
                (node.size, node.mtime) = totals;
            }
        }
    }

    /// Totals counting only the files `filter` matches, for the subtree under `root` (the layout
    /// never looks outside the directory being viewed).
    pub fn compute_filtered_sizes(&self, filter: &FilterConfig, root: u64) -> NodeMap<FilteredTotal> {
        let order = self.subtree_order(root);
        let matcher = filter.matcher();
        let mut totals: NodeMap<FilteredTotal> = NodeMap::with_capacity_and_hasher(order.len(), Default::default());
        for &id in order.iter().rev() {
            if let Some(node) = self.nodes.get(&id) {
                let own = node
                    .files
                    .iter()
                    .filter(|file| matcher.matches(&file.name, file.size))
                    .fold(FilteredTotal::default(), |total, file| total.add(file.size, file.mtime));
                let total = node
                    .children
                    .iter()
                    .filter_map(|child| totals.get(child))
                    .fold(own, |total, child| total.add(child.size, child.mtime));
                totals.insert(id, total);
            }
        }
        totals
    }

    pub fn breadcrumb(&self, view_root: u64) -> Vec<BreadcrumbEntry> {
        let root_id = self.root_id.unwrap_or(0);
        let mut path = Vec::new();
        let mut current = view_root;
        // Bound the walk by the node count: a corrupt parent cycle must not hang a request, since
        // the server calls this on every layout.
        for _ in 0..=self.nodes.len() {
            let Some(node) = self.nodes.get(&current) else { break };
            path.push(BreadcrumbEntry {
                id: current,
                name: node.name.to_string(),
            });
            if current == root_id || node.parent == 0 {
                break;
            }
            current = node.parent;
        }
        path.reverse();
        path
    }

    /// Where directory `node_id` is: its names joined onto `root_path`.
    pub fn full_path(&self, node_id: u64) -> Option<PathBuf> {
        let root_id = self.root_id?;
        let mut parts: Vec<&str> = Vec::new();
        let mut current = node_id;
        // Bounded by node count so a corrupt parent cycle can't hang this client-triggered walk.
        for _ in 0..=self.nodes.len() {
            if current == root_id {
                break;
            }
            let node = self.nodes.get(&current)?;
            parts.push(&node.name);
            if node.parent == 0 {
                break;
            }
            current = node.parent;
        }
        let mut path = self.root_path.clone();
        path.extend(parts.iter().rev());
        Some(path)
    }
}

/// Test trees built the way a scan builds them.
#[cfg(test)]
impl DirTree {
    /// Add directory `id` under `parent` with `files` as (name, size, mtime), as a scan's `Dir`
    /// event does: its size is theirs, and reaches every directory above it.
    pub(crate) fn add_dir(&mut self, id: u64, parent: u64, name: &str, files: &[(&str, u64, i64)]) {
        self.apply(ScanEvent::Dir {
            id,
            parent,
            name: name.into(),
            mtime: files.iter().map(|&(_, _, mtime)| mtime).max().unwrap_or(0),
            files: files
                .iter()
                .map(|&(name, size, mtime)| ScannedFile {
                    name: name.into(),
                    size,
                    mtime,
                })
                .collect(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build sample tree:
    ///   root (id=1, size=0)
    ///     dir_a (id=2, size=100)
    ///       dir_c (id=4, size=50)
    ///     dir_b (id=3, size=200)
    fn sample_tree() -> DirTree {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 1000);
        tree.insert_dir(2, 1, "dir_a", 100, 2000);
        tree.insert_dir(3, 1, "dir_b", 200, 3000);
        tree.insert_dir(4, 2, "dir_c", 50, 4000);
        tree
    }

    #[test]
    fn filter_default_is_inactive() {
        let f = FilterConfig::default();
        assert!(!f.is_active());
    }

    #[test]
    fn filter_active_with_extensions() {
        let f = FilterConfig {
            extensions: vec!["rs".into()],
            ..Default::default()
        };
        assert!(f.is_active());
    }

    #[test]
    fn filter_active_with_min_size() {
        let f = FilterConfig {
            min_size: 1,
            ..Default::default()
        };
        assert!(f.is_active());
    }

    #[test]
    fn filter_active_with_name_pattern() {
        let f = FilterConfig {
            name_pattern: "foo".into(),
            ..Default::default()
        };
        assert!(f.is_active());
    }

    #[test]
    fn filter_default_matches_everything() {
        let f = FilterConfig::default();
        assert!(f.matches_file("anything.txt", 0));
        assert!(f.matches_file("anything.txt", u64::MAX));
    }

    #[test]
    fn filter_min_size() {
        let f = FilterConfig {
            min_size: 100,
            ..Default::default()
        };
        assert!(!f.matches_file("a.txt", 99));
        assert!(f.matches_file("a.txt", 100));
        assert!(f.matches_file("a.txt", 101));
    }

    #[test]
    fn filter_extension_case_insensitive() {
        let f = FilterConfig {
            extensions: vec!["rs".into(), "toml".into()],
            ..Default::default()
        };
        assert!(f.matches_file("main.rs", 10));
        assert!(f.matches_file("main.RS", 10));
        assert!(f.matches_file("Cargo.TOML", 10));
        assert!(!f.matches_file("readme.md", 10));
        assert!(!f.matches_file("noext", 10));
    }

    #[test]
    fn filter_name_pattern_case_insensitive_substring() {
        let f = FilterConfig {
            name_pattern: "read".into(),
            ..Default::default()
        };
        assert!(f.matches_file("README.md", 10));
        assert!(f.matches_file("read.txt", 10));
        assert!(f.matches_file("unreadable.rs", 10));
        assert!(!f.matches_file("write.rs", 10));
        let upper = FilterConfig {
            name_pattern: "READ".into(),
            ..Default::default()
        };
        assert!(upper.matches_file("readme.md", 10), "either side's case");
    }

    #[test]
    fn filter_name_pattern_ignores_case_beyond_ascii() {
        let f = FilterConfig {
            name_pattern: "kesä".into(),
            ..Default::default()
        };
        assert!(f.matches_file("KESÄ_2024.jpg", 10));
        assert!(f.matches_file("loma-kesä.png", 10));
        assert!(!f.matches_file("kesa.png", 10));
    }

    /// An extension with dots of its own matches the end of the name, not just its last part.
    #[test]
    fn filter_extensions_with_dots() {
        let f = FilterConfig {
            extensions: vec!["tar.gz".into()],
            ..Default::default()
        };
        assert!(f.matches_file("backup.tar.gz", 10));
        assert!(f.matches_file("BACKUP.TAR.GZ", 10));
        assert!(!f.matches_file("notes.gz", 10));
        assert!(!f.matches_file("xtar.gz", 10), "only after a dot");
        assert!(!f.matches_file("tar.gz", 10), "a name, not an extension");
        let gz = FilterConfig {
            extensions: vec!["gz".into()],
            ..Default::default()
        };
        assert!(gz.matches_file("backup.tar.gz", 10));
        assert!(gz.matches_file(".gz", 10), "a dotfile's name counts, as before");
    }

    #[test]
    fn filter_combined_extension_and_size() {
        let f = FilterConfig {
            extensions: vec!["rs".into()],
            min_size: 100,
            ..Default::default()
        };
        // wrong extension
        assert!(!f.matches_file("a.txt", 200));
        // right extension, too small
        assert!(!f.matches_file("a.rs", 50));
        // both match
        assert!(f.matches_file("a.rs", 100));
    }

    #[test]
    fn a_dir_event_adds_its_files_and_their_total() {
        let file = |name: &str, size, mtime| ScannedFile {
            name: name.into(),
            size,
            mtime,
        };
        let mut tree = DirTree::new();
        tree.apply(ScanEvent::ScanStart { root: "/root".into() });
        tree.apply(ScanEvent::Dir {
            id: 1,
            parent: 0,
            name: "root".into(),
            mtime: 5,
            files: vec![file("a.rs", 10, 3), file("b", 5, 4)],
        });
        tree.apply(ScanEvent::Dir {
            id: 2,
            parent: 1,
            name: "sub".into(),
            mtime: 1,
            files: vec![file("c", 7, 1)],
        });
        assert_eq!(tree.nodes[&1].files.len(), 2);
        assert_eq!(tree.nodes[&1].direct_size, 15);
        assert_eq!(tree.nodes[&2].direct_size, 7);
        assert_eq!(tree.nodes[&1].size, 22, "a subdirectory's size reaches its parent");
        assert_eq!(tree.mtime_range, (1, 5));
    }

    #[test]
    fn each_scan_start_counts_a_scan() {
        let mut tree = DirTree::new();
        assert_eq!(tree.scans, 0);
        for scans in 1..=2 {
            tree.apply(ScanEvent::ScanStart { root: "/root".into() });
            assert_eq!(tree.scans, scans);
        }
    }

    #[test]
    fn only_a_complete_scan_leaves_the_tree_complete() {
        let mut tree = DirTree::new();
        for (outcome, complete) in [
            (ScanOutcome::Complete, true),
            (ScanOutcome::Stalled, false),
            (ScanOutcome::Cancelled, false),
            (ScanOutcome::Incomplete, false),
        ] {
            tree.apply(ScanEvent::ScanStart { root: "/root".into() });
            assert!(!tree.complete, "a running scan isn't complete");
            tree.apply(ScanEvent::ScanDone { outcome });
            assert_eq!(tree.complete, complete, "{outcome:?}");
        }
    }

    #[test]
    fn new_tree_is_empty() {
        let tree = DirTree::new();
        assert!(tree.nodes.is_empty());
        assert!(tree.root_id.is_none());
        assert_eq!(tree.root_path, PathBuf::new());
        assert_eq!(tree.mtime_range, (i64::MAX, 0));
    }

    #[test]
    fn insert_root_dir() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 1000);
        assert_eq!(tree.root_id, Some(1));
        assert!(tree.nodes.contains_key(&1));
        assert_eq!(&*tree.nodes[&1].name, "root");
        assert_eq!(tree.nodes[&1].parent, 0);
    }

    #[test]
    fn insert_child_dirs() {
        let tree = sample_tree();
        // root has two children
        assert_eq!(tree.nodes[&1].children, vec![2, 3]);
        // dir_a has one child
        assert_eq!(tree.nodes[&2].children, vec![4]);
        // dir_b and dir_c have no children
        assert!(tree.nodes[&3].children.is_empty());
        assert!(tree.nodes[&4].children.is_empty());
    }

    #[test]
    fn insert_out_of_order_creates_parent_placeholder() {
        let mut tree = DirTree::new();
        // insert child before parent exists
        tree.insert_dir(5, 2, "child", 10, 100);
        // parent node 2 should exist as a placeholder with child 5
        assert!(tree.nodes.contains_key(&2));
        assert_eq!(tree.nodes[&2].children, vec![5]);
        // now insert the actual parent
        tree.insert_dir(2, 1, "dir_a", 100, 200);
        assert_eq!(&*tree.nodes[&2].name, "dir_a");
        assert_eq!(tree.nodes[&2].direct_size, 100);
        // child link preserved
        assert!(tree.nodes[&2].children.contains(&5));
    }

    #[test]
    fn propagate_size_on_insert() {
        let tree = sample_tree();
        // dir_c(50) is under dir_a(100), both under root(0)
        // root = 0 + 100 + 200 + 50 = 350
        assert_eq!(tree.nodes[&1].size, 350);
        // dir_a = 100 + 50 = 150
        assert_eq!(tree.nodes[&2].size, 150);
        // dir_b = 200
        assert_eq!(tree.nodes[&3].size, 200);
        // dir_c = 50
        assert_eq!(tree.nodes[&4].size, 50);
    }

    #[test]
    fn recompute_totals_matches_propagated() {
        let mut tree = sample_tree();
        for node in tree.nodes.values_mut() {
            node.size = 0;
        }
        tree.recompute_totals();
        assert_eq!(tree.nodes[&1].size, 350);
        assert_eq!(tree.nodes[&2].size, 150);
        assert_eq!(tree.nodes[&3].size, 200);
        assert_eq!(tree.nodes[&4].size, 50);
    }

    /// A directory whose own event arrives after a subdirectory's: the running totals miss the
    /// subdirectory higher up, and `recompute_totals` (run at `ScanDone`) corrects them.
    #[test]
    fn recompute_totals_fixes_out_of_order_inserts() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_dir(3, 2, "grandchild", 50, 300); // before its parent, 2
        tree.insert_dir(2, 1, "child", 10, 200);
        tree.recompute_totals();
        assert_eq!(tree.nodes[&1].size, 60);
        assert_eq!(tree.nodes[&2].size, 60);
        assert_eq!(tree.nodes[&3].size, 50);
        assert_eq!(tree.nodes[&1].mtime, 300);
        assert_eq!(tree.nodes[&2].mtime, 300);
    }

    /// A folder's age is its newest content's, however deep: one holding only subfolders isn't
    /// left without a date.
    #[test]
    fn a_folder_is_as_new_as_anything_in_it() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_dir(2, 1, "only-folders", 0, 0);
        tree.insert_dir(3, 2, "old", 10, 500);
        tree.insert_dir(4, 2, "new", 10, 900);
        assert_eq!(tree.nodes[&2].direct_mtime, 0);
        assert_eq!(tree.nodes[&2].mtime, 900, "kept up during a scan");
        assert_eq!(tree.nodes[&1].mtime, 900);
        tree.recompute_totals();
        assert_eq!(tree.nodes[&2].mtime, 900, "and after it");
        assert_eq!(tree.nodes[&3].mtime, 500);
    }

    #[test]
    fn recompute_totals_empty_tree() {
        let mut tree = DirTree::new();
        tree.recompute_totals();
        assert!(tree.nodes.is_empty());
    }

    #[test]
    fn subtree_order_contains_all_nodes() {
        let tree = sample_tree();
        let order = tree.subtree_order(1);
        assert_eq!(order.len(), 4);
        // root is first (top-down BFS-like via stack)
        assert_eq!(order[0], 1);
        // all nodes present
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, vec![1, 2, 3, 4]);
    }

    #[test]
    fn subtree_order_reversed_is_leaves_first() {
        let tree = sample_tree();
        let order = tree.subtree_order(1);
        // When reversed, leaves must come before their parents.
        let mut rev = order.clone();
        rev.reverse();
        let pos = |id: u64| rev.iter().position(|&x| x == id).unwrap();
        // dir_c before dir_a
        assert!(pos(4) < pos(2));
        // dir_a and dir_b before root
        assert!(pos(2) < pos(1));
        assert!(pos(3) < pos(1));
    }

    /// `sample_tree`, scanned from `/home/user`.
    fn tree_at_home() -> DirTree {
        DirTree {
            root_path: "/home/user".into(),
            ..sample_tree()
        }
    }

    #[test]
    fn full_path_root_returns_root_path() {
        assert_eq!(tree_at_home().full_path(1).unwrap(), PathBuf::from("/home/user"));
    }

    #[test]
    fn full_path_single_level() {
        assert_eq!(tree_at_home().full_path(2).unwrap(), PathBuf::from("/home/user/dir_a"));
    }

    #[test]
    fn full_path_multi_level() {
        assert_eq!(
            tree_at_home().full_path(4).unwrap(),
            PathBuf::from("/home/user/dir_a/dir_c")
        );
    }

    #[test]
    fn full_path_no_root_returns_none() {
        assert!(DirTree::new().full_path(1).is_none());
    }

    #[test]
    fn full_path_missing_node_returns_none() {
        assert!(tree_at_home().full_path(999).is_none());
    }

    #[test]
    fn breadcrumb_from_leaf() {
        let tree = sample_tree();
        let crumbs = tree.breadcrumb(4);
        let names: Vec<&str> = crumbs.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["root", "dir_a", "dir_c"]);
        assert_eq!(crumbs[0].id, 1);
        assert_eq!(crumbs[1].id, 2);
        assert_eq!(crumbs[2].id, 4);
    }

    #[test]
    fn breadcrumb_from_root() {
        let tree = sample_tree();
        let crumbs = tree.breadcrumb(1);
        assert_eq!(crumbs.len(), 1);
        assert_eq!(crumbs[0].id, 1);
        assert_eq!(crumbs[0].name, "root");
    }

    #[test]
    fn a_new_scan_starts_from_an_empty_tree() {
        let mut tree = tree_at_home();
        tree.apply(ScanEvent::ScanStart {
            root: "/mnt/data".into(),
        });
        assert!(tree.nodes.is_empty());
        assert!(tree.root_id.is_none());
        assert_eq!(tree.root_path, PathBuf::from("/mnt/data"));
        assert_eq!(tree.mtime_range, (i64::MAX, 0));
        assert_eq!((tree.scans, tree.changes), (1, 1), "only the counters carry over");
    }

    #[test]
    fn mtime_range_tracks_min_max() {
        let tree = sample_tree();
        // mtimes: 1000, 2000, 3000, 4000
        assert_eq!(tree.mtime_range, (1000, 4000));
    }

    #[test]
    fn mtime_range_ignores_zero() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 0);
        // zero mtime ignored, range stays at initial
        assert_eq!(tree.mtime_range, (i64::MAX, 0));
        tree.insert_dir(2, 1, "child", 10, 500);
        assert_eq!(tree.mtime_range, (500, 500));
    }

    #[test]
    fn mtime_range_ignores_negative() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, -1);
        assert_eq!(tree.mtime_range, (i64::MAX, 0));
        tree.insert_dir(2, 1, "child", 10, 100);
        assert_eq!(tree.mtime_range, (100, 100));
    }

    #[test]
    fn mtime_range_from_files() {
        let mut tree = DirTree::new();
        tree.add_dir(1, 0, "root", &[("a.txt", 10, 100), ("b.txt", 20, 900)]);
        assert_eq!(tree.mtime_range, (100, 900));
    }

    #[test]
    fn compute_filtered_sizes_with_extension_filter() {
        let mut tree = DirTree::new();
        tree.add_dir(1, 0, "root", &[]);
        tree.add_dir(
            2,
            1,
            "src",
            &[("main.rs", 500, 100), ("lib.rs", 300, 200), ("notes.txt", 200, 900)],
        );

        let filter = FilterConfig {
            extensions: vec!["rs".into()],
            ..Default::default()
        };
        let total = |size, mtime| FilteredTotal { size, mtime };
        let sizes = tree.compute_filtered_sizes(&filter, 1);
        // src dir: main.rs(500) + lib.rs(300) = 800, as new as lib.rs: notes.txt doesn't count.
        assert_eq!(sizes[&2], total(800, 200));
        // root: sum of children = 800
        assert_eq!(sizes[&1], total(800, 200));

        // Folding from a subtree covers just that subtree.
        let sizes = tree.compute_filtered_sizes(&filter, 2);
        assert_eq!(sizes.get(&2), Some(&total(800, 200)));
        assert_eq!(sizes.get(&1), None);
    }

    #[test]
    fn compute_filtered_sizes_no_filter_sums_all_files() {
        let mut tree = DirTree::new();
        tree.add_dir(1, 0, "root", &[("a.txt", 100, 100), ("b.rs", 200, 100)]);

        let filter = FilterConfig::default();
        let sizes = tree.compute_filtered_sizes(&filter, 1);
        assert_eq!(sizes[&1].size, 300);
    }
}
