use super::dom::{add_class, remove_class, with_app};
use super::network::fetch_volumes;
use super::*;

impl TreemapApp {
    pub(super) fn install_event_listeners() -> Result<(), JsValue> {
        let mousemove = Closure::wrap(Box::new(move |event: MouseEvent| {
            with_app(|app| {
                let _ = app.borrow_mut().handle_mousemove(event);
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let _ = app
                .borrow()
                .surface
                .canvas
                .add_event_listener_with_callback("mousemove", mousemove.as_ref().unchecked_ref());
        });
        mousemove.forget();

        let mouseleave = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                let _ = app.borrow_mut().handle_mouseleave();
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let _ = app
                .borrow()
                .surface
                .canvas
                .add_event_listener_with_callback("mouseleave", mouseleave.as_ref().unchecked_ref());
        });
        mouseleave.forget();

        let click = Closure::wrap(Box::new(move |event: MouseEvent| {
            with_app(|app| {
                let _ = app.borrow_mut().handle_canvas_click(event);
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let _ = app
                .borrow()
                .surface
                .canvas
                .add_event_listener_with_callback("click", click.as_ref().unchecked_ref());
        });
        click.forget();

        let contextmenu = Closure::wrap(Box::new(move |event: MouseEvent| {
            event.prevent_default();
            with_app(|app| {
                let _ = app.borrow_mut().handle_context_menu(event);
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let _ = app
                .borrow()
                .surface
                .canvas
                .add_event_listener_with_callback("contextmenu", contextmenu.as_ref().unchecked_ref());
        });
        contextmenu.forget();

        let resize = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                let _ = app.borrow_mut().handle_resize();
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let _ = app
                .borrow()
                .window
                .add_event_listener_with_callback("resize", resize.as_ref().unchecked_ref());
        });
        resize.forget();

        let breadcrumb_click = Closure::wrap(Box::new(move |event: Event| {
            with_app(|app| {
                let _ = app.borrow_mut().handle_breadcrumb_event(event);
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let _ = app
                .borrow()
                .chrome
                .breadcrumb_bar
                .add_event_listener_with_callback("click", breadcrumb_click.as_ref().unchecked_ref());
        });
        breadcrumb_click.forget();

        let depth_change = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                let _ = app.borrow_mut().handle_depth_change();
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let _ = app
                .borrow()
                .chrome
                .depth_select
                .add_event_listener_with_callback("change", depth_change.as_ref().unchecked_ref());
        });
        depth_change.forget();

        let color_change = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                let _ = app.borrow_mut().handle_color_mode_change();
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let _ = app
                .borrow()
                .chrome
                .color_mode_select
                .add_event_listener_with_callback("change", color_change.as_ref().unchecked_ref());
        });
        color_change.forget();

        let filter_input = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                let _ = app.borrow_mut().schedule_filter_send();
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let app = app.borrow();
            let _ = app
                .chrome
                .filter_ext_input
                .add_event_listener_with_callback("input", filter_input.as_ref().unchecked_ref());
            let _ = app
                .chrome
                .filter_name_input
                .add_event_listener_with_callback("input", filter_input.as_ref().unchecked_ref());
            let _ = app
                .chrome
                .filter_min_input
                .add_event_listener_with_callback("input", filter_input.as_ref().unchecked_ref());
        });
        filter_input.forget();

        let filter_min_unit = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                let _ = app.borrow_mut().schedule_filter_send();
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let _ = app
                .borrow()
                .chrome
                .filter_min_unit_select
                .add_event_listener_with_callback("change", filter_min_unit.as_ref().unchecked_ref());
        });
        filter_min_unit.forget();

        let filter_clear = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                let _ = app.borrow_mut().clear_filter();
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let _ = app
                .borrow()
                .chrome
                .filter_clear_button
                .add_event_listener_with_callback("click", filter_clear.as_ref().unchecked_ref());
        });
        filter_clear.forget();

        let rescan = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                let _ = app.borrow().send_message(wire::encode_rescan());
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let _ = app
                .borrow()
                .chrome
                .rescan_button
                .add_event_listener_with_callback("click", rescan.as_ref().unchecked_ref());
        });
        rescan.forget();

        let refresh = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                let _ = app.borrow_mut().load_volumes();
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let _ = app
                .borrow()
                .chrome
                .picker_refresh_button
                .add_event_listener_with_callback("click", refresh.as_ref().unchecked_ref());
        });
        refresh.forget();

        let change = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                let _ = app.borrow_mut().show_picker();
            });
        }) as Box<dyn FnMut(_)>);
        with_app(|app| {
            let _ = app
                .borrow()
                .chrome
                .change_button
                .add_event_listener_with_callback("click", change.as_ref().unchecked_ref());
        });
        change.forget();

        Ok(())
    }

    pub(super) fn handle_depth_change(&mut self) -> Result<(), JsValue> {
        let depth = self.chrome.depth_select.value().parse::<u8>().unwrap_or(5);
        self.send_message(wire::encode_set_depth(depth))
    }

    pub(super) fn handle_color_mode_change(&mut self) -> Result<(), JsValue> {
        let mode = self.chrome.color_mode_select.value().parse::<u8>().unwrap_or(0);
        self.send_message(wire::encode_color_mode(mode))
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
        self.chrome.filter_ext_input.set_value("");
        self.chrome.filter_name_input.set_value("");
        self.chrome.filter_min_input.set_value("");
        self.send_message(wire::encode_clear_filter())
    }

    pub(super) fn send_filter_now(&mut self) -> Result<(), JsValue> {
        self.session.filter_timer_id = None;
        let ext_value = self.chrome.filter_ext_input.value().trim().to_string();
        let extensions = if ext_value.is_empty() {
            Vec::new()
        } else {
            ext_value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        };
        self.send_message(wire::encode_filter_ext(&extensions))?;

        let min_value = self.chrome.filter_min_input.value().parse::<f64>().unwrap_or(0.0);
        let min_unit = self.chrome.filter_min_unit_select.value().parse::<u64>().unwrap_or(1);
        self.send_message(wire::encode_filter_size(
            (min_value * min_unit as f64).floor() as u64,
            0,
        ))?;
        self.send_message(wire::encode_filter_name(self.chrome.filter_name_input.value().trim()))
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
            span.set_text_content(Some(if entry.name.is_empty() { "/" } else { &entry.name }));
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
        let Some(target) = event.target() else {
            return Ok(());
        };
        let Ok(mut element) = target.dyn_into::<Element>() else {
            return Ok(());
        };
        loop {
            if let Some(index) = element
                .get_attribute("data-breadcrumb-index")
                .and_then(|value| value.parse::<usize>().ok())
            {
                return self.handle_breadcrumb_click(index);
            }
            let Some(parent) = element.parent_element() else {
                return Ok(());
            };
            element = parent;
        }
    }

    pub(super) fn handle_breadcrumb_click(&mut self, index: usize) -> Result<(), JsValue> {
        if index + 1 >= self.view.breadcrumb.len() {
            return Ok(());
        }
        self.begin_navigation(wire::encode_navigate(self.view.breadcrumb[index].id))
    }

    pub(super) fn show_picker(&mut self) -> Result<(), JsValue> {
        add_class(&self.chrome.treemap_view, "hidden")?;
        remove_class(&self.chrome.picker_element, "hidden")?;
        self.load_volumes()
    }

    pub(super) fn hide_picker(&mut self) -> Result<(), JsValue> {
        add_class(&self.chrome.picker_element, "hidden")?;
        remove_class(&self.chrome.treemap_view, "hidden")?;
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

    pub(super) fn apply_volume_result(&mut self, result: Result<Vec<VolumeEntry>, JsValue>) -> Result<(), JsValue> {
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

    pub(super) fn create_volume_card(&self, volume: VolumeEntry) -> Result<Element, JsValue> {
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
            format_size_impl(volume.used_bytes as f64),
            format_size_impl(volume.total_bytes as f64)
        )));
        card.append_child(&sizes)?;

        if !volume.fs_type.is_empty() {
            let fs_type = self.document.create_element("div")?;
            fs_type.set_class_name("volume-fs");
            fs_type.set_text_content(Some(&volume.fs_type));
            card.append_child(&fs_type)?;
        }

        let mount_point = volume.mount_point.clone();
        let click = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                let _ = app.borrow_mut().scan_path(mount_point.clone());
            });
        }) as Box<dyn FnMut(_)>);
        card.add_event_listener_with_callback("click", click.as_ref().unchecked_ref())?;
        click.forget();

        Ok(card)
    }

    pub(super) fn scan_path(&mut self, path: String) -> Result<(), JsValue> {
        self.hide_picker()?;
        self.send_message(wire::encode_scan_path(&path))
    }
}
