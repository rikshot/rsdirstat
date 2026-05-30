use std::collections::{HashMap, HashSet};

use rsdirstat_protocol::{BreadcrumbEntry, LayoutRect};

use crate::{collapse_slashes, hit_test_impl, hsl_impl};

/// A laid-out rect ready to draw: geometry (in f64 layout space) plus the cached colour strings
/// derived from its hue. This is the single rect representation used throughout the client.
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
    pub is_files: bool,
    pub is_file: bool,
    pub mtime: i64,
    pub depth: u8,
    pub color_dark: String,
    pub color_border: String,
    pub color_background: Option<String>,
    pub color_header: Option<String>,
}

impl RenderRect {
    pub(crate) fn from_wire(rect: LayoutRect) -> Self {
        Self {
            id: rect.id,
            parent_id: rect.parent_id,
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
            color_dark: hsl_impl(rect.hue, 62, 38),
            color_border: hsl_impl(rect.hue, 60, 28),
            color_background: rect.is_container.then(|| hsl_impl(rect.hue, 25, 13)),
            color_header: rect.is_container.then(|| hsl_impl(rect.hue, 35, 20)),
            name: rect.name,
            size: rect.size,
            is_container: rect.is_container,
            header_height: rect.header_height,
            is_files: rect.is_files,
            is_file: rect.is_file,
            mtime: rect.mtime,
            depth: rect.depth,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HoverState {
    pub hovered_index: Option<usize>,
    pub hovered_ancestor_indices: Vec<usize>,
    pub path_text: String,
    pub size: u64,
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
    let mut target = None;
    for (index, rect) in rects.iter().enumerate() {
        if hit_test(rect, mouse_x, mouse_y) && !rect.is_files && !rect.is_file && rect.id > 0 {
            target = Some(index);
        }
    }
    target
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

pub(crate) fn build_breadcrumb_parts(breadcrumb: &[BreadcrumbEntry]) -> Vec<String> {
    breadcrumb
        .iter()
        .map(|entry| {
            if entry.name.is_empty() {
                "/".to_string()
            } else {
                entry.name.clone()
            }
        })
        .collect()
}

pub(crate) fn build_hover_state(
    rects: &[RenderRect],
    id_to_index: &HashMap<u64, usize>,
    breadcrumb_parts: &[String],
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

    let mut parts = Vec::with_capacity(breadcrumb_parts.len() + hovered_ancestor_indices.len() + 1);
    parts.extend(breadcrumb_parts.iter().cloned());
    for index in hovered_ancestor_indices.iter().rev() {
        parts.push(rects[*index].name.clone());
    }
    parts.push(hovered_rect.name.clone());

    Some(HoverState {
        hovered_index: Some(hovered_index),
        hovered_ancestor_indices,
        path_text: collapse_slashes(parts.join("/")),
        size: hovered_rect.size,
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

#[cfg(test)]
fn interpolate_rects(from: &[RenderRect], to: &[RenderRect], progress: f64) -> Vec<RenderRect> {
    InterpolationPlan::new(from, to).interpolate(progress)
}

fn hit_test(rect: &RenderRect, mouse_x: f64, mouse_y: f64) -> bool {
    hit_test_impl(rect.x, rect.y, rect.w, rect.h, mouse_x, mouse_y)
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
            is_files: false,
            is_file: false,
            mtime: 0,
            depth: 1,
            color_dark: String::new(),
            color_border: String::new(),
            color_background: None,
            color_header: None,
        }
    }

    fn wire_rect(is_container: bool) -> LayoutRect {
        LayoutRect {
            id: 1,
            parent_id: 0,
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
            name: "x".into(),
            hue: 120,
            size: 1,
            depth: 0,
            is_container,
            header_height: 0.0,
            is_files: false,
            is_file: !is_container,
            mtime: 0,
        }
    }

    #[test]
    fn from_wire_colours_only_containers() {
        let container = RenderRect::from_wire(wire_rect(true));
        assert!(container.color_background.is_some());
        assert!(container.color_header.is_some());

        let file = RenderRect::from_wire(wire_rect(false));
        assert!(file.color_background.is_none());
        assert!(file.color_header.is_none());
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
    fn find_navigable_target_skips_files_and_negative_ids() {
        let mut files = rect(-1, 0, 0.0, 0.0, 10.0, 10.0, "files");
        files.is_files = true;
        let mut file = rect(2, 0, 0.0, 0.0, 10.0, 10.0, "file");
        file.is_file = true;
        let dir = rect(3, 0, 0.0, 0.0, 10.0, 10.0, "dir");
        let rects = vec![files, file, dir];
        assert_eq!(find_navigable_target_index(&rects, 5.0, 5.0), Some(2));
    }

    #[test]
    fn build_hover_state_includes_breadcrumb_and_ancestors() {
        let root = rect(1, 0, 0.0, 0.0, 100.0, 100.0, "src");
        let child = rect(2, 1, 10.0, 10.0, 40.0, 40.0, "nested");
        let mut leaf = rect(3, 2, 15.0, 15.0, 10.0, 10.0, "main.rs");
        leaf.size = 42;
        leaf.is_container = false;
        leaf.is_file = true;
        leaf.header_height = 0.0;
        let breadcrumb = vec![BreadcrumbEntry {
            id: 0,
            name: "/tmp".into(),
        }];
        let rects = [root, child, leaf];
        let hover = build_hover_state(
            &rects,
            &build_rect_index(&rects),
            &build_breadcrumb_parts(&breadcrumb),
            16.0,
            16.0,
        )
        .unwrap();
        assert_eq!(hover.hovered_index, Some(2));
        assert_eq!(hover.hovered_ancestor_indices, vec![1, 0]);
        assert_eq!(hover.path_text, "/tmp/src/nested/main.rs");
        assert_eq!(hover.size, 42);
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
        let interpolated = interpolate_rects(&from, &to, 0.5);

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
