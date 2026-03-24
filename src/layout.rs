use std::collections::HashMap;
use std::path::{Path, PathBuf};

const NEST_PAD: f64 = 2.0;
const NEST_HEADER: f64 = 18.0;
const MIN_NEST_PX: f64 = 40.0;

fn header_height(w: f64, h: f64) -> f64 {
    if w > 60.0 && h > 30.0 {
        let hdr = (h * 0.15).floor().min(NEST_HEADER);
        if hdr >= 12.0 {
            return hdr;
        }
    }
    0.0
}

pub struct FileEntry {
    pub name: Box<str>,
    pub size: u64,
    pub hue: u16,
    pub mtime: i64,
}

#[derive(Default)]
pub struct DirNode {
    pub parent: u64,
    pub name: String,
    pub direct_size: u64,
    pub children: Vec<u64>,
    pub files: Vec<FileEntry>,
    pub hue: u16,
    pub mtime: i64,
}

pub struct DirTree {
    pub nodes: HashMap<u64, DirNode>,
    pub root_id: Option<u64>,
    pub recursive_sizes: HashMap<u64, u64>,
    pub scan_path: String,
    pub mtime_range: (i64, i64),
    hue_cache: HashMap<Box<str>, u16>,
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
    }

    pub fn insert_dir(&mut self, id: u64, parent: u64, name: &str, size: u64, mtime: i64) {
        let hue = hash_name(name);

        if parent == 0 {
            if self.root_id.is_none() {
                self.root_id = Some(id);
            }
        } else {
            let pn = self.nodes.entry(parent).or_default();
            pn.children.push(id);
        }

        if mtime > 0 {
            self.mtime_range.0 = self.mtime_range.0.min(mtime);
            self.mtime_range.1 = self.mtime_range.1.max(mtime);
        }

        match self.nodes.get_mut(&id) {
            Some(existing) => {
                existing.parent = parent;
                existing.name = name.to_string();
                existing.direct_size = size;
                existing.hue = hue;
                existing.mtime = mtime;
            }
            None => {
                self.nodes.insert(
                    id,
                    DirNode {
                        parent,
                        name: name.to_string(),
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

        if mtime > 0 {
            self.mtime_range.0 = self.mtime_range.0.min(mtime);
            self.mtime_range.1 = self.mtime_range.1.max(mtime);
        }

        let node = self.nodes.entry(parent).or_default();
        node.files.push(FileEntry {
            name: name.into(),
            size,
            hue,
            mtime,
        });
    }

    fn cached_extension_hue(&mut self, name: &str) -> u16 {
        let ext = match name.rsplit_once('.') {
            Some((_, e)) if !e.is_empty() && e.len() <= 10 => e,
            _ => return 0,
        };
        if let Some(&h) = self.hue_cache.get(ext) {
            return h;
        }
        let h = hue_for_ext(ext);
        self.hue_cache.insert(ext.into(), h);
        h
    }

    fn propagate_size(&mut self, node_id: u64, delta: u64) {
        let mut cur = node_id;
        loop {
            *self.recursive_sizes.entry(cur).or_insert(0) += delta;
            match self.nodes.get(&cur) {
                Some(n) if n.parent != 0 => cur = n.parent,
                _ => break,
            }
        }
    }

    fn bottom_up_order(&self) -> Vec<u64> {
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
                for f in &node.files {
                    if filter.matches_file(&f.name, f.size) {
                        total += f.size;
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
        let mut cur = view_root;
        while let Some(node) = self.nodes.get(&cur) {
            let name = if node.name.is_empty() {
                self.scan_path.clone()
            } else {
                node.name.clone()
            };
            path.push(BreadcrumbEntry { id: cur, name });
            if cur == root_id || node.parent == 0 {
                break;
            }
            cur = node.parent;
        }
        path.reverse();
        path
    }

    pub fn full_path(&self, node_id: u64, root_path: &Path) -> Option<PathBuf> {
        let root_id = self.root_id?;
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = node_id;
        loop {
            if cur == root_id {
                break;
            }
            let node = self.nodes.get(&cur)?;
            parts.push(&node.name);
            if node.parent == 0 {
                break;
            }
            cur = node.parent;
        }
        parts.reverse();
        let mut path = root_path.to_path_buf();
        for part in parts {
            path.push(part);
        }
        Some(path)
    }
}

/// Hash a name to a hue (0-359) using UTF-16 code units.
pub fn hash_name(name: &str) -> u16 {
    let mut h: i32 = 0;
    for c in name.encode_utf16() {
        h = h.wrapping_shl(5).wrapping_sub(h).wrapping_add(c as i32);
    }
    h.rem_euclid(360) as u16
}

fn hash_id_to_hue(id: u64) -> u16 {
    ((id.wrapping_mul(2654435761) >> 16) % 360) as u16
}

fn hue_for_ext(ext: &str) -> u16 {
    let mime = mime_guess::from_ext(ext).first_or(mime::APPLICATION_OCTET_STREAM);
    match mime.type_().as_str() {
        "video" => 220,     // blue
        "audio" => 280,     // purple
        "image" => 130,     // green
        "text" => 55,       // yellow
        "font" => 310,      // pink
        "application" => 5, // red
        _ => hash_name(ext),
    }
}

pub fn age_hue(mtime: i64, min_t: i64, max_t: i64) -> u16 {
    if max_t <= min_t || mtime <= 0 {
        return 60; // neutral yellow
    }
    let t = ((mtime - min_t) as f64) / ((max_t - min_t) as f64);
    // newest (t=1) → green(120), oldest (t=0) → red(0)
    (t * 120.0) as u16
}

pub const COLOR_MODE_AGE: u8 = 1;

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
            let pat = self.name_pattern.as_bytes();
            if !name.as_bytes().windows(pat.len()).any(|w| w.eq_ignore_ascii_case(pat)) {
                return false;
            }
        }
        if !self.extensions.is_empty() {
            let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
            if !self.extensions.iter().any(|e| e.as_ref().eq_ignore_ascii_case(ext)) {
                return false;
            }
        }
        true
    }
}

pub struct LayoutConfig {
    pub max_depth: u8,
    pub color_mode: u8,
    pub filter: FilterConfig,
    pub mtime_range: (i64, i64),
}

pub struct LayoutRect {
    pub id: i64,
    pub parent_id: u64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub name: String,
    pub hue: u16,
    pub size: u64,
    pub depth: u8,
    pub is_container: bool,
    pub header_h: f64,
    pub is_files: bool,
    pub is_file: bool,
    pub mtime: i64,
}

#[derive(Clone)]
pub struct BreadcrumbEntry {
    pub id: u64,
    pub name: String,
}

pub fn compute_layout(
    tree: &DirTree,
    view_root: u64,
    canvas_w: f64,
    canvas_h: f64,
    config: &LayoutConfig,
) -> Vec<LayoutRect> {
    let filtered;
    let sizes = if config.filter.is_active() {
        filtered = tree.compute_filtered_sizes(&config.filter);
        &filtered
    } else {
        &tree.recursive_sizes
    };

    let pad = NEST_PAD;
    let mut out = Vec::new();
    let mut file_id = -1i64;
    layout_node(
        tree,
        sizes,
        config,
        view_root,
        pad,
        pad,
        canvas_w - pad * 2.0,
        canvas_h - pad * 2.0,
        0,
        &mut out,
        &mut file_id,
    );
    out
}

enum ItemKind<'a> {
    Dir {
        child_id: u64,
    },
    File {
        name: &'a str,
        hue: u16,
        parent_id: u64,
        size: u64,
        mtime: i64,
    },
    Aggregate {
        hue: u16,
        parent_id: u64,
    },
}

#[allow(clippy::too_many_arguments)]
fn layout_node(
    tree: &DirTree,
    sizes: &HashMap<u64, u64>,
    config: &LayoutConfig,
    node_id: u64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    depth: u8,
    out: &mut Vec<LayoutRect>,
    file_id: &mut i64,
) {
    let node = match tree.nodes.get(&node_id) {
        Some(n) => n,
        None => return,
    };

    let pad = if depth > 0 { NEST_PAD } else { 0.0 };
    let hdr = if depth > 0 { header_height(w, h) } else { 0.0 };

    let cx = x + pad;
    let cy = y + hdr;
    let cw = (w - pad * 2.0).max(0.0);
    let ch = (h - hdr - pad).max(0.0);
    if cw < 2.0 || ch < 2.0 {
        return;
    }

    struct LayoutItem<'a> {
        id: i64,
        size: f64,
        kind: ItemKind<'a>,
    }

    let mut layout_items: Vec<LayoutItem<'_>> = Vec::new();
    let filtering = config.filter.is_active();

    for &child_id in &node.children {
        let s = sizes.get(&child_id).copied().unwrap_or(0);
        if s > 0 {
            layout_items.push(LayoutItem {
                id: child_id as i64,
                size: s as f64,
                kind: ItemKind::Dir { child_id },
            });
        }
    }

    let total_size = sizes.get(&node_id).copied().unwrap_or(1) as f64;
    let area = cw * ch;
    let min_file_size = if area > 0.0 && total_size > 0.0 {
        (4.0 / area) * total_size
    } else {
        f64::MAX
    };

    let mut residual: u64 = 0;
    for f in &node.files {
        if f.size == 0 {
            continue;
        }
        if filtering && !config.filter.matches_file(&f.name, f.size) {
            continue;
        }
        if (f.size as f64) >= min_file_size {
            let id = *file_id;
            *file_id -= 1;
            layout_items.push(LayoutItem {
                id,
                size: f.size as f64,
                kind: ItemKind::File {
                    name: &f.name,
                    hue: f.hue,
                    parent_id: node_id,
                    size: f.size,
                    mtime: f.mtime,
                },
            });
        } else {
            residual += f.size;
        }
    }

    if residual > 0 {
        let id = *file_id;
        *file_id -= 1;
        let hue = hash_id_to_hue(node_id);
        layout_items.push(LayoutItem {
            id,
            size: residual as f64,
            kind: ItemKind::Aggregate {
                hue,
                parent_id: node_id,
            },
        });
    }

    if layout_items.is_empty() {
        return;
    }
    layout_items.sort_unstable_by(|a, b| b.size.partial_cmp(&a.size).unwrap());

    let squarify_items: Vec<(i64, f64)> = layout_items.iter().map(|i| (i.id, i.size)).collect();
    let mut rects = Vec::new();
    squarify(&squarify_items, cx, cy, cw, ch, &mut rects);

    let (min_t, max_t) = config.mtime_range;

    for (raw, item) in rects.iter().zip(layout_items.iter()) {
        match &item.kind {
            ItemKind::Dir { child_id } => {
                let cn = tree.nodes.get(child_id);
                let name = cn.map_or_else(|| "?".to_string(), |n| n.name.clone());
                let mut hue = cn.map_or(0, |n| n.hue);
                let size = sizes.get(child_id).copied().unwrap_or(0);
                let mtime = cn.map_or(0, |n| n.mtime);

                if config.color_mode == COLOR_MODE_AGE {
                    hue = age_hue(mtime, min_t, max_t);
                }

                let can_nest = depth < config.max_depth
                    && raw.w >= MIN_NEST_PX
                    && raw.h >= MIN_NEST_PX
                    && cn.is_some_and(|n| !n.children.is_empty());

                let (is_container, hdr_h) = if can_nest {
                    (true, header_height(raw.w, raw.h))
                } else {
                    (false, 0.0)
                };
                out.push(LayoutRect {
                    id: raw.id,
                    parent_id: node_id,
                    x: raw.x,
                    y: raw.y,
                    w: raw.w,
                    h: raw.h,
                    name,
                    hue,
                    size,
                    depth,
                    is_container,
                    header_h: hdr_h,
                    is_files: false,
                    is_file: false,
                    mtime,
                });
                if can_nest {
                    layout_node(
                        tree,
                        sizes,
                        config,
                        *child_id,
                        raw.x,
                        raw.y,
                        raw.w,
                        raw.h,
                        depth + 1,
                        out,
                        file_id,
                    );
                }
            }
            ItemKind::File {
                name,
                hue,
                parent_id,
                size,
                mtime,
            } => {
                let final_hue = if config.color_mode == COLOR_MODE_AGE {
                    age_hue(*mtime, min_t, max_t)
                } else {
                    *hue
                };
                out.push(LayoutRect {
                    id: raw.id,
                    parent_id: *parent_id,
                    x: raw.x,
                    y: raw.y,
                    w: raw.w,
                    h: raw.h,
                    name: name.to_string(),
                    hue: final_hue,
                    size: *size,
                    depth,
                    is_container: false,
                    header_h: 0.0,
                    is_files: false,
                    is_file: true,
                    mtime: *mtime,
                });
            }
            ItemKind::Aggregate { hue, parent_id } => {
                out.push(LayoutRect {
                    id: raw.id,
                    parent_id: *parent_id,
                    x: raw.x,
                    y: raw.y,
                    w: raw.w,
                    h: raw.h,
                    name: "(other files)".to_string(),
                    hue: *hue,
                    size: residual,
                    depth,
                    is_container: false,
                    header_h: 0.0,
                    is_files: true,
                    is_file: false,
                    mtime: 0,
                });
            }
        }
    }
}

struct RawRect {
    id: i64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn squarify(items: &[(i64, f64)], x: f64, y: f64, w: f64, h: f64, out: &mut Vec<RawRect>) {
    if items.is_empty() || w <= 0.0 || h <= 0.0 {
        return;
    }
    let total: f64 = items.iter().map(|i| i.1).sum();
    if total <= 0.0 {
        return;
    }
    squarify_slice(items, 0, items.len(), x, y, w, h, total, out);
}

#[allow(clippy::too_many_arguments)]
fn squarify_slice(
    items: &[(i64, f64)],
    lo: usize,
    hi: usize,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    area_left: f64,
    out: &mut Vec<RawRect>,
) {
    if lo >= hi || w <= 0.0 || h <= 0.0 || area_left <= 0.0 {
        return;
    }
    if hi - lo == 1 {
        out.push(RawRect {
            id: items[lo].0,
            x,
            y,
            w,
            h,
        });
        return;
    }

    let vertical = w >= h;
    let short_side = if vertical { h } else { w };
    let scale = (w * h) / area_left;

    let mut row_area = 0.0_f64;
    let mut best_worst = f64::INFINITY;
    let mut split = lo;

    for i in lo..hi {
        let test_area = row_area + items[i].1;
        let test_len = (test_area * scale) / short_side;
        if test_len <= 0.0 {
            row_area = test_area;
            split = i + 1;
            continue;
        }

        // Worst AR is from the largest (lo) or smallest (i) item since items are sorted descending
        let worst = {
            let first = (items[lo].1 * scale) / test_len;
            let last = (items[i].1 * scale) / test_len;
            let ar_first = if first > 0.0 {
                (test_len / first).max(first / test_len)
            } else {
                0.0
            };
            let ar_last = if last > 0.0 {
                (test_len / last).max(last / test_len)
            } else {
                0.0
            };
            ar_first.max(ar_last)
        };

        if worst <= best_worst {
            best_worst = worst;
            row_area = test_area;
            split = i + 1;
        } else {
            break;
        }
    }

    if split == lo {
        split = lo + 1;
    }
    let row_frac = row_area / area_left;

    let row_items = &items[lo..split];
    if vertical {
        let row_w = w * row_frac;
        let mut cy = y;
        for (i, item) in row_items.iter().enumerate() {
            let item_h = if i == row_items.len() - 1 {
                y + h - cy
            } else {
                (item.1 / row_area) * h
            };
            out.push(RawRect {
                id: item.0,
                x,
                y: cy,
                w: row_w,
                h: item_h,
            });
            cy += item_h;
        }
        squarify_slice(items, split, hi, x + row_w, y, w - row_w, h, area_left - row_area, out);
    } else {
        let row_h = h * row_frac;
        let mut cx = x;
        for (i, item) in row_items.iter().enumerate() {
            let item_w = if i == row_items.len() - 1 {
                x + w - cx
            } else {
                (item.1 / row_area) * w
            };
            out.push(RawRect {
                id: item.0,
                x: cx,
                y,
                w: item_w,
                h: row_h,
            });
            cx += item_w;
        }
        squarify_slice(items, split, hi, x, y + row_h, w, h - row_h, area_left - row_area, out);
    }
}
