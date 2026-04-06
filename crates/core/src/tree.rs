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
