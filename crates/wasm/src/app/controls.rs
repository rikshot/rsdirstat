use super::dom::{delegated_attribute, listen, set_hidden, with_app};
use super::network::fetch_volumes;
use super::*;
use crate::logic::crumb_label;

impl TreemapApp {
    pub(super) fn install_event_listeners(&self) -> Result<(), JsValue> {
        let canvas = &self.surface.canvas;
        listen(canvas, "mousemove", |app, event: MouseEvent| {
            app.handle_mousemove(event)
        })?;
        listen(canvas, "mouseleave", |app, _: Event| app.handle_mouseleave())?;
        listen(canvas, "click", |app, event: MouseEvent| app.handle_canvas_click(event))?;
        listen(canvas, "contextmenu", |app, event: MouseEvent| {
            event.prevent_default();
            app.handle_context_menu(event)
        })?;
        listen(&self.window, "resize", |app, _: Event| app.handle_resize())?;

        let chrome = &self.chrome;
        listen(&chrome.breadcrumb_bar, "click", |app, event: Event| {
            app.handle_breadcrumb_event(event)
        })?;
        // One delegated listener on the picker grid instead of a closure per volume card (which
        // leaked one forgotten closure per card on every picker refresh).
        listen(&chrome.picker_grid, "click", |app, event: Event| {
            app.handle_picker_event(event)
        })?;
        listen(&chrome.depth_select, "change", |app, _: Event| {
            app.handle_depth_change()
        })?;
        listen(&chrome.color_mode_select, "change", |app, _: Event| {
            app.handle_color_mode_change()
        })?;
        for input in [
            &chrome.filter_ext_input,
            &chrome.filter_name_input,
            &chrome.filter_min_input,
        ] {
            listen(input, "input", |app, _: Event| app.schedule_filter_send())?;
        }
        listen(&chrome.filter_min_unit_select, "change", |app, _: Event| {
            app.schedule_filter_send()
        })?;
        listen(&chrome.filter_clear_button, "click", |app, _: Event| app.clear_filter())?;
        listen(&chrome.rescan_button, "click", |app, _: Event| {
            app.send_message(ClientMessage::Rescan)
        })?;
        listen(&chrome.picker_refresh_button, "click", |app, _: Event| {
            app.load_volumes()
        })?;
        listen(&chrome.change_button, "click", |app, _: Event| app.show_picker())
    }

    pub(super) fn handle_depth_change(&mut self) -> Result<(), JsValue> {
        let depth = self.chrome.depth_select.value().parse().unwrap_or(wire::DEFAULT_DEPTH);
        self.send_message(ClientMessage::SetDepth { depth })
    }

    pub(super) fn handle_color_mode_change(&mut self) -> Result<(), JsValue> {
        let mode = self
            .chrome
            .color_mode_select
            .value()
            .parse()
            .unwrap_or(wire::COLOR_MODE_TYPE);
        self.send_message(ClientMessage::ColorMode { mode })
    }

    pub(super) fn schedule_filter_send(&mut self) -> Result<(), JsValue> {
        if let Some(id) = self.session.filter_timer_id.take() {
            self.window.clear_timeout_with_handle(id);
        }
        let callback = Closure::once_into_js(move || {
            with_app(|app| {
                let _ = app.borrow_mut().send_filter_now();
            });
        });
        let id = self
            .window
            .set_timeout_with_callback_and_timeout_and_arguments_0(callback.unchecked_ref::<Function>(), 300)?;
        self.session.filter_timer_id = Some(id);
        Ok(())
    }

    pub(super) fn clear_filter(&mut self) -> Result<(), JsValue> {
        // Cancel any debounced filter send still pending, or it would fire after the clear and
        // re-send the (now empty) inputs.
        if let Some(id) = self.session.filter_timer_id.take() {
            self.window.clear_timeout_with_handle(id);
        }
        self.chrome.filter_ext_input.set_value("");
        self.chrome.filter_name_input.set_value("");
        self.chrome.filter_min_input.set_value("");
        self.send_message(ClientMessage::ClearFilter)
    }

    pub(super) fn send_filter_now(&mut self) -> Result<(), JsValue> {
        self.session.filter_timer_id = None;
        let extensions = self
            .chrome
            .filter_ext_input
            .value()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Box::from)
            .collect();
        self.send_message(ClientMessage::FilterExt { extensions })?;

        let min_value = self.chrome.filter_min_input.value().parse::<f64>().unwrap_or(0.0);
        let min_unit = self.chrome.filter_min_unit_select.value().parse::<u64>().unwrap_or(1);
        self.send_message(ClientMessage::FilterSize {
            min: (min_value * min_unit as f64).floor() as u64,
            max: 0,
        })?;
        let pattern = self.chrome.filter_name_input.value().trim().to_string();
        self.send_message(ClientMessage::FilterName { pattern })
    }

    pub(super) fn build_breadcrumb(&mut self) -> Result<(), JsValue> {
        if self.view.rendered_breadcrumb == self.view.breadcrumb {
            return Ok(());
        }
        self.chrome.breadcrumb_bar.set_inner_html("");
        for (index, entry) in self.view.breadcrumb.iter().enumerate() {
            if index > 0 {
                let separator = self.document.create_element("span")?;
                separator.set_class_name("separator");
                separator.set_text_content(Some("/"));
                self.chrome.breadcrumb_bar.append_child(&separator)?;
            }
            let span = self.document.create_element("span")?;
            span.set_text_content(Some(crumb_label(entry)));
            if index + 1 == self.view.breadcrumb.len() {
                span.set_class_name("current");
            } else {
                span.set_attribute("data-breadcrumb-index", &index.to_string())?;
            }
            self.chrome.breadcrumb_bar.append_child(&span)?;
        }
        self.view.rendered_breadcrumb = self.view.breadcrumb.clone();
        Ok(())
    }

    pub(super) fn handle_breadcrumb_event(&mut self, event: Event) -> Result<(), JsValue> {
        match delegated_attribute(&event, "data-breadcrumb-index").and_then(|index| index.parse().ok()) {
            Some(index) => self.handle_breadcrumb_click(index),
            None => Ok(()),
        }
    }

    pub(super) fn handle_breadcrumb_click(&mut self, index: usize) -> Result<(), JsValue> {
        if index + 1 >= self.view.breadcrumb.len() {
            return Ok(());
        }
        self.begin_navigation(ClientMessage::Navigate {
            id: self.view.breadcrumb[index].id,
        })
    }

    pub(super) fn show_picker(&mut self) -> Result<(), JsValue> {
        set_hidden(&self.chrome.treemap_view, true)?;
        set_hidden(&self.chrome.picker_element, false)?;
        self.load_volumes()
    }

    pub(super) fn hide_picker(&mut self) -> Result<(), JsValue> {
        set_hidden(&self.chrome.picker_element, true)?;
        set_hidden(&self.chrome.treemap_view, false)?;
        self.resize()
    }

    pub(super) fn load_volumes(&mut self) -> Result<(), JsValue> {
        self.chrome
            .picker_grid
            .set_inner_html(r#"<div class="picker-loading">Loading volumes...</div>"#);
        spawn_local(async move {
            let result = fetch_volumes().await;
            with_app(|app| {
                let _ = app.borrow_mut().apply_volume_result(result);
            });
        });
        Ok(())
    }

    pub(super) fn apply_volume_result(&mut self, result: Result<Vec<VolumeInfo>, JsValue>) -> Result<(), JsValue> {
        self.chrome.picker_grid.set_inner_html("");
        match result {
            Ok(volumes) if volumes.is_empty() => {
                self.chrome
                    .picker_grid
                    .set_inner_html(r#"<div class="picker-loading">No volumes found</div>"#);
            }
            Ok(volumes) => {
                for volume in volumes {
                    self.chrome
                        .picker_grid
                        .append_child(self.create_volume_card(volume)?.as_ref())?;
                }
            }
            Err(_) => {
                self.chrome
                    .picker_grid
                    .set_inner_html(r#"<div class="picker-loading">Failed to load volumes</div>"#);
            }
        }
        Ok(())
    }

    pub(super) fn create_volume_card(&self, volume: VolumeInfo) -> Result<Element, JsValue> {
        let card = self.document.create_element("div")?;
        card.set_class_name("volume-card");

        let name = self.document.create_element("div")?;
        name.set_class_name("volume-name");
        name.set_text_content(Some(&volume.name));
        card.append_child(&name)?;

        let path = self.document.create_element("div")?;
        path.set_class_name("volume-path");
        path.set_text_content(Some(&volume.mount_point));
        card.append_child(&path)?;

        let bar = self.document.create_element("div")?;
        bar.set_class_name("volume-bar");
        let fill = self.document.create_element("div")?;
        fill.set_class_name("volume-bar-fill");
        let used_percent = if volume.total_bytes > 0 {
            (volume.used_bytes as f64 / volume.total_bytes as f64) * 100.0
        } else {
            0.0
        };
        fill.dyn_ref::<HtmlElement>()
            .ok_or_else(|| JsValue::from_str("volume bar fill is not HtmlElement"))?
            .style()
            .set_property("width", &format!("{used_percent:.1}%"))?;
        bar.append_child(&fill)?;
        card.append_child(&bar)?;

        let sizes = self.document.create_element("div")?;
        sizes.set_class_name("volume-sizes");
        sizes.set_text_content(Some(&format!(
            "{} used of {}",
            format_size(volume.used_bytes),
            format_size(volume.total_bytes)
        )));
        card.append_child(&sizes)?;

        if !volume.fs_type.is_empty() {
            let fs_type = self.document.create_element("div")?;
            fs_type.set_class_name("volume-fs");
            fs_type.set_text_content(Some(&volume.fs_type));
            card.append_child(&fs_type)?;
        }

        // The delegated picker-grid listener reads this to start the scan; no per-card closure.
        card.set_attribute("data-mount", &volume.mount_point)?;

        Ok(card)
    }

    pub(super) fn handle_picker_event(&mut self, event: Event) -> Result<(), JsValue> {
        match delegated_attribute(&event, "data-mount") {
            Some(path) => self.scan_path(path),
            None => Ok(()),
        }
    }

    pub(super) fn scan_path(&mut self, path: String) -> Result<(), JsValue> {
        self.hide_picker()?;
        self.send_message(ClientMessage::ScanPath { path })
    }
}
