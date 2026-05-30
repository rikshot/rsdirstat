use super::*;
use crate::logic::{build_hover_state, find_navigable_target_index, find_rect_index};

impl TreemapApp {
    pub(super) fn clear_hover(&mut self) -> Result<(), JsValue> {
        self.view.hovered_index = None;
        self.view.hovered_ancestor_indices.clear();
        self.chrome.tooltip_element.style().set_property("display", "none")?;
        self.chrome.path_text_element.set_text_content(Some(""));
        self.chrome.path_size_element.set_text_content(Some(""));
        Ok(())
    }

    pub(super) fn begin_navigation(&mut self, message: Vec<u8>) -> Result<(), JsValue> {
        if self.view.zoom_anim.is_some() {
            return Ok(());
        }
        self.clear_hover()?;
        self.view.pending_old_rects = Some(self.view.rects.clone());
        self.send_message(message)
    }

    pub(super) fn recompute_hover(&mut self) -> Result<(), JsValue> {
        if self.view.last_mouse_x < 0.0 {
            self.view.hovered_index = None;
            self.view.hovered_ancestor_indices.clear();
            return Ok(());
        }
        let hover = build_hover_state(
            &self.view.rects,
            &self.view.rect_index_by_id,
            &self.view.breadcrumb_parts,
            self.view.last_mouse_x,
            self.view.last_mouse_y,
        );
        self.view.hovered_index = hover.as_ref().and_then(|state| state.hovered_index);
        self.view.hovered_ancestor_indices = hover
            .as_ref()
            .map(|state| state.hovered_ancestor_indices.clone())
            .unwrap_or_default();
        if let Some(hover) = hover {
            self.chrome.path_text_element.set_text_content(Some(&hover.path_text));
            self.chrome
                .path_size_element
                .set_text_content(Some(&format_size_impl(hover.size as f64)));
        } else {
            self.chrome.path_text_element.set_text_content(Some(""));
            self.chrome.path_size_element.set_text_content(Some(""));
        }
        Ok(())
    }

    pub(super) fn handle_mousemove(&mut self, event: MouseEvent) -> Result<(), JsValue> {
        if self.view.zoom_anim.is_some() {
            return Ok(());
        }
        self.view.last_mouse_x = event.client_x() as f64 - self.surface.canvas_left;
        self.view.last_mouse_y = event.client_y() as f64 - self.surface.canvas_top;

        let previous = self.view.hovered_index;
        self.recompute_hover()?;
        if self.view.hovered_index != previous {
            self.view.dirty = true;
            self.schedule_tick()?;
            if let Some(rect) = self.view.hovered_index.and_then(|index| self.view.rects.get(index)) {
                self.chrome.tooltip_element.style().set_property("display", "block")?;
                self.chrome.tooltip_name.set_text_content(Some(&rect.name));
                self.chrome
                    .tooltip_size
                    .set_text_content(Some(&format_size_impl(rect.size as f64)));
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
            let inner_width = self
                .window
                .inner_width()?
                .as_f64()
                .ok_or_else(|| JsValue::from_str("missing innerWidth"))?;
            let inner_height = self
                .window
                .inner_height()?
                .as_f64()
                .ok_or_else(|| JsValue::from_str("missing innerHeight"))?;
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
        self.view.last_mouse_x = -1.0;
        self.view.last_mouse_y = -1.0;
        self.clear_hover()?;
        self.view.dirty = true;
        self.schedule_tick()
    }

    pub(super) fn handle_canvas_click(&mut self, event: MouseEvent) -> Result<(), JsValue> {
        if self.view.zoom_anim.is_some() {
            return Ok(());
        }
        let mouse_x = event.client_x() as f64 - self.surface.canvas_left;
        let mouse_y = event.client_y() as f64 - self.surface.canvas_top;
        if let Some(index) = find_navigable_target_index(&self.view.rects, mouse_x, mouse_y) {
            let id = self.view.rects[index].id as u64;
            self.begin_navigation(wire::ClientMessage::Navigate { id }.encode())?;
        }
        Ok(())
    }

    pub(super) fn handle_context_menu(&mut self, event: MouseEvent) -> Result<(), JsValue> {
        let mouse_x = event.client_x() as f64 - self.surface.canvas_left;
        let mouse_y = event.client_y() as f64 - self.surface.canvas_top;
        if let Some(index) = find_rect_index(&self.view.rects, mouse_x, mouse_y) {
            let rect = &self.view.rects[index];
            if rect.is_file {
                self.send_message(
                    wire::ClientMessage::RevealFile {
                        dir_id: rect.parent_id,
                        name: rect.name.clone(),
                    }
                    .encode(),
                )?;
            } else if rect.id > 0 {
                self.send_message(wire::ClientMessage::RevealDir { id: rect.id as u64 }.encode())?;
            }
        }
        Ok(())
    }
}
