use std::collections::{HashMap, HashSet};

use rsdirstat_protocol::BreadcrumbEntry;

use crate::{collapse_slashes, hit_test_impl};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TreemapRect {
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
    Matched { from: TreemapRect, to: TreemapRect },
    Entering { to: TreemapRect },
    Exiting { from: TreemapRect },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InterpolationPlan {
    steps: Vec<InterpolationStep>,
}

pub(crate) trait RectLike {
    fn id(&self) -> i64;
    fn parent_id(&self) -> u64;
    fn x(&self) -> f64;
    fn y(&self) -> f64;
    fn w(&self) -> f64;
    fn h(&self) -> f64;
    fn name(&self) -> &str;
    fn size(&self) -> u64;
    fn is_container(&self) -> bool;
    fn header_height(&self) -> f64;
    fn is_files(&self) -> bool;
    fn is_file(&self) -> bool;
    fn mtime(&self) -> i64;
}

impl RectLike for TreemapRect {
    fn id(&self) -> i64 {
        self.id
    }

    fn parent_id(&self) -> u64 {
        self.parent_id
    }

    fn x(&self) -> f64 {
        self.x
    }

    fn y(&self) -> f64 {
        self.y
    }

    fn w(&self) -> f64 {
        self.w
    }

    fn h(&self) -> f64 {
        self.h
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn is_container(&self) -> bool {
        self.is_container
    }

    fn header_height(&self) -> f64 {
        self.header_height
    }

    fn is_files(&self) -> bool {
        self.is_files
    }

    fn is_file(&self) -> bool {
        self.is_file
    }

    fn mtime(&self) -> i64 {
        self.mtime
    }
}

pub(crate) fn find_rect_index<R: RectLike>(rects: &[R], mouse_x: f64, mouse_y: f64) -> Option<usize> {
    rects
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, rect)| hit_test(rect, mouse_x, mouse_y).then_some(index))
}

pub(crate) fn find_navigable_target_index<R: RectLike>(rects: &[R], mouse_x: f64, mouse_y: f64) -> Option<usize> {
    let mut target = None;
    for (index, rect) in rects.iter().enumerate() {
        if hit_test(rect, mouse_x, mouse_y) && !rect.is_files() && !rect.is_file() && rect.id() > 0 {
            target = Some(index);
        }
    }
    target
}

pub(crate) fn build_rect_index<R: RectLike>(rects: &[R]) -> HashMap<u64, usize> {
    let mut id_to_index = HashMap::with_capacity(rects.len());
    for (index, rect) in rects.iter().enumerate() {
        if rect.id() > 0 {
            id_to_index.insert(rect.id() as u64, index);
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

pub(crate) fn build_hover_state<R: RectLike>(
    rects: &[R],
    id_to_index: &HashMap<u64, usize>,
    breadcrumb_parts: &[String],
    mouse_x: f64,
    mouse_y: f64,
) -> Option<HoverState> {
    let hovered_index = find_rect_index(rects, mouse_x, mouse_y)?;
    let hovered_rect = &rects[hovered_index];
    let mut current_parent = hovered_rect.parent_id();
    let mut hovered_ancestor_indices = Vec::new();
    while let Some(index) = id_to_index.get(&current_parent).copied() {
        let rect = &rects[index];
        if rect.is_container() {
            hovered_ancestor_indices.push(index);
        }
        current_parent = rect.parent_id();
    }

    let mut parts = Vec::with_capacity(breadcrumb_parts.len() + hovered_ancestor_indices.len() + 1);
    parts.extend(breadcrumb_parts.iter().cloned());
    for index in hovered_ancestor_indices.iter().rev() {
        parts.push(rects[*index].name().to_owned());
    }
    parts.push(hovered_rect.name().to_owned());

    Some(HoverState {
        hovered_index: Some(hovered_index),
        hovered_ancestor_indices,
        path_text: collapse_slashes(parts.join("/")),
        size: hovered_rect.size(),
    })
}

impl InterpolationPlan {
    pub(crate) fn new<R: RectLike>(from: &[R], to: &[R]) -> Self {
        let mut from_by_id = HashMap::with_capacity(from.len());
        for rect in from {
            from_by_id.insert(rect.id(), rect);
        }
        let mut seen = HashSet::with_capacity(to.len());
        let mut steps = Vec::with_capacity(to.len() + from.len());

        for to_rect in to {
            seen.insert(to_rect.id());
            if let Some(from_rect) = from_by_id.get(&to_rect.id()) {
                steps.push(InterpolationStep::Matched {
                    from: clone_rect(*from_rect),
                    to: clone_rect(to_rect),
                });
            } else {
                steps.push(InterpolationStep::Entering {
                    to: clone_rect(to_rect),
                });
            }
        }

        for from_rect in from {
            if seen.contains(&from_rect.id()) {
                continue;
            }
            steps.push(InterpolationStep::Exiting {
                from: clone_rect(from_rect),
            });
        }

        Self { steps }
    }

    pub(crate) fn interpolate(&self, progress: f64) -> Vec<TreemapRect> {
        let clamped_progress = progress.clamp(0.0, 1.0);
        let inverse = 1.0 - clamped_progress;
        let mut result = Vec::with_capacity(self.steps.len());

        for step in &self.steps {
            match step {
                InterpolationStep::Matched { from, to } => {
                    result.push(TreemapRect {
                        id: to.id,
                        parent_id: to.parent_id,
                        x: from.x + (to.x - from.x) * clamped_progress,
                        y: from.y + (to.y - from.y) * clamped_progress,
                        w: from.w + (to.w - from.w) * clamped_progress,
                        h: from.h + (to.h - from.h) * clamped_progress,
                        name: to.name.clone(),
                        size: to.size,
                        is_container: to.is_container,
                        header_height: to.header_height,
                        is_files: to.is_files,
                        is_file: to.is_file,
                        mtime: to.mtime,
                    });
                }
                InterpolationStep::Entering { to } => {
                    result.push(TreemapRect {
                        id: to.id,
                        parent_id: to.parent_id,
                        x: to.x + to.w * 0.5 * inverse,
                        y: to.y + to.h * 0.5 * inverse,
                        w: to.w * clamped_progress,
                        h: to.h * clamped_progress,
                        name: to.name.clone(),
                        size: to.size,
                        is_container: to.is_container,
                        header_height: to.header_height,
                        is_files: to.is_files,
                        is_file: to.is_file,
                        mtime: to.mtime,
                    });
                }
                InterpolationStep::Exiting { from } => {
                    result.push(TreemapRect {
                        id: from.id,
                        parent_id: from.parent_id,
                        x: from.x + from.w * 0.5 * clamped_progress,
                        y: from.y + from.h * 0.5 * clamped_progress,
                        w: from.w * inverse,
                        h: from.h * inverse,
                        name: from.name.clone(),
                        size: from.size,
                        is_container: from.is_container,
                        header_height: from.header_height,
                        is_files: from.is_files,
                        is_file: from.is_file,
                        mtime: from.mtime,
                    });
                }
            }
        }

        result
    }
}

#[cfg(test)]
fn interpolate_rects<R: RectLike>(from: &[R], to: &[R], progress: f64) -> Vec<TreemapRect> {
    InterpolationPlan::new(from, to).interpolate(progress)
}

fn hit_test<R: RectLike>(rect: &R, mouse_x: f64, mouse_y: f64) -> bool {
    hit_test_impl(rect.x(), rect.y(), rect.w(), rect.h(), mouse_x, mouse_y)
}

fn clone_rect<R: RectLike>(rect: &R) -> TreemapRect {
    TreemapRect {
        id: rect.id(),
        parent_id: rect.parent_id(),
        x: rect.x(),
        y: rect.y(),
        w: rect.w(),
        h: rect.h(),
        name: rect.name().to_owned(),
        size: rect.size(),
        is_container: rect.is_container(),
        header_height: rect.header_height(),
        is_files: rect.is_files(),
        is_file: rect.is_file(),
        mtime: rect.mtime(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(id: i64, parent_id: u64, x: f64, y: f64, w: f64, h: f64, name: &str) -> TreemapRect {
        TreemapRect {
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
        }
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
        let leaf = TreemapRect {
            id: 3,
            parent_id: 2,
            x: 15.0,
            y: 15.0,
            w: 10.0,
            h: 10.0,
            name: "main.rs".into(),
            size: 42,
            is_container: false,
            header_height: 0.0,
            is_files: false,
            is_file: true,
            mtime: 0,
        };
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
