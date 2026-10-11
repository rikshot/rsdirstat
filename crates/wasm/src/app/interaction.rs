use super::dom::window_inner_size;
use super::*;
use crate::logic::{build_hover_state, find_navigable_target_index, find_rect_index};
use rsdirstat_protocol::RectKind;

impl TreemapApp {
    /// The pointer position in canvas coordinates. Taken from the event itself (`offsetX/Y` are
    /// relative to the canvas, the listeners' target) rather than from a stored canvas position:
    /// one measured at startup, while the treemap view was still hidden, read as (0, 0) and put
    /// every hover and click 60px below the cursor until the window was resized.
    fn canvas_point(event: &MouseEvent) -> (f64, f64) {
        (event.offset_x() as f64, event.offset_y() as f64)
    }

    pub(super) fn clear_hover(&mut self) -> Result<(), JsValue> {
        self.view.hovered_index = None;
        self.view.hovered_ancestor_indices.clear();
        self.chrome.tooltip_element.style().set_property("display", "none")?;
        self.chrome.path_text_element.set_text_content(Some(""));
        self.chrome.path_size_element.set_text_content(Some(""));
        Ok(())
    }

    pub(super) fn begin_navigation(&mut self, message: ClientMessage) -> Result<(), JsValue> {
        if self.view.zoom_anim.is_some() {
            return Ok(());
        }
        self.clear_hover()?;
        // Only arm the zoom animation once the scan is done. Mid-scan the server streams progress
        // layouts on the same channel, and any of them would otherwise consume this one-shot
        // snapshot and animate against the wrong (non-navigation) target.
        if self.view.scan_done {
            self.view.pending_old_rects = Some(self.view.rects.clone());
        }
        self.send_message(message)
    }

    pub(super) fn recompute_hover(&mut self) -> Result<(), JsValue> {
        let Some((mouse_x, mouse_y)) = self.view.last_mouse else {
            self.view.hovered_index = None;
            self.view.hovered_ancestor_indices.clear();
            return Ok(());
        };
        let hover = build_hover_state(
            &self.view.rects,
            &self.view.rect_index_by_id,
            &self.view.breadcrumb,
            mouse_x,
            mouse_y,
        );
        if let Some(state) = hover {
            self.view.hovered_index = Some(state.hovered_index);
            self.view.hovered_ancestor_indices = state.hovered_ancestor_indices;
            self.chrome.path_text_element.set_text_content(Some(&state.path_text));
            self.chrome
                .path_size_element
                .set_text_content(Some(&format_size(self.view.rects[state.hovered_index].size)));
        } else {
            self.view.hovered_index = None;
            self.view.hovered_ancestor_indices.clear();
            self.chrome.path_text_element.set_text_content(Some(""));
            self.chrome.path_size_element.set_text_content(Some(""));
        }
        Ok(())
    }

    pub(super) fn handle_mousemove(&mut self, event: MouseEvent) -> Result<(), JsValue> {
        if self.view.zoom_anim.is_some() {
            return Ok(());
        }
        self.view.last_mouse = Some(Self::canvas_point(&event));

        let previous = self.view.hovered_index;
        self.recompute_hover()?;
        if self.view.hovered_index != previous {
            self.view.dirty = true;
            self.schedule_tick()?;
            if let Some(rect) = self.view.hovered_index.and_then(|index| self.view.rects.get(index)) {
                self.chrome.tooltip_element.style().set_property("display", "block")?;
                self.chrome.tooltip_name.set_text_content(Some(&rect.name));
                self.chrome.tooltip_size.set_text_content(Some(&format_size(rect.size)));
                let percent = if self.view.view_root_size > 0 {
                    (rect.size as f64 / self.view.view_root_size as f64) * 100.0
                } else {
                    0.0
                };
                self.chrome
                    .tooltip_percent
                    .set_text_content(Some(&format!("{percent:.1}%")));
                let mtime = if rect.mtime > 0 {
                    Date::new(&JsValue::from_f64(rect.mtime as f64 * 1000.0))
                        .to_locale_date_string("default", &JsValue::UNDEFINED)
                        .as_string()
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                self.chrome.tooltip_mtime.set_text_content(Some(&mtime));
            } else {
                self.chrome.tooltip_element.style().set_property("display", "none")?;
            }
        }

        if self.view.hovered_index.is_some() {
            let mut tip_x = event.client_x() as f64 + 14.0;
            let mut tip_y = event.client_y() as f64 + 14.0;
            let (inner_width, inner_height) = window_inner_size(&self.window)?;
            let tooltip_width = self.chrome.tooltip_element.offset_width() as f64;
            let tooltip_height = self.chrome.tooltip_element.offset_height() as f64;
            if tip_x + tooltip_width > inner_width - 8.0 {
                tip_x = event.client_x() as f64 - tooltip_width - 8.0;
            }
            if tip_y + tooltip_height > inner_height - 8.0 {
                tip_y = event.client_y() as f64 - tooltip_height - 8.0;
            }
            self.chrome
                .tooltip_element
                .style()
                .set_property("left", &format!("{tip_x}px"))?;
            self.chrome
                .tooltip_element
                .style()
                .set_property("top", &format!("{tip_y}px"))?;
        }
        Ok(())
    }

    pub(super) fn handle_mouseleave(&mut self) -> Result<(), JsValue> {
        self.view.last_mouse = None;
        self.clear_hover()?;
        self.view.dirty = true;
        self.schedule_tick()
    }

    pub(super) fn handle_canvas_click(&mut self, event: MouseEvent) -> Result<(), JsValue> {
        if self.view.zoom_anim.is_some() {
            return Ok(());
        }
        let (mouse_x, mouse_y) = Self::canvas_point(&event);
        if let Some(index) = find_navigable_target_index(&self.view.rects, mouse_x, mouse_y) {
            let id = self.view.rects[index].id;
            self.begin_navigation(ClientMessage::Navigate { id })?;
        }
        Ok(())
    }

    pub(super) fn handle_context_menu(&mut self, event: MouseEvent) -> Result<(), JsValue> {
        let (mouse_x, mouse_y) = Self::canvas_point(&event);
        if let Some(index) = find_rect_index(&self.view.rects, mouse_x, mouse_y) {
            let rect = &self.view.rects[index];
            if rect.kind == RectKind::File {
                self.send_message(ClientMessage::RevealFile {
                    dir_id: rect.parent_id,
                    name: rect.name.clone(),
                })?;
            } else if rect.kind.is_dir() {
                self.send_message(ClientMessage::RevealDir { id: rect.id })?;
            }
        }
        Ok(())
    }
}
