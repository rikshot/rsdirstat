use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAX_NEST_DEPTH: u8 = 5;
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
    pub name: String,
    pub size: u64,
    pub hue: u16,
}

#[derive(Default)]
pub struct DirNode {
    pub parent: u64,
    pub name: String,
    pub direct_size: u64,
    pub children: Vec<u64>,
    pub files: Vec<FileEntry>,
    pub hue: u16,
}

pub struct DirTree {
    pub nodes: HashMap<u64, DirNode>,
    pub root_id: Option<u64>,
    pub recursive_sizes: HashMap<u64, u64>,
    pub scan_path: String,
    hue_cache: HashMap<Box<str>, u16>,
}

impl DirTree {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            root_id: None,
            recursive_sizes: HashMap::new(),
            scan_path: String::new(),
            hue_cache: HashMap::new(),
        }
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
        self.root_id = None;
        self.recursive_sizes.clear();
        self.scan_path.clear();
    }

    pub fn insert_dir(&mut self, id: u64, parent: u64, name: &str, size: u64) {
        let hue = hash_name(name);

        if parent == 0 {
            if self.root_id.is_none() {
                self.root_id = Some(id);
            }
        } else {
            let pn = self.nodes.entry(parent).or_default();
            pn.children.push(id);
        }

        match self.nodes.get_mut(&id) {
            Some(existing) => {
                existing.parent = parent;
                existing.name = name.to_string();
                existing.direct_size = size;
                existing.hue = hue;
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
                    },
                );
            }
        }

        self.propagate_size(id, size);
    }

    pub fn insert_file(&mut self, parent: u64, name: &str, size: u64) {
        let hue = self.cached_extension_hue(name);
        let node = self.nodes.entry(parent).or_default();
        node.files.push(FileEntry {
            name: name.to_string(),
            size,
            hue,
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

    pub fn recompute_sizes(&mut self) {
        self.recursive_sizes.clear();
        let root_id = match self.root_id {
            Some(id) => id,
            None => return,
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
}

#[derive(Clone)]
pub struct BreadcrumbEntry {
    pub id: u64,
    pub name: String,
}

pub fn compute_layout(tree: &DirTree, view_root: u64, canvas_w: f64, canvas_h: f64) -> Vec<LayoutRect> {
    let pad = NEST_PAD;
    let mut out = Vec::new();
    let mut file_id = -1i64;
    layout_node(
        tree,
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
    },
    Aggregate {
        hue: u16,
        parent_id: u64,
    },
}

#[allow(clippy::too_many_arguments)]
fn layout_node(
    tree: &DirTree,
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

    // Build items: child directories + individual files + residual aggregate
    struct LayoutItem<'a> {
        id: i64,
        size: f64,
        kind: ItemKind<'a>,
    }

    let mut layout_items: Vec<LayoutItem<'_>> = Vec::new();

    for &child_id in &node.children {
        let s = tree.recursive_sizes.get(&child_id).copied().unwrap_or(0);
        if s > 0 {
            layout_items.push(LayoutItem {
                id: child_id as i64,
                size: s as f64,
                kind: ItemKind::Dir { child_id },
            });
        }
    }

    let total_size = tree.recursive_sizes.get(&node_id).copied().unwrap_or(1) as f64;
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

    for (raw, item) in rects.iter().zip(layout_items.iter()) {
        match &item.kind {
            ItemKind::Dir { child_id } => {
                let cn = tree.nodes.get(child_id);
                let name = cn.map_or_else(|| "?".to_string(), |n| n.name.clone());
                let hue = cn.map_or(0, |n| n.hue);
                let size = tree.recursive_sizes.get(child_id).copied().unwrap_or(0);

                let can_nest = depth < MAX_NEST_DEPTH
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
                });
                if can_nest {
                    layout_node(tree, *child_id, raw.x, raw.y, raw.w, raw.h, depth + 1, out, file_id);
                }
            }
            ItemKind::File {
                name,
                hue,
                parent_id,
                size,
            } => {
                out.push(LayoutRect {
                    id: raw.id,
                    parent_id: *parent_id,
                    x: raw.x,
                    y: raw.y,
                    w: raw.w,
                    h: raw.h,
                    name: name.to_string(),
                    hue: *hue,
                    size: *size,
                    depth,
                    is_container: false,
                    header_h: 0.0,
                    is_files: false,
                    is_file: true,
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
