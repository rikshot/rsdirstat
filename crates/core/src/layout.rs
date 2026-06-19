use std::collections::HashMap;

use crate::color::{COLOR_MODE_AGE, age_hue, hash_id_to_hue};
pub use crate::tree::{BreadcrumbEntry, DirTree, FilterConfig};
pub use rsdirstat_protocol::LayoutRect;

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
    layout_items.sort_unstable_by(|a, b| b.size.total_cmp(&a.size));

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
            // Last item fills the remainder; clamp so float drift can't yield a negative extent.
            let ih = if i == row_items.len() - 1 {
                (y + h - cy).max(0.0)
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
            // Last item fills the remainder; clamp so float drift can't yield a negative extent.
            let iw = if i == row_items.len() - 1 {
                (x + w - cx).max(0.0)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> LayoutConfig {
        LayoutConfig {
            max_depth: 3,
            color_mode: 0,
            filter: FilterConfig::default(),
            mtime_range: (0, 0),
        }
    }

    #[test]
    fn header_height_zero_for_small_rect() {
        assert_eq!(header_height(60.0, 100.0), 0.0);
        assert_eq!(header_height(100.0, 30.0), 0.0);
        assert_eq!(header_height(50.0, 20.0), 0.0);
    }

    #[test]
    fn header_height_capped_at_18() {
        assert_eq!(header_height(200.0, 200.0), 18.0);
    }

    #[test]
    fn header_height_proportional_in_mid_range() {
        assert_eq!(header_height(200.0, 100.0), 15.0);
    }

    #[test]
    fn header_height_zero_when_below_min() {
        assert_eq!(header_height(200.0, 40.0), 0.0);
    }

    #[test]
    fn empty_tree_produces_empty_layout() {
        let tree = DirTree::new();
        let config = default_config();
        let rects = compute_layout(&tree, 0, 800.0, 600.0, &config);
        assert!(rects.is_empty());
    }

    #[test]
    fn single_root_no_children_produces_empty_layout() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 100, 100);
        let config = default_config();
        let rects = compute_layout(&tree, 1, 800.0, 600.0, &config);
        assert!(rects.is_empty());
    }

    #[test]
    fn two_equal_children_split_canvas() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_dir(2, 1, "a", 500, 100);
        tree.insert_dir(3, 1, "b", 500, 100);

        let config = default_config();
        let rects = compute_layout(&tree, 1, 800.0, 600.0, &config);

        let top_level: Vec<&LayoutRect> = rects.iter().filter(|r| r.depth == 0).collect();
        assert_eq!(top_level.len(), 2);

        let area_a = top_level[0].w * top_level[0].h;
        let area_b = top_level[1].w * top_level[1].h;
        let ratio = area_a / area_b;
        assert!(
            (0.9..=1.1).contains(&ratio),
            "Equal children should have similar areas, got ratio {ratio}"
        );
    }

    #[test]
    fn larger_child_gets_more_area() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_dir(2, 1, "big", 900, 100);
        tree.insert_dir(3, 1, "small", 100, 100);

        let config = default_config();
        let rects = compute_layout(&tree, 1, 800.0, 600.0, &config);

        let top_level: Vec<&LayoutRect> = rects.iter().filter(|r| r.depth == 0).collect();
        assert_eq!(top_level.len(), 2);

        let big = top_level.iter().find(|r| r.name == "big").unwrap();
        let small = top_level.iter().find(|r| r.name == "small").unwrap();

        let area_big = big.w * big.h;
        let area_small = small.w * small.h;
        assert!(
            area_big > area_small * 2.0,
            "big ({area_big}) should be significantly larger than small ({area_small})"
        );
    }

    fn rects_overlap(a: &LayoutRect, b: &LayoutRect) -> bool {
        let eps = 0.01;
        let x_overlap = a.x + eps < b.x + b.w && b.x + eps < a.x + a.w;
        let y_overlap = a.y + eps < b.y + b.h && b.y + eps < a.y + a.h;
        x_overlap && y_overlap
    }

    #[test]
    fn sibling_rects_do_not_overlap() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_dir(2, 1, "a", 400, 100);
        tree.insert_dir(3, 1, "b", 300, 100);
        tree.insert_dir(4, 1, "c", 200, 100);
        tree.insert_dir(5, 1, "d", 100, 100);

        let config = default_config();
        let rects = compute_layout(&tree, 1, 800.0, 600.0, &config);

        let siblings: Vec<&LayoutRect> = rects.iter().filter(|r| r.depth == 0).collect();
        for i in 0..siblings.len() {
            for j in (i + 1)..siblings.len() {
                assert!(
                    !rects_overlap(siblings[i], siblings[j]),
                    "Rects '{}' and '{}' overlap",
                    siblings[i].name,
                    siblings[j].name
                );
            }
        }
    }

    #[test]
    fn all_rects_fit_within_canvas() {
        let canvas_w = 1024.0;
        let canvas_h = 768.0;

        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_dir(2, 1, "a", 500, 100);
        tree.insert_dir(3, 1, "b", 300, 100);
        tree.insert_dir(4, 1, "c", 200, 100);

        let config = default_config();
        let rects = compute_layout(&tree, 1, canvas_w, canvas_h, &config);

        let eps = 0.01;
        for r in &rects {
            assert!(r.x >= -eps, "Rect '{}' has x={} < 0", r.name, r.x);
            assert!(r.y >= -eps, "Rect '{}' has y={} < 0", r.name, r.y);
            assert!(
                r.x + r.w <= canvas_w + eps,
                "Rect '{}' extends past canvas width: x+w={} > {canvas_w}",
                r.name,
                r.x + r.w
            );
            assert!(
                r.y + r.h <= canvas_h + eps,
                "Rect '{}' extends past canvas height: y+h={} > {canvas_h}",
                r.name,
                r.y + r.h
            );
        }
    }

    #[test]
    fn top_level_area_approximately_matches_canvas() {
        let canvas_w = 800.0;
        let canvas_h = 600.0;
        let canvas_area = canvas_w * canvas_h;

        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_dir(2, 1, "a", 600, 100);
        tree.insert_dir(3, 1, "b", 300, 100);
        tree.insert_dir(4, 1, "c", 100, 100);

        let config = default_config();
        let rects = compute_layout(&tree, 1, canvas_w, canvas_h, &config);

        let top_area: f64 = rects.iter().filter(|r| r.depth == 0).map(|r| r.w * r.h).sum();

        let ratio = top_area / canvas_area;
        assert!(
            (0.99..=1.01).contains(&ratio),
            "Top-level area ({top_area}) should match canvas area ({canvas_area}), ratio={ratio}"
        );
    }

    #[test]
    fn layout_includes_file_rects() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_file(1, "big.txt", 5000, 200);
        tree.insert_file(1, "huge.rs", 10000, 300);
        *tree.recursive_sizes.entry(1).or_insert(0) += 15000;

        let config = default_config();
        let rects = compute_layout(&tree, 1, 800.0, 600.0, &config);

        let file_rects: Vec<&LayoutRect> = rects.iter().filter(|r| r.is_file).collect();
        assert!(
            file_rects.len() >= 2,
            "Expected at least 2 file rects, got {}",
            file_rects.len()
        );

        let names: Vec<&str> = file_rects.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"big.txt"), "Missing big.txt in {names:?}");
        assert!(names.contains(&"huge.rs"), "Missing huge.rs in {names:?}");

        for r in &file_rects {
            assert!(r.id < 0, "File rect should have negative id, got {}", r.id);
        }
    }

    #[test]
    fn filter_affects_layout() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_dir(2, 1, "src", 0, 100);
        tree.insert_dir(3, 1, "docs", 0, 100);
        tree.insert_file(2, "main.rs", 5000, 200);
        tree.insert_file(2, "lib.rs", 3000, 200);
        tree.insert_file(3, "readme.txt", 4000, 200);
        tree.recursive_sizes.insert(2, 8000);
        tree.recursive_sizes.insert(3, 4000);
        tree.recursive_sizes.insert(1, 12000);

        let config_no_filter = default_config();
        let rects_no_filter = compute_layout(&tree, 1, 800.0, 600.0, &config_no_filter);
        let dirs_no_filter: Vec<&str> = rects_no_filter
            .iter()
            .filter(|r| r.depth == 0 && !r.is_file && !r.is_files)
            .map(|r| r.name.as_str())
            .collect();
        assert!(dirs_no_filter.contains(&"src"));
        assert!(dirs_no_filter.contains(&"docs"));

        let config_filtered = LayoutConfig {
            max_depth: 3,
            color_mode: 0,
            filter: FilterConfig {
                extensions: vec!["rs".into()],
                min_size: 0,
                max_size: 0,
                name_pattern: String::new(),
            },
            mtime_range: (0, 0),
        };
        let rects_filtered = compute_layout(&tree, 1, 800.0, 600.0, &config_filtered);

        let filtered_names: Vec<&str> = rects_filtered
            .iter()
            .filter(|r| r.depth == 0 && !r.is_file && !r.is_files)
            .map(|r| r.name.as_str())
            .collect();
        assert!(filtered_names.contains(&"src"), "src should be visible with .rs filter");
        assert!(
            !filtered_names.contains(&"docs"),
            "docs should be hidden with .rs filter (no .rs files)"
        );
    }

    #[test]
    fn max_depth_limits_nesting() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_dir(2, 1, "level1", 0, 100);
        tree.insert_dir(3, 2, "level2", 0, 100);
        tree.insert_dir(4, 3, "level3", 500, 100);

        let config_shallow = LayoutConfig {
            max_depth: 1,
            color_mode: 0,
            filter: FilterConfig::default(),
            mtime_range: (0, 0),
        };
        let rects_shallow = compute_layout(&tree, 1, 800.0, 600.0, &config_shallow);
        let max_depth = rects_shallow.iter().map(|r| r.depth).max().unwrap_or(0);
        assert!(
            max_depth <= 1,
            "With max_depth=1, deepest rect depth should be <= 1, got {max_depth}"
        );

        let config_deep = LayoutConfig {
            max_depth: 10,
            color_mode: 0,
            filter: FilterConfig::default(),
            mtime_range: (0, 0),
        };
        let rects_deep = compute_layout(&tree, 1, 800.0, 600.0, &config_deep);
        let max_depth_deep = rects_deep.iter().map(|r| r.depth).max().unwrap_or(0);
        assert!(
            max_depth_deep > 1,
            "With max_depth=10, should nest deeper than 1, got {max_depth_deep}"
        );
    }

    #[test]
    fn many_children_no_overlap_and_fit() {
        let canvas_w = 1920.0;
        let canvas_h = 1080.0;

        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        for i in 2..=21u64 {
            tree.insert_dir(i, 1, &format!("dir{i}"), i * 100, 100);
        }

        let config = default_config();
        let rects = compute_layout(&tree, 1, canvas_w, canvas_h, &config);

        let top_level: Vec<&LayoutRect> = rects.iter().filter(|r| r.depth == 0).collect();
        assert_eq!(top_level.len(), 20);

        for i in 0..top_level.len() {
            for j in (i + 1)..top_level.len() {
                assert!(
                    !rects_overlap(top_level[i], top_level[j]),
                    "Overlap between '{}' and '{}'",
                    top_level[i].name,
                    top_level[j].name
                );
            }
        }

        let eps = 0.01;
        for r in &top_level {
            assert!(r.x >= -eps && r.y >= -eps);
            assert!(r.x + r.w <= canvas_w + eps);
            assert!(r.y + r.h <= canvas_h + eps);
        }
    }
}
