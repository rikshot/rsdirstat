use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::color::{hash_name, hue_for_extension};

pub struct FileEntry {
    pub name: Box<str>,
    pub size: u64,
    pub hue: u16,
    pub mtime: i64,
}

#[derive(Default)]
pub struct DirNode {
    pub parent: u64,
    pub name: Box<str>,
    pub direct_size: u64,
    pub children: Vec<u64>,
    pub files: Vec<FileEntry>,
    pub hue: u16,
    pub mtime: i64,
}

#[derive(Clone, Default)]
pub struct FilterConfig {
    pub extensions: Vec<Box<str>>,
    pub min_size: u64,
    pub max_size: u64,
    pub name_pattern: String,
}

impl FilterConfig {
    pub fn is_active(&self) -> bool {
        !self.extensions.is_empty() || self.min_size > 0 || self.max_size > 0 || !self.name_pattern.is_empty()
    }

    pub fn matches_file(&self, name: &str, size: u64) -> bool {
        if self.min_size > 0 && size < self.min_size {
            return false;
        }
        if self.max_size > 0 && size > self.max_size {
            return false;
        }
        if !self.name_pattern.is_empty() {
            let pattern = self.name_pattern.as_bytes();
            if !name
                .as_bytes()
                .windows(pattern.len())
                .any(|window| window.eq_ignore_ascii_case(pattern))
            {
                return false;
            }
        }
        if !self.extensions.is_empty() {
            let extension = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
            if !self
                .extensions
                .iter()
                .any(|e| e.as_ref().eq_ignore_ascii_case(extension))
            {
                return false;
            }
        }
        true
    }
}

#[derive(Clone)]
pub struct BreadcrumbEntry {
    pub id: u64,
    pub name: String,
}

pub struct DirTree {
    pub nodes: HashMap<u64, DirNode>,
    pub root_id: Option<u64>,
    pub recursive_sizes: HashMap<u64, u64>,
    pub scan_path: String,
    pub mtime_range: (i64, i64),
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
            nodes: HashMap::new(),
            root_id: None,
            recursive_sizes: HashMap::new(),
            scan_path: String::new(),
            mtime_range: (i64::MAX, 0),
            hue_cache: HashMap::new(),
        }
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
        self.root_id = None;
        self.recursive_sizes.clear();
        self.scan_path.clear();
        self.mtime_range = (i64::MAX, 0);
        self.hue_cache.clear();
    }

    pub fn insert_dir(&mut self, id: u64, parent: u64, name: &str, size: u64, mtime: i64) {
        let hue = hash_name(name);

        if parent == 0 {
            if self.root_id.is_none() {
                self.root_id = Some(id);
            }
        } else {
            let parent_node = self.nodes.entry(parent).or_default();
            parent_node.children.push(id);
        }

        self.update_mtime_range(mtime);

        match self.nodes.get_mut(&id) {
            Some(existing) => {
                existing.parent = parent;
                existing.name = name.into();
                existing.direct_size = size;
                existing.hue = hue;
                existing.mtime = mtime;
            }
            None => {
                self.nodes.insert(
                    id,
                    DirNode {
                        parent,
                        name: name.into(),
                        direct_size: size,
                        children: Vec::new(),
                        files: Vec::new(),
                        hue,
                        mtime,
                    },
                );
            }
        }

        self.propagate_size(id, size);
    }

    pub fn insert_file(&mut self, parent: u64, name: &str, size: u64, mtime: i64) {
        let hue = self.cached_extension_hue(name);
        self.update_mtime_range(mtime);

        let node = self.nodes.entry(parent).or_default();
        node.files.push(FileEntry {
            name: name.into(),
            size,
            hue,
            mtime,
        });
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

    fn propagate_size(&mut self, node_id: u64, delta: u64) {
        let mut current = node_id;
        loop {
            *self.recursive_sizes.entry(current).or_insert(0) += delta;
            match self.nodes.get(&current) {
                Some(node) if node.parent != 0 => current = node.parent,
                _ => break,
            }
        }
    }

    pub fn bottom_up_order(&self) -> Vec<u64> {
        let root_id = match self.root_id {
            Some(id) => id,
            None => return Vec::new(),
        };
        let mut stack = vec![root_id];
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(id) = stack.pop() {
            order.push(id);
            if let Some(node) = self.nodes.get(&id) {
                for &child in &node.children {
                    stack.push(child);
                }
            }
        }
        order
    }

    pub fn recompute_sizes(&mut self) {
        self.recursive_sizes.clear();
        let order = self.bottom_up_order();
        for &id in order.iter().rev() {
            if let Some(node) = self.nodes.get(&id) {
                let mut total = node.direct_size;
                for &child in &node.children {
                    total += self.recursive_sizes.get(&child).copied().unwrap_or(0);
                }
                self.recursive_sizes.insert(id, total);
            }
        }
    }

    pub fn compute_filtered_sizes(&self, filter: &FilterConfig) -> HashMap<u64, u64> {
        let order = self.bottom_up_order();
        let mut sizes = HashMap::with_capacity(order.len());
        for &id in order.iter().rev() {
            if let Some(node) = self.nodes.get(&id) {
                let mut total: u64 = 0;
                for file in &node.files {
                    if filter.matches_file(&file.name, file.size) {
                        total += file.size;
                    }
                }
                for &child in &node.children {
                    total += sizes.get(&child).copied().unwrap_or(0);
                }
                sizes.insert(id, total);
            }
        }
        sizes
    }

    pub fn breadcrumb(&self, view_root: u64) -> Vec<BreadcrumbEntry> {
        let root_id = self.root_id.unwrap_or(0);
        let mut path = Vec::new();
        let mut current = view_root;
        while let Some(node) = self.nodes.get(&current) {
            let name = if node.name.is_empty() {
                self.scan_path.clone()
            } else {
                node.name.to_string()
            };
            path.push(BreadcrumbEntry { id: current, name });
            if current == root_id || node.parent == 0 {
                break;
            }
            current = node.parent;
        }
        path.reverse();
        path
    }

    pub fn full_path(&self, node_id: u64, root_path: &Path) -> Option<PathBuf> {
        let root_id = self.root_id?;
        let mut parts: Vec<&str> = Vec::new();
        let mut current = node_id;
        loop {
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
        parts.reverse();
        let mut path = root_path.to_path_buf();
        for part in parts {
            path.push(part);
        }
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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
    fn filter_active_with_max_size() {
        let f = FilterConfig {
            max_size: 100,
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
    fn filter_max_size() {
        let f = FilterConfig {
            max_size: 100,
            ..Default::default()
        };
        assert!(f.matches_file("a.txt", 99));
        assert!(f.matches_file("a.txt", 100));
        assert!(!f.matches_file("a.txt", 101));
    }

    #[test]
    fn filter_size_range() {
        let f = FilterConfig {
            min_size: 50,
            max_size: 150,
            ..Default::default()
        };
        assert!(!f.matches_file("a.txt", 49));
        assert!(f.matches_file("a.txt", 50));
        assert!(f.matches_file("a.txt", 100));
        assert!(f.matches_file("a.txt", 150));
        assert!(!f.matches_file("a.txt", 151));
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
    fn new_tree_is_empty() {
        let tree = DirTree::new();
        assert!(tree.nodes.is_empty());
        assert!(tree.root_id.is_none());
        assert!(tree.recursive_sizes.is_empty());
        assert!(tree.scan_path.is_empty());
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
    fn insert_file_adds_to_parent() {
        let mut tree = sample_tree();
        tree.insert_file(2, "hello.rs", 75, 5000);
        tree.insert_file(2, "world.txt", 25, 5001);
        assert_eq!(tree.nodes[&2].files.len(), 2);
        assert_eq!(&*tree.nodes[&2].files[0].name, "hello.rs");
        assert_eq!(tree.nodes[&2].files[0].size, 75);
        assert_eq!(&*tree.nodes[&2].files[1].name, "world.txt");
    }

    #[test]
    fn propagate_size_on_insert() {
        let tree = sample_tree();
        // dir_c(50) is under dir_a(100), both under root(0)
        // root = 0 + 100 + 200 + 50 = 350
        assert_eq!(tree.recursive_sizes[&1], 350);
        // dir_a = 100 + 50 = 150
        assert_eq!(tree.recursive_sizes[&2], 150);
        // dir_b = 200
        assert_eq!(tree.recursive_sizes[&3], 200);
        // dir_c = 50
        assert_eq!(tree.recursive_sizes[&4], 50);
    }

    #[test]
    fn recompute_sizes_matches_propagated() {
        let mut tree = sample_tree();
        // Manually corrupt recursive_sizes, then recompute
        tree.recursive_sizes.clear();
        tree.recompute_sizes();
        assert_eq!(tree.recursive_sizes[&1], 350);
        assert_eq!(tree.recursive_sizes[&2], 150);
        assert_eq!(tree.recursive_sizes[&3], 200);
        assert_eq!(tree.recursive_sizes[&4], 50);
    }

    #[test]
    fn recompute_sizes_empty_tree() {
        let mut tree = DirTree::new();
        tree.recompute_sizes();
        assert!(tree.recursive_sizes.is_empty());
    }

    #[test]
    fn bottom_up_order_contains_all_nodes() {
        let tree = sample_tree();
        let order = tree.bottom_up_order();
        assert_eq!(order.len(), 4);
        // root is first (top-down BFS-like via stack)
        assert_eq!(order[0], 1);
        // all nodes present
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, vec![1, 2, 3, 4]);
    }

    #[test]
    fn bottom_up_order_empty_tree() {
        let tree = DirTree::new();
        assert!(tree.bottom_up_order().is_empty());
    }

    #[test]
    fn bottom_up_order_reversed_is_leaves_first() {
        let tree = sample_tree();
        let order = tree.bottom_up_order();
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

    #[test]
    fn full_path_root_returns_root_path() {
        let tree = sample_tree();
        let p = tree.full_path(1, Path::new("/home/user")).unwrap();
        assert_eq!(p, PathBuf::from("/home/user"));
    }

    #[test]
    fn full_path_single_level() {
        let tree = sample_tree();
        let p = tree.full_path(2, Path::new("/home/user")).unwrap();
        assert_eq!(p, PathBuf::from("/home/user/dir_a"));
    }

    #[test]
    fn full_path_multi_level() {
        let tree = sample_tree();
        let p = tree.full_path(4, Path::new("/home/user")).unwrap();
        assert_eq!(p, PathBuf::from("/home/user/dir_a/dir_c"));
    }

    #[test]
    fn full_path_no_root_returns_none() {
        let tree = DirTree::new();
        assert!(tree.full_path(1, Path::new("/tmp")).is_none());
    }

    #[test]
    fn full_path_missing_node_returns_none() {
        let tree = sample_tree();
        assert!(tree.full_path(999, Path::new("/tmp")).is_none());
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
    fn breadcrumb_root_uses_scan_path_when_name_empty() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "", 0, 100);
        tree.scan_path = "/mnt/data".into();
        let crumbs = tree.breadcrumb(1);
        assert_eq!(crumbs[0].name, "/mnt/data");
    }

    #[test]
    fn clear_resets_everything() {
        let mut tree = sample_tree();
        tree.scan_path = "/some/path".into();
        tree.clear();
        assert!(tree.nodes.is_empty());
        assert!(tree.root_id.is_none());
        assert!(tree.recursive_sizes.is_empty());
        assert!(tree.scan_path.is_empty());
        assert_eq!(tree.mtime_range, (i64::MAX, 0));
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
        tree.insert_dir(1, 0, "root", 0, 500);
        tree.insert_file(1, "a.txt", 10, 100);
        tree.insert_file(1, "b.txt", 20, 900);
        assert_eq!(tree.mtime_range, (100, 900));
    }

    #[test]
    fn compute_filtered_sizes_with_extension_filter() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_dir(2, 1, "src", 0, 100);
        tree.insert_file(2, "main.rs", 500, 100);
        tree.insert_file(2, "lib.rs", 300, 100);
        tree.insert_file(2, "notes.txt", 200, 100);

        let filter = FilterConfig {
            extensions: vec!["rs".into()],
            ..Default::default()
        };
        let sizes = tree.compute_filtered_sizes(&filter);
        // src dir: main.rs(500) + lib.rs(300) = 800
        assert_eq!(sizes[&2], 800);
        // root: sum of children = 800
        assert_eq!(sizes[&1], 800);
    }

    #[test]
    fn compute_filtered_sizes_no_filter_sums_all_files() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_file(1, "a.txt", 100, 100);
        tree.insert_file(1, "b.rs", 200, 100);

        let filter = FilterConfig::default();
        let sizes = tree.compute_filtered_sizes(&filter);
        assert_eq!(sizes[&1], 300);
    }
}
