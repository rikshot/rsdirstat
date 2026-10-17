use rsdirstat_protocol::{ColorMode, LayoutRect, RectKind};

use crate::color::{age_hue, hash_id_to_hue};
use crate::tree::{DirNode, DirTree, FileEntry, FilterConfig, NodeMap};

const MIN_NEST_PX: f64 = 40.0;

/// How a view is laid out.
pub struct LayoutConfig<'a> {
    pub max_depth: u8,
    pub color_mode: ColorMode,
    pub filter: &'a FilterConfig,
}

/// A view's treemap.
#[derive(Default)]
pub struct Layout {
    /// The size the rects divide up: the view root's, counting with a filter active only the files
    /// it matches, as the rects do.
    pub size: u64,
    pub rects: Vec<LayoutRect>,
}

/// Sizes under a filter, kept from one layout of a view to the next while the tree under the view
/// root, the view root and the filter stay the same. Filtering visits every file under the view,
/// too much to repeat for each layout of a big tree that hasn't changed (resizing a window lays it
/// out many times).
#[derive(Default)]
pub struct FilteredSizes {
    /// What `sizes` were computed for: the view root, as of when it last `changed`, and the filter.
    key: Option<(u64, u64, FilterConfig)>,
    sizes: NodeMap<u64>,
}

impl FilteredSizes {
    fn get(&mut self, tree: &DirTree, filter: &FilterConfig, view_root: u64, changed: u64) -> &NodeMap<u64> {
        let fresh = self
            .key
            .as_ref()
            .is_some_and(|(root, as_of, cached)| (*root, *as_of) == (view_root, changed) && cached == filter);
        if !fresh {
            self.sizes = tree.compute_filtered_sizes(filter, view_root);
            self.key = Some((view_root, changed, filter.clone()));
        }
        &self.sizes
    }
}

/// Lay out `view_root`'s subtree on a `canvas_w` × `canvas_h` canvas. With a filter active,
/// `filtered` holds on to the sizes it leaves for the view's next layout.
pub fn compute_layout(
    tree: &DirTree,
    view_root: u64,
    canvas_w: f64,
    canvas_h: f64,
    config: &LayoutConfig,
    filtered: &mut FilteredSizes,
) -> Layout {
    let Some(root) = tree.nodes.get(&view_root) else {
        return Layout::default();
    };
    let filtered_sizes = config
        .filter
        .is_active()
        .then(|| filtered.get(tree, config.filter, view_root, root.changed));
    let mut ctx = LayoutCtx {
        tree,
        filtered_sizes,
        config,
        out: Vec::new(),
    };
    let canvas = Rect {
        x: 0.0,
        y: 0.0,
        w: canvas_w,
        h: canvas_h,
    };
    let size = ctx.size(view_root, root);
    ctx.layout_node(view_root, root, size, canvas, 0.0, 0);
    Layout { size, rects: ctx.out }
}

#[derive(Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    /// The same rect with its axes swapped.
    fn transposed(self) -> Rect {
        Rect {
            x: self.y,
            y: self.x,
            w: self.h,
            h: self.w,
        }
    }
}

enum ItemKind<'a> {
    /// A folder, with its node id.
    Dir(u64, &'a DirNode),
    File(&'a FileEntry),
    /// Files too small to draw individually, summed into one "(other files)" rect.
    Aggregate,
}

struct LayoutItem<'a> {
    size: u64,
    kind: ItemKind<'a>,
}

/// Per-call layout state: the inputs every level needs plus the output.
struct LayoutCtx<'a> {
    tree: &'a DirTree,
    /// With a filter active, sizes counting only the files it matches; otherwise each directory's
    /// own `size` is used.
    filtered_sizes: Option<&'a NodeMap<u64>>,
    config: &'a LayoutConfig<'a>,
    out: Vec<LayoutRect>,
}

impl<'a> LayoutCtx<'a> {
    fn size(&self, id: u64, node: &DirNode) -> u64 {
        match self.filtered_sizes {
            Some(sizes) => sizes.get(&id).copied().unwrap_or(0),
            None => node.size,
        }
    }

    fn hue(&self, hue: u16, mtime: i64) -> u16 {
        if self.config.color_mode == ColorMode::Age {
            let (min_time, max_time) = self.tree.mtime_range;
            age_hue(mtime, min_time, max_time)
        } else {
            hue
        }
    }

    /// Lay out `node`'s sub-directories and files inside `rect`, below a header strip of height
    /// `header` (0 for the view root), as rects at nesting level `depth`.
    fn layout_node(&mut self, node_id: u64, node: &'a DirNode, total_size: u64, rect: Rect, header: f64, depth: u8) {
        let content = Rect {
            y: rect.y + header,
            h: (rect.h - header).max(0.0),
            ..rect
        };
        if content.w < 2.0 || content.h < 2.0 {
            return;
        }

        let mut items: Vec<LayoutItem<'a>> = Vec::new();
        for &id in &node.children {
            if let Some(child) = self.tree.nodes.get(&id)
                && let size = self.size(id, child)
                && size > 0
            {
                items.push(LayoutItem {
                    size,
                    kind: ItemKind::Dir(id, child),
                });
            }
        }

        // Files that would get less than ~4px² are folded into one aggregate rect.
        let area = content.w * content.h;
        let min_file_size = if area > 0.0 && total_size > 0 {
            (4.0 / area) * total_size as f64
        } else {
            f64::MAX
        };
        let mut residual: u64 = 0;
        for file in &node.files {
            if file.size == 0
                || (self.filtered_sizes.is_some() && !self.config.filter.matches_file(&file.name, file.size))
            {
                continue;
            }
            if (file.size as f64) >= min_file_size {
                items.push(LayoutItem {
                    size: file.size,
                    kind: ItemKind::File(file),
                });
            } else {
                residual += file.size;
            }
        }
        if residual > 0 {
            items.push(LayoutItem {
                size: residual,
                kind: ItemKind::Aggregate,
            });
        }

        if items.is_empty() {
            return;
        }
        items.sort_unstable_by_key(|item| std::cmp::Reverse(item.size));

        let mut rects = Vec::with_capacity(items.len());
        squarify(&items, content, &mut rects);

        // The zip relies on squarify emitting exactly one rect per item, in input order.
        debug_assert_eq!(rects.len(), items.len(), "squarify must emit one rect per item");
        for (r, item) in rects.into_iter().zip(&items) {
            // Skip any rect the client won't draw (it filters `w >= 1 && h >= 1`) before it is
            // serialized + deflated into the payload sent ~20×/sec during a scan. This pays off
            // because directories — unlike files, which aggregate below `min_file_size` — get a rect per
            // non-empty child with no size floor, so a high-fan-out dir (Windows WinSxS, the driver/
            // component store) otherwise emits thousands of sub-pixel rects. A sub-pixel rect never
            // satisfies `can_nest` (which needs >= MIN_NEST_PX), so culling here drops nothing nested.
            if r.w < 1.0 || r.h < 1.0 {
                continue;
            }
            // Files and "(other files)" keep id 0 (see `LayoutRect::id`).
            let mut rect = LayoutRect {
                parent_id: node_id,
                x: r.x as f32,
                y: r.y as f32,
                w: r.w as f32,
                h: r.h as f32,
                size: item.size,
                depth,
                ..LayoutRect::default()
            };
            let mut nest = None;
            match item.kind {
                ItemKind::Dir(id, child) => {
                    rect.id = id;
                    rect.name = child.name.to_string();
                    rect.hue = self.hue(child.hue, child.mtime);
                    rect.mtime = child.mtime;
                    // `max_depth` is the number of levels shown: this rect sits on level `depth + 1`,
                    // so opening it puts its contents on `depth + 2`. Any folder with room opens up,
                    // including one holding only files, which would otherwise stay a solid block at
                    // every depth.
                    if depth + 1 < self.config.max_depth && r.w >= MIN_NEST_PX && r.h >= MIN_NEST_PX {
                        rect.kind = RectKind::Container;
                        nest = Some((id, child));
                    }
                }
                ItemKind::File(file) => {
                    rect.name = file.name.to_string();
                    rect.hue = self.hue(file.hue, file.mtime);
                    rect.mtime = file.mtime;
                    rect.kind = RectKind::File;
                }
                ItemKind::Aggregate => {
                    rect.name = "(other files)".to_string();
                    rect.hue = hash_id_to_hue(node_id);
                    rect.kind = RectKind::OtherFiles;
                }
            }
            // From the rect's f32 size, exactly as the client computes it, so the contents start
            // right below the header it draws.
            let header = f64::from(rect.header_height());
            self.out.push(rect);
            if let Some((id, child)) = nest {
                self.layout_node(id, child, item.size, r, header, depth + 1);
            }
        }
    }
}

/// Squarified treemap: split `bounds` among `items` (sorted by descending size) and push exactly
/// one rect per item, in input order, until the space or the remaining size runs out.
fn squarify(items: &[LayoutItem], mut bounds: Rect, out: &mut Vec<Rect>) {
    let size = |item: &LayoutItem| item.size as f64;
    let mut area_left: f64 = items.iter().map(size).sum();
    let mut lo = 0;
    let hi = items.len();
    while lo < hi && bounds.w > 0.0 && bounds.h > 0.0 && area_left > 0.0 {
        if hi - lo == 1 {
            out.push(bounds);
            return;
        }

        // Each row runs along the short side: a column on the left of a wide space, or, the same
        // with the axes swapped, a row along the top of a tall one.
        let vertical = bounds.w >= bounds.h;
        let mut space = if vertical { bounds } else { bounds.transposed() };
        let short_side = space.h;
        let scale = (bounds.w * bounds.h) / area_left;

        let mut row_area = 0.0_f64;
        let mut best_worst = f64::INFINITY;
        let mut split = lo;

        for i in lo..hi {
            let test_area = row_area + size(&items[i]);
            let test_len = (test_area * scale) / short_side;
            if test_len <= 0.0 {
                row_area = test_area;
                split = i + 1;
                continue;
            }

            let aspect = |item_size: f64| {
                let extent = (item_size * scale) / test_len;
                if extent > 0.0 {
                    (test_len / extent).max(extent / test_len)
                } else {
                    0.0
                }
            };
            let worst = aspect(size(&items[lo])).max(aspect(size(&items[i])));

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

        let row = &items[lo..split];
        let row_w = space.w * row_frac;
        let mut cy = space.y;
        for (i, item) in row.iter().enumerate() {
            // Last item fills the remainder; clamp so float drift can't yield a negative extent.
            let ih = if i == row.len() - 1 {
                (space.y + space.h - cy).max(0.0)
            } else {
                (size(item) / row_area) * space.h
            };
            let rect = Rect {
                x: space.x,
                y: cy,
                w: row_w,
                h: ih,
            };
            out.push(if vertical { rect } else { rect.transposed() });
            cy += ih;
        }
        space.x += row_w;
        space.w -= row_w;
        bounds = if vertical { space } else { space.transposed() };
        area_left -= row_area;
        lo = split;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_FILTER: &FilterConfig = &FilterConfig {
        extensions: Vec::new(),
        min_size: 0,
        name_pattern: String::new(),
    };

    fn default_config() -> LayoutConfig<'static> {
        LayoutConfig {
            max_depth: 3,
            color_mode: ColorMode::Type,
            filter: NO_FILTER,
        }
    }

    /// The rects of a layout made afresh.
    fn layout(tree: &DirTree, view_root: u64, canvas_w: f64, canvas_h: f64, config: &LayoutConfig) -> Vec<LayoutRect> {
        compute_layout(
            tree,
            view_root,
            canvas_w,
            canvas_h,
            config,
            &mut FilteredSizes::default(),
        )
        .rects
    }

    /// In Age mode a folder drawn as one block takes the colour of its newest content, not of its
    /// own files: one holding only subfolders has none.
    #[test]
    fn age_colours_a_folder_block_by_its_newest_content() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 0);
        tree.insert_dir(2, 1, "old", 0, 0);
        tree.insert_dir(3, 2, "inside-old", 500, 1_000);
        tree.insert_dir(4, 1, "new", 0, 0);
        tree.insert_dir(5, 4, "inside-new", 500, 2_000);
        let config = LayoutConfig {
            max_depth: 1,
            color_mode: ColorMode::Age,
            ..default_config()
        };
        let rects = layout(&tree, 1, 800.0, 600.0, &config);
        let rect = |name: &str| rects.iter().find(|rect| rect.name == name).unwrap();
        assert_eq!((rect("old").hue, rect("old").mtime), (0, 1_000), "oldest: red");
        assert_eq!((rect("new").hue, rect("new").mtime), (120, 2_000), "newest: green");
    }

    #[test]
    fn empty_tree_produces_empty_layout() {
        let tree = DirTree::new();
        let config = default_config();
        let rects = layout(&tree, 0, 800.0, 600.0, &config);
        assert!(rects.is_empty());
    }

    #[test]
    fn single_root_no_children_produces_empty_layout() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 100, 100);
        let config = default_config();
        let rects = layout(&tree, 1, 800.0, 600.0, &config);
        assert!(rects.is_empty());
    }

    #[test]
    fn two_equal_children_split_canvas() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_dir(2, 1, "a", 500, 100);
        tree.insert_dir(3, 1, "b", 500, 100);

        let config = default_config();
        let rects = layout(&tree, 1, 800.0, 600.0, &config);

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
        let rects = layout(&tree, 1, 800.0, 600.0, &config);

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
        let rects = layout(&tree, 1, 800.0, 600.0, &config);

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
        let rects = layout(&tree, 1, canvas_w, canvas_h, &config);

        let eps = 0.01_f64;
        for r in &rects {
            assert!(f64::from(r.x) >= -eps, "Rect '{}' has x={} < 0", r.name, r.x);
            assert!(f64::from(r.y) >= -eps, "Rect '{}' has y={} < 0", r.name, r.y);
            assert!(
                f64::from(r.x + r.w) <= canvas_w + eps,
                "Rect '{}' extends past canvas width: x+w={} > {canvas_w}",
                r.name,
                r.x + r.w
            );
            assert!(
                f64::from(r.y + r.h) <= canvas_h + eps,
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
        let rects = layout(&tree, 1, canvas_w, canvas_h, &config);

        let top_area: f64 = rects
            .iter()
            .filter(|r| r.depth == 0)
            .map(|r| f64::from(r.w) * f64::from(r.h))
            .sum();

        let ratio = top_area / canvas_area;
        assert!(
            (0.99..=1.01).contains(&ratio),
            "Top-level area ({top_area}) should match canvas area ({canvas_area}), ratio={ratio}"
        );
    }

    #[test]
    fn layout_includes_file_rects() {
        let mut tree = DirTree::new();
        tree.add_dir(1, 0, "root", &[("big.txt", 5000, 200), ("huge.rs", 10000, 300)]);

        let config = default_config();
        let rects = layout(&tree, 1, 800.0, 600.0, &config);

        let file_rects: Vec<&LayoutRect> = rects.iter().filter(|r| r.kind == RectKind::File).collect();
        assert!(
            file_rects.len() >= 2,
            "Expected at least 2 file rects, got {}",
            file_rects.len()
        );

        let names: Vec<&str> = file_rects.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"big.txt"), "Missing big.txt in {names:?}");
        assert!(names.contains(&"huge.rs"), "Missing huge.rs in {names:?}");
    }

    #[test]
    fn filter_affects_layout() {
        let mut tree = DirTree::new();
        tree.add_dir(1, 0, "root", &[]);
        tree.add_dir(2, 1, "src", &[("main.rs", 5000, 200), ("lib.rs", 3000, 200)]);
        tree.add_dir(3, 1, "docs", &[("readme.txt", 4000, 200)]);

        let config_no_filter = default_config();
        let rects_no_filter = layout(&tree, 1, 800.0, 600.0, &config_no_filter);
        let dirs_no_filter: Vec<&str> = rects_no_filter
            .iter()
            .filter(|r| r.depth == 0 && r.kind.is_dir())
            .map(|r| r.name.as_str())
            .collect();
        assert!(dirs_no_filter.contains(&"src"));
        assert!(dirs_no_filter.contains(&"docs"));

        let rs_files = FilterConfig {
            extensions: vec!["rs".into()],
            ..FilterConfig::default()
        };
        let config_filtered = LayoutConfig {
            filter: &rs_files,
            ..default_config()
        };
        let rects_filtered = layout(&tree, 1, 800.0, 600.0, &config_filtered);

        let filtered_names: Vec<&str> = rects_filtered
            .iter()
            .filter(|r| r.depth == 0 && r.kind.is_dir())
            .map(|r| r.name.as_str())
            .collect();
        assert!(filtered_names.contains(&"src"), "src should be visible with .rs filter");
        assert!(
            !filtered_names.contains(&"docs"),
            "docs should be hidden with .rs filter (no .rs files)"
        );

        // The total is what the rects divide up: with a filter, only the files it matches. The
        // page measures each rect's share of the view against it.
        assert_eq!(
            compute_layout(&tree, 1, 800.0, 600.0, &config_no_filter, &mut FilteredSizes::default()).size,
            12_000
        );
        assert_eq!(
            compute_layout(&tree, 1, 800.0, 600.0, &config_filtered, &mut FilteredSizes::default()).size,
            8_000
        );
    }

    /// `max_depth` is the number of levels drawn: 1 is a flat map of the view root's children, and
    /// each step adds one level of nesting (issue #5).
    #[test]
    fn max_depth_is_levels_shown() {
        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        tree.insert_dir(2, 1, "level1", 0, 100);
        tree.insert_dir(3, 2, "level2", 0, 100);
        tree.insert_dir(4, 3, "level3", 0, 100);
        tree.insert_dir(5, 4, "level4", 500, 100);

        let levels_drawn = |max_depth: u8| {
            let config = LayoutConfig {
                max_depth,
                ..default_config()
            };
            let rects = layout(&tree, 1, 800.0, 600.0, &config);
            rects.iter().map(|r| r.depth + 1).max().unwrap_or(0)
        };
        for max_depth in 1..=4 {
            assert_eq!(levels_drawn(max_depth), max_depth, "max_depth={max_depth}");
        }
        assert_eq!(levels_drawn(10), 4, "the tree is only 4 levels deep");

        let config = LayoutConfig {
            max_depth: 1,
            ..default_config()
        };
        let rects = layout(&tree, 1, 800.0, 600.0, &config);
        assert!(
            rects.iter().all(|r| r.kind != RectKind::Container),
            "max_depth=1 should not open any folder"
        );
    }

    /// A folder holding only files opens up like any other, instead of staying a solid block at
    /// every depth (issue #5).
    #[test]
    fn files_only_folder_shows_its_files() {
        let mut tree = DirTree::new();
        tree.add_dir(1, 0, "root", &[]);
        tree.add_dir(2, 1, "scans", &[("a.pdf", 2000, 100), ("b.pdf", 1000, 100)]);

        let rects = layout(&tree, 1, 800.0, 600.0, &default_config());
        let folder = rects.iter().find(|r| r.name == "scans").unwrap();
        assert_eq!(folder.kind, RectKind::Container, "a files-only folder should open up");
        let mut files: Vec<&str> = rects
            .iter()
            .filter(|r| r.kind == RectKind::File && r.parent_id == 2)
            .map(|r| r.name.as_str())
            .collect();
        files.sort_unstable();
        assert_eq!(files, ["a.pdf", "b.pdf"]);
    }

    #[test]
    fn many_children_no_overlap_and_fit() {
        let canvas_w = 1920.0;
        let canvas_h = 1080.0;

        let mut tree = DirTree::new();
        tree.insert_dir(1, 0, "root", 0, 100);
        for i in 2..=21u64 {
            tree.insert_dir(i, 1, format!("dir{i}"), i * 100, 100);
        }

        let config = default_config();
        let rects = layout(&tree, 1, canvas_w, canvas_h, &config);

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

        let eps = 0.01_f64;
        for r in &top_level {
            assert!(f64::from(r.x) >= -eps && f64::from(r.y) >= -eps);
            assert!(f64::from(r.x + r.w) <= canvas_w + eps);
            assert!(f64::from(r.y + r.h) <= canvas_h + eps);
        }
    }
}
