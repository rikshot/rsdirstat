use std::collections::HashMap;

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

pub struct DirNode {
    pub parent: u64,
    pub name: String,
    pub direct_size: u64,
    pub children: Vec<u64>,
    pub hue: u16,
}

pub struct DirTree {
    pub nodes: HashMap<u64, DirNode>,
    pub root_id: Option<u64>,
    pub recursive_sizes: HashMap<u64, u64>,
    pub scan_path: String,
}

impl DirTree {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            root_id: None,
            recursive_sizes: HashMap::new(),
            scan_path: String::new(),
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
            let pn = self.nodes.entry(parent).or_insert_with(|| DirNode {
                parent: 0,
                name: String::new(),
                direct_size: 0,
                children: Vec::new(),
                hue: 0,
            });
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
                        hue,
                    },
                );
            }
        }

        self.propagate_size(id, size);
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
        loop {
            if let Some(node) = self.nodes.get(&cur) {
                let name = if node.name.is_empty() {
                    self.scan_path.clone()
                } else {
                    node.name.clone()
                };
                path.push(BreadcrumbEntry { id: cur, name });
                if cur == root_id {
                    break;
                }
                if node.parent == 0 {
                    break;
                }
                cur = node.parent;
            } else {
                break;
            }
        }
        path.reverse();
        path
    }
}

/// Hash a name to a hue (0-359) using UTF-16 code units.
pub fn hash_name(name: &str) -> u16 {
    let mut h: i32 = 0;
    for c in name.encode_utf16() {
        h = h.wrapping_shl(5).wrapping_sub(h).wrapping_add(c as i32);
    }
    (((h % 360) + 360) % 360) as u16
}

pub struct LayoutRect {
    pub id: i64,
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
) -> Vec<LayoutRect> {
    let pad = 2.0;
    let mut out = Vec::new();
    layout_node(
        tree,
        view_root,
        pad,
        pad,
        canvas_w - pad * 2.0,
        canvas_h - pad * 2.0,
        0,
        &mut out,
    );
    out
}

fn layout_node(
    tree: &DirTree,
    node_id: u64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    depth: u8,
    out: &mut Vec<LayoutRect>,
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

    let mut items: Vec<(i64, f64)> = Vec::new();
    for &child_id in &node.children {
        let s = tree.recursive_sizes.get(&child_id).copied().unwrap_or(0);
        if s > 0 {
            items.push((child_id as i64, s as f64));
        }
    }
    if node.direct_size > 0 {
        items.push((-(node_id as i64 + 1), node.direct_size as f64));
    }
    if items.is_empty() {
        return;
    }
    items.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    let mut rects = Vec::new();
    squarify(&items, cx, cy, cw, ch, &mut rects);

    for raw in rects {
        if raw.id < 0 {
            let orig_id = -(raw.id + 1) as u64;
            let fhue = hash_name(&format!("__files__{orig_id}"));
            out.push(LayoutRect {
                id: raw.id,
                x: raw.x,
                y: raw.y,
                w: raw.w,
                h: raw.h,
                name: "(files)".to_string(),
                hue: fhue,
                size: node.direct_size,
                depth,
                is_container: false,
                header_h: 0.0,
                is_files: true,
            });
        } else {
            let child_id = raw.id as u64;
            let cn = tree.nodes.get(&child_id);
            let name = cn.map_or_else(|| "?".to_string(), |n| n.name.clone());
            let hue = cn.map_or(0, |n| n.hue);
            let size = tree.recursive_sizes.get(&child_id).copied().unwrap_or(0);

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
            });
            if can_nest {
                layout_node(tree, child_id, raw.x, raw.y, raw.w, raw.h, depth + 1, out);
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

        let mut worst = 0.0_f64;
        for j in lo..=i {
            let item_side = (items[j].1 * scale) / test_len;
            if item_side <= 0.0 {
                continue;
            }
            let ar = if test_len > item_side {
                test_len / item_side
            } else {
                item_side / test_len
            };
            if ar > worst {
                worst = ar;
            }
        }

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

    if vertical {
        let row_w = w * row_frac;
        let mut cy = y;
        for k in lo..split {
            let item_h = if k == split - 1 {
                y + h - cy
            } else {
                (items[k].1 / row_area) * h
            };
            out.push(RawRect {
                id: items[k].0,
                x,
                y: cy,
                w: row_w,
                h: item_h,
            });
            cy += item_h;
        }
        squarify_slice(
            items,
            split,
            hi,
            x + row_w,
            y,
            w - row_w,
            h,
            area_left - row_area,
            out,
        );
    } else {
        let row_h = h * row_frac;
        let mut cx = x;
        for k in lo..split {
            let item_w = if k == split - 1 {
                x + w - cx
            } else {
                (items[k].1 / row_area) * w
            };
            out.push(RawRect {
                id: items[k].0,
                x: cx,
                y,
                w: item_w,
                h: row_h,
            });
            cx += item_w;
        }
        squarify_slice(
            items,
            split,
            hi,
            x,
            y + row_h,
            w,
            h - row_h,
            area_left - row_area,
            out,
        );
    }
}
