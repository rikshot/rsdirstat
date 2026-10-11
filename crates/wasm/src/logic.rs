use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use rsdirstat_protocol::{BreadcrumbEntry, ClientMessage, LayoutPayload, LayoutRect, RectKind, ServerMessage};

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

/// The status line for a finished scan: how many directories it found and how long it took, the
/// size of the view, and whether the scan stopped before reading everything.
pub(crate) fn scan_summary(dir_count: u32, seconds: f64, root_size: u64, partial: bool) -> String {
    let mut summary = format!("{dir_count} dirs in {seconds:.1}s — {}", format_size(root_size));
    if partial {
        summary.push_str(" — incomplete: the scan got stuck (see the terminal)");
    }
    summary
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

/// Join path components for display. The separator is `\` when the scan root is a Windows path
/// (its label holds one, as `C:\` does) and `/` otherwise, and none is added after a component that
/// already ends with one, such as a root of `/` or `C:\`.
pub(crate) fn join_path(parts: &[&str]) -> String {
    let separator = if parts.first().is_some_and(|root| root.contains('\\')) {
        '\\'
    } else {
        '/'
    };
    let mut path = String::new();
    for part in parts {
        if !path.is_empty() && !path.ends_with(['/', '\\']) {
            path.push(separator);
        }
        path.push_str(part);
    }
    path
}

/// A breadcrumb entry's display name; the scan root of `/` arrives with an empty name.
pub(crate) fn crumb_label(entry: &BreadcrumbEntry) -> &str {
    if entry.name.is_empty() { "/" } else { &entry.name }
}

/// A laid-out rect ready to draw: geometry (in f64 layout space) plus the colours derived from its
/// hue. This is the single rect representation used throughout the client.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RenderRect {
    pub id: u64,
    pub parent_id: u64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub name: String,
    pub size: u64,
    pub kind: RectKind,
    /// A container's header strip (see `LayoutRect::header_height`); 0 for every other rect.
    pub header_height: f64,
    pub mtime: i64,
    pub depth: u8,
    /// Body colour: a dark shade for a container (its children draw on top), a vivid one otherwise.
    pub fill: &'static str,
    /// Header-strip colour; only drawn for containers.
    pub header_fill: &'static str,
}

impl RenderRect {
    pub(crate) fn from_wire(rect: LayoutRect) -> Self {
        let (fill, header_fill) = if rect.kind.is_container() {
            (hsl(rect.hue, 25, 13), hsl(rect.hue, 35, 20))
        } else {
            let vivid = hsl(rect.hue, 62, 38);
            (vivid, vivid)
        };
        Self {
            id: rect.id,
            parent_id: rect.parent_id,
            x: rect.x.into(),
            y: rect.y.into(),
            w: rect.w.into(),
            h: rect.h.into(),
            header_height: rect.header_height().into(),
            name: rect.name,
            size: rect.size,
            kind: rect.kind,
            mtime: rect.mtime,
            depth: rect.depth,
            fill,
            header_fill,
        }
    }

    /// What identifies this rect from one layout to the next: a folder's id, or a file's (or
    /// "(other files)" block's) folder and name. No file is named "", so the two can't collide.
    fn key(&self) -> (u64, &str) {
        if self.kind.is_dir() {
            (self.id, "")
        } else {
            (self.parent_id, &self.name)
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
    // Topmost (last-drawn) folder under the cursor — same back-to-front scan as find_rect_index.
    rects
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, rect)| (hit_test(rect, mouse_x, mouse_y) && rect.kind.is_dir()).then_some(index))
}

pub(crate) fn build_rect_index(rects: &[RenderRect]) -> HashMap<u64, usize> {
    let mut id_to_index = HashMap::with_capacity(rects.len());
    for (index, rect) in rects.iter().enumerate() {
        if rect.kind.is_dir() {
            id_to_index.insert(rect.id, index);
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
        if rect.kind.is_container() {
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
    let path_text = join_path(&parts);

    Some(HoverState {
        hovered_index,
        hovered_ancestor_indices,
        path_text,
    })
}

impl InterpolationPlan {
    pub(crate) fn new(from: &[RenderRect], to: &[RenderRect]) -> Self {
        let mut from_by_key = HashMap::with_capacity(from.len());
        for rect in from {
            from_by_key.insert(rect.key(), rect);
        }
        let mut seen = HashSet::with_capacity(to.len());
        let mut steps = Vec::with_capacity(to.len() + from.len());

        for to_rect in to {
            seen.insert(to_rect.key());
            if let Some(from_rect) = from_by_key.get(&to_rect.key()) {
                steps.push(InterpolationStep::Matched {
                    from: (*from_rect).clone(),
                    to: to_rect.clone(),
                });
            } else {
                steps.push(InterpolationStep::Entering { to: to_rect.clone() });
            }
        }

        for from_rect in from {
            if seen.contains(&from_rect.key()) {
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
        kind: RectKind::Container,
        mtime: 1_700_000_000,
    }];
    let layout = ServerMessage::Layout(LayoutPayload {
        root_size: 10_000,
        dir_count: 3,
        scan_done: true,
        partial: false,
        scan_ms: 1_500,
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
        ("join a path under /", join_path(&["/", "usr", "lib"]) == "/usr/lib"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(id: u64, parent_id: u64, x: f64, y: f64, w: f64, h: f64, name: &str) -> RenderRect {
        RenderRect {
            id,
            parent_id,
            x,
            y,
            w,
            h,
            name: name.into(),
            size: 100,
            kind: RectKind::Container,
            header_height: 18.0,
            mtime: 0,
            depth: 1,
            fill: "",
            header_fill: "",
        }
    }

    fn wire_rect(kind: RectKind) -> LayoutRect {
        LayoutRect {
            id: 1,
            w: 100.0,
            h: 100.0,
            name: "x".into(),
            hue: 120,
            size: 1,
            kind,
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
    fn scan_summary_says_when_a_scan_is_incomplete() {
        assert_eq!(scan_summary(12, 3.25, 2048, false), "12 dirs in 3.2s — 2.00 KiB");
        assert_eq!(
            scan_summary(12, 3.25, 2048, true),
            "12 dirs in 3.2s — 2.00 KiB — incomplete: the scan got stuck (see the terminal)"
        );
    }

    #[test]
    fn hit_test_is_half_open() {
        let r = rect(1, 0, 10.0, 20.0, 50.0, 30.0, "r");
        assert!(hit_test(&r, 10.0, 20.0));
        assert!(!hit_test(&r, 60.0, 50.0));
    }

    #[test]
    fn join_path_uses_one_separator_of_the_roots_kind() {
        assert_eq!(join_path(&["/"]), "/");
        assert_eq!(join_path(&["/", "usr", "lib"]), "/usr/lib");
        assert_eq!(join_path(&["projects", "src", "main.rs"]), "projects/src/main.rs");
        // Regression: a Windows drive root showed as `C:\/Windows/System32`.
        assert_eq!(join_path(&["C:\\", "Windows", "System32"]), "C:\\Windows\\System32");
    }

    #[test]
    fn from_wire_shades_containers_darker() {
        let container = RenderRect::from_wire(wire_rect(RectKind::Container));
        assert_eq!(container.fill, "hsl(120,25%,13%)");
        assert_eq!(container.header_fill, "hsl(120,35%,20%)");
        assert_eq!(container.header_height, 15.0, "computed from the size, not sent");

        let file = RenderRect::from_wire(wire_rect(RectKind::File));
        assert_eq!(file.fill, "hsl(120,62%,38%)");
        assert_eq!(file.header_height, 0.0);
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
        let mut files = rect(10, 0, 0.0, 0.0, 10.0, 10.0, "(other files)");
        files.kind = RectKind::OtherFiles;
        let mut file = rect(11, 0, 0.0, 0.0, 10.0, 10.0, "file");
        file.kind = RectKind::File;
        let dir = rect(3, 0, 0.0, 0.0, 10.0, 10.0, "dir");
        let rects = vec![dir, files, file];
        assert_eq!(find_navigable_target_index(&rects, 5.0, 5.0), Some(0));
    }

    #[test]
    fn build_hover_state_includes_breadcrumb_and_ancestors() {
        let root = rect(1, 0, 0.0, 0.0, 100.0, 100.0, "src");
        let child = rect(2, 1, 10.0, 10.0, 40.0, 40.0, "nested");
        let mut leaf = rect(10, 2, 15.0, 15.0, 10.0, 10.0, "main.rs");
        leaf.kind = RectKind::File;
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

    /// Files arrive without ids and are matched by folder and name: the same file moves between
    /// layouts, and a same-named file in another folder is a different one.
    #[test]
    fn interpolation_matches_files_by_folder_and_name() {
        let file = |parent, name: &str, x| {
            let mut rect = rect(0, parent, x, 0.0, 10.0, 10.0, name);
            rect.kind = RectKind::File;
            rect
        };
        let from = vec![file(7, "a.rs", 0.0), file(7, "b.rs", 10.0)];
        let to = vec![file(7, "b.rs", 50.0), file(8, "a.rs", 90.0)];
        let mid = InterpolationPlan::new(&from, &to).interpolate(0.5);

        assert_eq!(mid.len(), 3);
        assert_eq!((mid[0].name.as_str(), mid[0].x), ("b.rs", 30.0), "7/b.rs moves");
        assert_eq!((mid[1].parent_id, mid[1].w), (8, 5.0), "8/a.rs enters");
        assert_eq!((mid[2].parent_id, mid[2].w), (7, 5.0), "7/a.rs exits");
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
