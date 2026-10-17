use super::dom::{delegated_attribute, listen, set_hidden, with_app};
use super::network::fetch_volumes;
use super::*;

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
        // One delegated listener on the picker grid, not a closure per volume card: each refresh
        // replaces the cards, and their closures would have to be kept and dropped with them.
        listen(&chrome.picker_grid, "click", |app, event: Event| {
            app.handle_picker_event(event)
        })?;
        // A card takes Enter and Space too, as a button does, so the picker works from the keyboard.
        listen(&chrome.picker_grid, "keydown", |app, event: web_sys::KeyboardEvent| {
            if !matches!(event.key().as_str(), "Enter" | " ") {
                return Ok(());
            }
            event.prevent_default();
            app.handle_picker_event(event.into())
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
        // The `#color-mode` options are 0 (type) and 1 (age).
        let mode = if self.chrome.color_mode_select.value() == "1" {
            wire::ColorMode::Age
        } else {
            wire::ColorMode::Type
        };
        self.send_message(ClientMessage::ColorMode { mode })
    }

    pub(super) fn schedule_filter_send(&mut self) -> Result<(), JsValue> {
        self.cancel_filter_send();
        let id = self.window.set_timeout_with_callback_and_timeout_and_arguments_0(
            self.callbacks.filter_send.as_ref().unchecked_ref(),
            300,
        )?;
        self.session.filter_timer_id = Some(id);
        Ok(())
    }

    /// Cancel the debounced filter send, if one is pending.
    fn cancel_filter_send(&mut self) {
        if let Some(id) = self.session.filter_timer_id.take() {
            self.window.clear_timeout_with_handle(id);
        }
    }

    pub(super) fn clear_filter(&mut self) -> Result<(), JsValue> {
        self.chrome.filter_ext_input.set_value("");
        self.chrome.filter_name_input.set_value("");
        self.chrome.filter_min_input.set_value("");
        self.send_filter_now()
    }

    /// Send the filter as the toolbar has it, superseding a debounced send still pending.
    pub(super) fn send_filter_now(&mut self) -> Result<(), JsValue> {
        self.cancel_filter_send();
        let extensions = self
            .chrome
            .filter_ext_input
            .value()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Box::from)
            .collect();
        let min_value = self.chrome.filter_min_input.value().parse::<f64>().unwrap_or(0.0);
        let min_unit = self.chrome.filter_min_unit_select.value().parse::<u64>().unwrap_or(1);
        self.send_message(ClientMessage::Filter {
            extensions,
            min_size: (min_value * min_unit as f64).floor() as u64,
            name_pattern: self.chrome.filter_name_input.value().trim().to_string(),
        })
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
            span.set_text_content(Some(&entry.name));
            if index + 1 == self.view.breadcrumb.len() {
                span.set_class_name("current");
            } else {
                span.set_attribute("data-breadcrumb-index", &index.to_string())?;
            }
            self.chrome.breadcrumb_bar.append_child(&span)?;
        }
        self.view.rendered_breadcrumb = self.view.breadcrumb.clone();
        self.scroll_breadcrumb_to_end();
        Ok(())
    }

    /// Show the end of a breadcrumb too long for its bar, the current folder and its nearest parents,
    /// rather than the root's end: the bar clips what doesn't fit.
    pub(super) fn scroll_breadcrumb_to_end(&self) {
        let bar = &self.chrome.breadcrumb_bar;
        bar.set_scroll_left(bar.scroll_width());
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
        self.set_picker_message("Loading volumes...");
        spawn_local(async move {
            let result = fetch_volumes().await;
            with_app(|app| {
                let _ = app.borrow_mut().apply_volume_result(result);
            });
        });
        Ok(())
    }

    /// Fill the picker grid with just `message`.
    fn set_picker_message(&self, message: &str) {
        self.chrome
            .picker_grid
            .set_inner_html(&format!(r#"<div class="picker-loading">{message}</div>"#));
    }

    pub(super) fn apply_volume_result(&mut self, result: Result<Vec<VolumeInfo>, JsValue>) -> Result<(), JsValue> {
        let message = match result {
            Ok(volumes) if !volumes.is_empty() => {
                self.chrome.picker_grid.set_inner_html("");
                for volume in volumes {
                    self.chrome
                        .picker_grid
                        .append_child(self.create_volume_card(volume)?.as_ref())?;
                }
                return Ok(());
            }
            Ok(_) => "No volumes found",
            Err(_) => "Failed to load volumes",
        };
        self.set_picker_message(message);
        Ok(())
    }

    /// A new `<div>` of class `class` holding `text`, at the end of `parent`.
    fn append_div(&self, parent: &Element, class: &str, text: &str) -> Result<Element, JsValue> {
        let div = self.document.create_element("div")?;
        div.set_class_name(class);
        div.set_text_content(Some(text));
        parent.append_child(&div)?;
        Ok(div)
    }

    pub(super) fn create_volume_card(&self, volume: VolumeInfo) -> Result<Element, JsValue> {
        let card = self.document.create_element("div")?;
        card.set_class_name("volume-card");
        self.append_div(&card, "volume-name", &volume.name)?;
        self.append_div(&card, "volume-path", &volume.mount_point)?;
        let bar = self.append_div(&card, "volume-bar", "")?;
        let fill = self.append_div(&bar, "volume-bar-fill", "")?;
        let used_percent = percent(volume.used_bytes, volume.total_bytes);
        fill.set_attribute("style", &format!("width: {used_percent:.1}%"))?;
        let sizes = format!(
            "{} used of {}",
            format_size(volume.used_bytes),
            format_size(volume.total_bytes)
        );
        self.append_div(&card, "volume-sizes", &sizes)?;
        if !volume.fs_type.is_empty() {
            self.append_div(&card, "volume-fs", &volume.fs_type)?;
        }

        // The delegated picker-grid listeners read this to start the scan; no per-card closure.
        card.set_attribute("data-mount", &volume.mount_point)?;
        // Focusable and announced as a button, which the keydown listener makes it act as.
        card.set_attribute("tabindex", "0")?;
        card.set_attribute("role", "button")?;

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
