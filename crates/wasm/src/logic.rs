use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use rsdirstat_protocol::{BreadcrumbEntry, ClientMessage, LayoutPayload, LayoutRect, ServerMessage};

pub(crate) fn format_size(bytes: u64) -> String {
    let mut value = bytes as f64;
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut index = 0usize;
    while value >= 1024.0 && index < units.len() - 1 {
        value /= 1024.0;
        index += 1;
    }
    if index == 0 {
        format!("{value:.0} {}", units[index])
    } else if value < 10.0 {
        format!("{value:.2} {}", units[index])
    } else if value < 100.0 {
        format!("{value:.1} {}", units[index])
    } else {
        format!("{value:.0} {}", units[index])
    }
}

/// CSS `hsl()` colour, formatted once per distinct value and then shared: there are only a few
/// hundred of them, so they live for the page's lifetime and rects can hold `&'static str`s.
pub(crate) fn hsl(hue: u16, saturation: u8, lightness: u8) -> &'static str {
    thread_local! {
        static CACHE: RefCell<HashMap<(u16, u8, u8), &'static str>> = RefCell::new(HashMap::new());
    }
    CACHE.with(|cache| {
        *cache
            .borrow_mut()
            .entry((hue, saturation, lightness))
            .or_insert_with(|| Box::leak(format!("hsl({hue},{saturation}%,{lightness}%)").into_boxed_str()))
    })
}

pub(crate) fn collapse_slashes(path: String) -> String {
    let mut out = String::with_capacity(path.len());
    let mut last_was_slash = false;
    for ch in path.chars() {
        if ch == '/' {
            if !last_was_slash {
                out.push(ch);
            }
            last_was_slash = true;
        } else {
            out.push(ch);
            last_was_slash = false;
        }
    }
    out
}

/// A breadcrumb entry's display name; the scan root of `/` arrives with an empty name.
pub(crate) fn crumb_label(entry: &BreadcrumbEntry) -> &str {
    if entry.name.is_empty() { "/" } else { &entry.name }
}

/// A laid-out rect ready to draw: geometry (in f64 layout space) plus the colours derived from its
/// hue. This is the single rect representation used throughout the client.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RenderRect {
    pub id: i64,
    pub parent_id: u64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub name: String,
    pub size: u64,
    pub is_container: bool,
    pub header_height: f64,
    pub is_file: bool,
    pub mtime: i64,
    pub depth: u8,
    /// Body colour: a dark shade for a container (its children draw on top), a vivid one otherwise.
    pub fill: &'static str,
    /// Header-strip colour; only drawn for containers.
    pub header_fill: &'static str,
}

impl RenderRect {
    pub(crate) fn from_wire(rect: LayoutRect) -> Self {
        let (fill, header_fill) = if rect.is_container {
            (hsl(rect.hue, 25, 13), hsl(rect.hue, 35, 20))
        } else {
            let vivid = hsl(rect.hue, 62, 38);
            (vivid, vivid)
        };
        Self {
            id: rect.id,
            parent_id: rect.parent_id,
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
            name: rect.name,
            size: rect.size,
            is_container: rect.is_container,
            header_height: rect.header_height,
            is_file: rect.is_file,
            mtime: rect.mtime,
            depth: rect.depth,
            fill,
            header_fill,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HoverState {
    pub hovered_index: usize,
    pub hovered_ancestor_indices: Vec<usize>,
    pub path_text: String,
}

#[derive(Clone, Debug, PartialEq)]
enum InterpolationStep {
    Matched { from: RenderRect, to: RenderRect },
    Entering { to: RenderRect },
    Exiting { from: RenderRect },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InterpolationPlan {
    steps: Vec<InterpolationStep>,
}

pub(crate) fn find_rect_index(rects: &[RenderRect], mouse_x: f64, mouse_y: f64) -> Option<usize> {
    rects
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, rect)| hit_test(rect, mouse_x, mouse_y).then_some(index))
}

pub(crate) fn find_navigable_target_index(rects: &[RenderRect], mouse_x: f64, mouse_y: f64) -> Option<usize> {
    // Topmost (last-drawn) directory under the cursor — same back-to-front scan as find_rect_index.
    // Files and "(other files)" aggregates carry negative ids, so `id > 0` means a real directory.
    rects
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, rect)| (hit_test(rect, mouse_x, mouse_y) && rect.id > 0).then_some(index))
}

pub(crate) fn build_rect_index(rects: &[RenderRect]) -> HashMap<u64, usize> {
    let mut id_to_index = HashMap::with_capacity(rects.len());
    for (index, rect) in rects.iter().enumerate() {
        if rect.id > 0 {
            id_to_index.insert(rect.id as u64, index);
        }
    }
    id_to_index
}

pub(crate) fn build_hover_state(
    rects: &[RenderRect],
    id_to_index: &HashMap<u64, usize>,
    breadcrumb: &[BreadcrumbEntry],
    mouse_x: f64,
    mouse_y: f64,
) -> Option<HoverState> {
    let hovered_index = find_rect_index(rects, mouse_x, mouse_y)?;
    let hovered_rect = &rects[hovered_index];
    let mut current_parent = hovered_rect.parent_id;
    let mut hovered_ancestor_indices = Vec::new();
    while let Some(index) = id_to_index.get(&current_parent).copied() {
        let rect = &rects[index];
        if rect.is_container {
            hovered_ancestor_indices.push(index);
        }
        current_parent = rect.parent_id;
    }

    let mut parts: Vec<&str> = Vec::with_capacity(breadcrumb.len() + hovered_ancestor_indices.len() + 1);
    parts.extend(breadcrumb.iter().map(crumb_label));
    parts.extend(
        hovered_ancestor_indices
            .iter()
            .rev()
            .map(|&index| rects[index].name.as_str()),
    );
    parts.push(&hovered_rect.name);
    let path_text = collapse_slashes(parts.join("/"));

    Some(HoverState {
        hovered_index,
        hovered_ancestor_indices,
        path_text,
    })
}

impl InterpolationPlan {
    pub(crate) fn new(from: &[RenderRect], to: &[RenderRect]) -> Self {
        let mut from_by_id = HashMap::with_capacity(from.len());
        for rect in from {
            from_by_id.insert(rect.id, rect);
        }
        let mut seen = HashSet::with_capacity(to.len());
        let mut steps = Vec::with_capacity(to.len() + from.len());

        for to_rect in to {
            seen.insert(to_rect.id);
            if let Some(from_rect) = from_by_id.get(&to_rect.id) {
                steps.push(InterpolationStep::Matched {
                    from: (*from_rect).clone(),
                    to: to_rect.clone(),
                });
            } else {
                steps.push(InterpolationStep::Entering { to: to_rect.clone() });
            }
        }

        for from_rect in from {
            if seen.contains(&from_rect.id) {
                continue;
            }
            steps.push(InterpolationStep::Exiting {
                from: from_rect.clone(),
            });
        }

        Self { steps }
    }

    pub(crate) fn interpolate(&self, progress: f64) -> Vec<RenderRect> {
        let clamped_progress = progress.clamp(0.0, 1.0);
        let inverse = 1.0 - clamped_progress;
        let mut result = Vec::with_capacity(self.steps.len());

        for step in &self.steps {
            // Each arm keeps the colours/name/flags of the rect it is animating toward (or, for
            // exits, the one it is leaving) and only the geometry is interpolated.
            let rect = match step {
                InterpolationStep::Matched { from, to } => {
                    let mut rect = to.clone();
                    rect.x = from.x + (to.x - from.x) * clamped_progress;
                    rect.y = from.y + (to.y - from.y) * clamped_progress;
                    rect.w = from.w + (to.w - from.w) * clamped_progress;
                    rect.h = from.h + (to.h - from.h) * clamped_progress;
                    rect
                }
                InterpolationStep::Entering { to } => {
                    let mut rect = to.clone();
                    rect.x = to.x + to.w * 0.5 * inverse;
                    rect.y = to.y + to.h * 0.5 * inverse;
                    rect.w = to.w * clamped_progress;
                    rect.h = to.h * clamped_progress;
                    rect
                }
                InterpolationStep::Exiting { from } => {
                    let mut rect = from.clone();
                    rect.x = from.x + from.w * 0.5 * clamped_progress;
                    rect.y = from.y + from.h * 0.5 * clamped_progress;
                    rect.w = from.w * inverse;
                    rect.h = from.h * inverse;
                    rect
                }
            };
            result.push(rect);
        }

        result
    }
}

fn hit_test(rect: &RenderRect, mouse_x: f64, mouse_y: f64) -> bool {
    mouse_x >= rect.x && mouse_x < rect.x + rect.w && mouse_y >= rect.y && mouse_y < rect.y + rect.h
}

/// Sanity checks run both natively (`cargo test`) and inside the browser (the `runBrowserTests`
/// export, driven by the e2e suite), where they also prove the wire codec works under wasm32.
pub(crate) fn self_checks() -> Vec<(&'static str, bool)> {
    let breadcrumb = vec![BreadcrumbEntry {
        id: 7,
        name: "root".into(),
    }];
    let rects = vec![LayoutRect {
        id: 42,
        parent_id: 7,
        x: 10.0,
        y: 20.0,
        w: 100.0,
        h: 60.0,
        name: "src".into(),
        hue: 120,
        size: 5_000,
        depth: 2,
        is_container: true,
        header_height: 18.0,
        mtime: 1_700_000_000,
        ..LayoutRect::default()
    }];
    let layout = ServerMessage::Layout(LayoutPayload {
        root_size: 10_000,
        dir_count: 3,
        scan_done: true,
        breadcrumb: breadcrumb.clone(),
        rects: rects.clone(),
    });
    let hit_rect = RenderRect::from_wire(rects[0].clone());

    vec![
        ("formatSize zero", format_size(0) == "0 B"),
        ("formatSize kib", format_size(1024) == "1.00 KiB"),
        ("hsl", hsl(120, 50, 40) == "hsl(120,50%,40%)"),
        ("hitTest", hit_test(&hit_rect, 35.0, 35.0)),
        (
            "viewport roundtrip",
            matches!(
                ClientMessage::decode(&ClientMessage::Viewport { width: 800.0, height: 600.0 }.encode()),
                Some(ClientMessage::Viewport { width, height }) if width == 800.0 && height == 600.0
            ),
        ),
        (
            "navigate roundtrip",
            matches!(
                ClientMessage::decode(&ClientMessage::Navigate { id: 42 }.encode()),
                Some(ClientMessage::Navigate { id }) if id == 42
            ),
        ),
        (
            "layout roundtrip",
            matches!(
                ServerMessage::decode(&layout.encode()),
                Some(ServerMessage::Layout(decoded))
                    if decoded.root_size == 10_000
                        && decoded.dir_count == 3
                        && decoded.scan_done
                        && decoded.breadcrumb == breadcrumb
                        && decoded.rects == rects
            ),
        ),
        ("collapse slashes", collapse_slashes("///a//b".into()) == "/a/b"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(id: i64, parent_id: u64, x: f64, y: f64, w: f64, h: f64, name: &str) -> RenderRect {
        RenderRect {
            id,
            parent_id,
            x,
            y,
            w,
            h,
            name: name.into(),
            size: 100,
            is_container: true,
            header_height: 18.0,
            is_file: false,
            mtime: 0,
            depth: 1,
            fill: "",
            header_fill: "",
        }
    }

    fn wire_rect(is_container: bool) -> LayoutRect {
        LayoutRect {
            id: 1,
            w: 10.0,
            h: 10.0,
            name: "x".into(),
            hue: 120,
            size: 1,
            is_container,
            is_file: !is_container,
            ..LayoutRect::default()
        }
    }

    #[test]
    fn self_checks_pass() {
        let failures: Vec<_> = self_checks().into_iter().filter(|(_, ok)| !ok).collect();
        assert!(failures.is_empty(), "failed: {failures:?}");
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024 * 1024), "1.00 MiB");
        assert_eq!(format_size(15 * 1024), "15.0 KiB");
        assert_eq!(format_size(500 * 1024), "500 KiB");
    }

    #[test]
    fn hit_test_is_half_open() {
        let r = rect(1, 0, 10.0, 20.0, 50.0, 30.0, "r");
        assert!(hit_test(&r, 10.0, 20.0));
        assert!(!hit_test(&r, 60.0, 50.0));
    }

    #[test]
    fn collapse_slashes_keeps_root() {
        assert_eq!(collapse_slashes("/".into()), "/");
    }

    #[test]
    fn from_wire_shades_containers_darker() {
        let container = RenderRect::from_wire(wire_rect(true));
        assert_eq!(container.fill, "hsl(120,25%,13%)");
        assert_eq!(container.header_fill, "hsl(120,35%,20%)");

        let file = RenderRect::from_wire(wire_rect(false));
        assert_eq!(file.fill, "hsl(120,62%,38%)");
    }

    #[test]
    fn find_rect_index_prefers_last_matching_rect() {
        let rects = vec![
            rect(1, 0, 0.0, 0.0, 10.0, 10.0, "a"),
            rect(2, 0, 0.0, 0.0, 10.0, 10.0, "b"),
        ];
        assert_eq!(find_rect_index(&rects, 5.0, 5.0), Some(1));
    }

    #[test]
    fn find_navigable_target_skips_files_and_aggregates() {
        let mut files = rect(-1, 0, 0.0, 0.0, 10.0, 10.0, "(other files)");
        files.is_container = false;
        let mut file = rect(-2, 0, 0.0, 0.0, 10.0, 10.0, "file");
        file.is_container = false;
        file.is_file = true;
        let dir = rect(3, 0, 0.0, 0.0, 10.0, 10.0, "dir");
        let rects = vec![dir, files, file];
        assert_eq!(find_navigable_target_index(&rects, 5.0, 5.0), Some(0));
    }

    #[test]
    fn build_hover_state_includes_breadcrumb_and_ancestors() {
        let root = rect(1, 0, 0.0, 0.0, 100.0, 100.0, "src");
        let child = rect(2, 1, 10.0, 10.0, 40.0, 40.0, "nested");
        let mut leaf = rect(-1, 2, 15.0, 15.0, 10.0, 10.0, "main.rs");
        leaf.is_container = false;
        leaf.is_file = true;
        leaf.header_height = 0.0;
        let breadcrumb = vec![BreadcrumbEntry {
            id: 0,
            name: "/tmp".into(),
        }];
        let rects = [root, child, leaf];
        let hover = build_hover_state(&rects, &build_rect_index(&rects), &breadcrumb, 16.0, 16.0).unwrap();
        assert_eq!(hover.hovered_index, 2);
        assert_eq!(hover.hovered_ancestor_indices, vec![1, 0]);
        assert_eq!(hover.path_text, "/tmp/src/nested/main.rs");
    }

    #[test]
    fn interpolate_rects_handles_matches_and_exits() {
        let from = vec![
            rect(1, 0, 0.0, 0.0, 100.0, 50.0, "a"),
            rect(2, 0, 100.0, 0.0, 50.0, 50.0, "b"),
        ];
        let to = vec![
            rect(1, 0, 50.0, 50.0, 50.0, 25.0, "a"),
            rect(3, 0, 0.0, 0.0, 20.0, 20.0, "c"),
        ];
        let interpolated = InterpolationPlan::new(&from, &to).interpolate(0.5);

        assert_eq!(interpolated.len(), 3);
        assert_eq!(interpolated[0].id, 1);
        assert_eq!(interpolated[0].x, 25.0);
        assert_eq!(interpolated[0].w, 75.0);

        assert_eq!(interpolated[1].id, 3);
        assert_eq!(interpolated[1].w, 10.0);

        assert_eq!(interpolated[2].id, 2);
        assert_eq!(interpolated[2].w, 25.0);
    }
}
