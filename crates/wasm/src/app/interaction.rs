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
        self.view.hover = None;
        self.show_hover_path();
        self.chrome.tooltip_element.style().set_property("display", "none")
    }

    /// The hovered rect's path and size in the path bar, or nothing.
    fn show_hover_path(&self) {
        let (path, size) = match &self.view.hover {
            Some(hover) => (
                hover.path_text.as_str(),
                format_size(self.view.rects[hover.hovered_index].size),
            ),
            None => ("", String::new()),
        };
        self.chrome.path_text_element.set_text_content(Some(path));
        self.chrome.path_size_element.set_text_content(Some(&size));
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

    pub(super) fn recompute_hover(&mut self) {
        self.view.hover = self.view.last_mouse.and_then(|(mouse_x, mouse_y)| {
            build_hover_state(
                &self.view.rects,
                &self.view.rect_index_by_id,
                &self.view.breadcrumb,
                self.view.separator,
                mouse_x,
                mouse_y,
            )
        });
        self.show_hover_path();
    }

    pub(super) fn handle_mousemove(&mut self, event: MouseEvent) -> Result<(), JsValue> {
        if self.view.zoom_anim.is_some() {
            return Ok(());
        }
        let (mouse_x, mouse_y) = Self::canvas_point(&event);
        self.view.last_mouse = Some((mouse_x, mouse_y));

        // Most moves stay on the same rect, and only the tooltip follows the pointer.
        let hovered = self.view.hover.as_ref().map(|hover| hover.hovered_index);
        if find_rect_index(&self.view.rects, mouse_x, mouse_y) != hovered {
            self.recompute_hover();
            self.redraw(false)?;
            self.fill_tooltip()?;
        }

        if self.view.hover.is_some() {
            let (tooltip_width, tooltip_height) = self.view.tooltip_size;
            let mut tip_x = event.client_x() as f64 + 14.0;
            let mut tip_y = event.client_y() as f64 + 14.0;
            let (inner_width, inner_height) = window_inner_size(&self.window)?;
            if tip_x + tooltip_width > inner_width - 8.0 {
                tip_x = event.client_x() as f64 - tooltip_width - 8.0;
            }
            if tip_y + tooltip_height > inner_height - 8.0 {
                tip_y = event.client_y() as f64 - tooltip_height - 8.0;
            }
            let style = self.chrome.tooltip_element.style();
            style.set_property("left", &format!("{tip_x}px"))?;
            style.set_property("top", &format!("{tip_y}px"))?;
        }
        Ok(())
    }

    /// Show the hovered rect's details in the tooltip and measure it, or hide it.
    fn fill_tooltip(&mut self) -> Result<(), JsValue> {
        let tooltip = &self.chrome.tooltip_element;
        let Some(rect) = self
            .view
            .hover
            .as_ref()
            .and_then(|hover| self.view.rects.get(hover.hovered_index))
        else {
            return tooltip.style().set_property("display", "none");
        };
        tooltip.style().set_property("display", "block")?;
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
        self.view.tooltip_size = (tooltip.offset_width() as f64, tooltip.offset_height() as f64);
        Ok(())
    }

    pub(super) fn handle_mouseleave(&mut self) -> Result<(), JsValue> {
        self.view.last_mouse = None;
        self.clear_hover()?;
        self.redraw(false)
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
