use std::collections::HashMap;

use crate::color::{COLOR_MODE_AGE, age_hue, hash_id_to_hue};
pub use crate::tree::{BreadcrumbEntry, DirTree, FilterConfig};

const NEST_HEADER: f64 = 18.0;
const MIN_NEST_PX: f64 = 40.0;

fn header_height(width: f64, height: f64) -> f64 {
    if width > 60.0 && height > 30.0 {
        let header = (height * 0.15).floor().min(NEST_HEADER);
        if header >= 12.0 {
            return header;
        }
    }
    0.0
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
    pub header_height: f64,
    pub is_files: bool,
    pub is_file: bool,
    pub mtime: i64,
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

    let mut out = Vec::new();
    let mut file_id = -1i64;
    layout_node(
        tree,
        sizes,
        config,
        view_root,
        0.0,
        0.0,
        canvas_w,
        canvas_h,
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
        Some(node) => node,
        None => return,
    };

    let header = if depth > 0 { header_height(w, h) } else { 0.0 };
    let content_y = y + header;
    let content_h = (h - header).max(0.0);
    if w < 2.0 || content_h < 2.0 {
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
        let child_size = sizes.get(&child_id).copied().unwrap_or(0);
        if child_size > 0 {
            layout_items.push(LayoutItem {
                id: child_id as i64,
                size: child_size as f64,
                kind: ItemKind::Dir { child_id },
            });
        }
    }

    let total_size = sizes.get(&node_id).copied().unwrap_or(1) as f64;
    let area = w * content_h;
    let min_file_size = if area > 0.0 && total_size > 0.0 {
        (4.0 / area) * total_size
    } else {
        f64::MAX
    };

    let mut residual: u64 = 0;
    for file in &node.files {
        if file.size == 0 {
            continue;
        }
        if filtering && !config.filter.matches_file(&file.name, file.size) {
            continue;
        }
        if (file.size as f64) >= min_file_size {
            let id = *file_id;
            *file_id -= 1;
            layout_items.push(LayoutItem {
                id,
                size: file.size as f64,
                kind: ItemKind::File {
                    name: &file.name,
                    hue: file.hue,
                    parent_id: node_id,
                    size: file.size,
                    mtime: file.mtime,
                },
            });
        } else {
            residual += file.size;
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

    let squarify_items: Vec<(i64, f64)> = layout_items.iter().map(|item| (item.id, item.size)).collect();
    let mut rects = Vec::new();
    squarify(&squarify_items, x, content_y, w, content_h, &mut rects);

    let (min_time, max_time) = config.mtime_range;

    for (raw, item) in rects.iter().zip(layout_items.iter()) {
        match &item.kind {
            ItemKind::Dir { child_id } => {
                let child_node = tree.nodes.get(child_id);
                let name = child_node.map_or_else(|| "?".to_string(), |n| n.name.to_string());
                let mut hue = child_node.map_or(0, |n| n.hue);
                let size = sizes.get(child_id).copied().unwrap_or(0);
                let mtime = child_node.map_or(0, |n| n.mtime);

                if config.color_mode == COLOR_MODE_AGE {
                    hue = age_hue(mtime, min_time, max_time);
                }

                let can_nest = depth < config.max_depth
                    && raw.w >= MIN_NEST_PX
                    && raw.h >= MIN_NEST_PX
                    && child_node.is_some_and(|n| !n.children.is_empty());

                let (is_container, header_height_value) = if can_nest {
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
                    header_height: header_height_value,
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
                    age_hue(*mtime, min_time, max_time)
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
                    header_height: 0.0,
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
                    header_height: 0.0,
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
    let total: f64 = items.iter().map(|item| item.1).sum();
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
            let ih = if i == row_items.len() - 1 {
                y + h - cy
            } else {
                (item.1 / row_area) * h
            };
            out.push(RawRect {
                id: item.0,
                x,
                y: cy,
                w: row_w,
                h: ih,
            });
            cy += ih;
        }
        squarify_slice(items, split, hi, x + row_w, y, w - row_w, h, area_left - row_area, out);
    } else {
        let row_h = h * row_frac;
        let mut cx = x;
        for (i, item) in row_items.iter().enumerate() {
            let iw = if i == row_items.len() - 1 {
                x + w - cx
            } else {
                (item.1 / row_area) * w
            };
            out.push(RawRect {
                id: item.0,
                x: cx,
                y,
                w: iw,
                h: row_h,
            });
            cx += iw;
        }
        squarify_slice(items, split, hi, x, y + row_h, w, h - row_h, area_left - row_area, out);
    }
}
