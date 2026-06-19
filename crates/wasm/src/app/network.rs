use super::dom::{add_class, remove_class, set_hidden, with_app};
use super::*;

impl TreemapApp {
    pub(super) fn send_message(&self, bytes: Vec<u8>) -> Result<(), JsValue> {
        if let Some(ws) = &self.session.ws
            && ws.ready_state() == WebSocket::OPEN
        {
            ws.send_with_u8_array(&bytes)?;
        }
        Ok(())
    }

    pub(super) fn send_viewport(&self) -> Result<(), JsValue> {
        if self.surface.canvas_width <= 0.0 {
            return Ok(());
        }
        self.send_message(
            wire::ClientMessage::Viewport {
                width: self.surface.canvas_width as f32,
                height: self.surface.canvas_height as f32,
            }
            .encode(),
        )
    }

    pub(super) fn start_scan_timer(&mut self) -> Result<(), JsValue> {
        if self.session.scan_timer_id.is_some() {
            return Ok(());
        }
        self.session.scan_start_time = self.now();
        let closure = Closure::wrap(Box::new(move || {
            with_app(|app| {
                let app = app.borrow_mut();
                let seconds = (app.now() - app.session.scan_start_time) / 1000.0;
                app.chrome
                    .status_element
                    .set_text_content(Some(&format!("Scanning... {seconds:.1}s")));
            });
        }) as Box<dyn FnMut()>);
        let id = self
            .window
            .set_interval_with_callback_and_timeout_and_arguments_0(closure.as_ref().unchecked_ref(), 100)?;
        closure.forget();
        self.session.scan_timer_id = Some(id);
        Ok(())
    }

    pub(super) fn stop_scan_timer(&mut self) {
        if let Some(id) = self.session.scan_timer_id.take() {
            self.window.clear_interval_with_handle(id);
        }
    }

    pub(super) fn connect(&mut self) -> Result<(), JsValue> {
        let protocol = if self.window.location().protocol()? == "https:" {
            "wss:"
        } else {
            "ws:"
        };
        let url = format!("{protocol}//{}/ws", self.window.location().host()?);
        self.chrome.status_element.set_text_content(Some("Connecting..."));

        let ws = WebSocket::new(&url)?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let onopen = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                let _ = app.borrow_mut().handle_ws_open();
            });
        }) as Box<dyn FnMut(_)>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));

        let onmessage = Closure::wrap(Box::new(move |event: MessageEvent| {
            with_app(|app| {
                let _ = app.borrow_mut().handle_ws_message(event);
            });
        }) as Box<dyn FnMut(_)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        let onclose = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                let _ = app.borrow_mut().handle_ws_close();
            });
        }) as Box<dyn FnMut(_)>);
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));

        let onerror = Closure::wrap(Box::new(move |_event: Event| {
            with_app(|app| {
                app.borrow()
                    .chrome
                    .status_element
                    .set_text_content(Some("Connection error."));
            });
        }) as Box<dyn FnMut(_)>);
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        self.session.ws = Some(ws);
        self.session.ws_callbacks = Some(WebSocketCallbacks {
            onopen,
            onmessage,
            onclose,
            onerror,
        });
        Ok(())
    }

    pub(super) fn handle_ws_open(&mut self) -> Result<(), JsValue> {
        self.session.reconnect_attempts = 0;
        if self.session.wait_mode {
            self.chrome.status_element.set_text_content(Some(""));
            let button = self
                .document
                .create_element("button")?
                .dyn_into::<HtmlButtonElement>()
                .map_err(|_| JsValue::from_str("start button is not HtmlButtonElement"))?;
            button.set_text_content(Some("Start Scan"));
            button.set_class_name("action-button");
            let click = Closure::wrap(Box::new(move |_event: Event| {
                with_app(|app| {
                    let _ = app.borrow_mut().handle_manual_start();
                });
            }) as Box<dyn FnMut(_)>);
            button.add_event_listener_with_callback("click", click.as_ref().unchecked_ref())?;
            click.forget();
            self.chrome.status_element.append_child(&button)?;
        } else {
            self.chrome
                .status_element
                .set_text_content(Some("Connected. Waiting for scan..."));
        }
        self.send_viewport()
    }

    pub(super) fn handle_manual_start(&mut self) -> Result<(), JsValue> {
        if let Some(button) = self.chrome.status_element.query_selector("button")? {
            let button = button
                .dyn_into::<HtmlButtonElement>()
                .map_err(|_| JsValue::from_str("status button is not HtmlButtonElement"))?;
            button.set_disabled(true);
            button.set_text_content(Some("Starting…"));
        }
        let _ = self.window.fetch_with_str("/start");
        self.session.wait_mode = false;
        Ok(())
    }

    pub(super) fn handle_ws_message(&mut self, event: MessageEvent) -> Result<(), JsValue> {
        let Ok(buffer) = event.data().dyn_into::<ArrayBuffer>() else {
            return Ok(());
        };
        let bytes = Uint8Array::new(&buffer).to_vec();
        match wire::ServerMessage::decode(&bytes) {
            Some(ServerMessage::PickerMode) => {
                self.show_picker()?;
            }
            Some(ServerMessage::ScanStart { .. }) => {
                self.hide_picker()?;
                self.view.rects.clear();
                self.view.rect_index_by_id.clear();
                self.view.breadcrumb.clear();
                self.view.breadcrumb_parts.clear();
                self.view.view_root_size = 0;
                self.view.zoom_anim = None;
                self.view.pending_old_rects = None;
                self.view.hovered_index = None;
                self.view.hovered_ancestor_indices.clear();
                self.view.buffer_dirty = true;
                self.view.dirty = true;
                self.schedule_tick()?;
                self.stop_scan_timer();
                self.start_scan_timer()?;
                self.build_breadcrumb()?;
                add_class(&self.chrome.change_button, "hidden")?;
            }
            Some(ServerMessage::Layout(payload)) => {
                remove_class(&self.chrome.treemap_view, "hidden")?;
                self.handle_layout(payload)?;
            }
            None => {}
        }
        Ok(())
    }

    pub(super) fn handle_ws_close(&mut self) -> Result<(), JsValue> {
        // Stop the scan timer first: otherwise its 100ms interval keeps overwriting the status with
        // "Scanning... Ns" right after we set the disconnected message, and the two fight forever.
        self.stop_scan_timer();
        self.session.ws = None;
        self.session.ws_callbacks = None;

        // Give up after a bounded number of attempts so a permanently-gone server (it self-exits a
        // few seconds after the last client leaves) doesn't leave the tab reconnecting forever.
        const MAX_ATTEMPTS: u32 = 10;
        if self.session.reconnect_attempts >= MAX_ATTEMPTS {
            self.chrome
                .status_element
                .set_text_content(Some("Disconnected — server unavailable. Reload to retry."));
            return Ok(());
        }
        self.session.reconnect_attempts += 1;

        // Exponential backoff capped at 30s (1, 2, 4, 8, 16, 30, 30, ...) instead of hammering a
        // dead port every 3s.
        let delay_ms = (1000u32 << (self.session.reconnect_attempts - 1).min(5)).min(30_000);
        self.chrome
            .status_element
            .set_text_content(Some(&format!("Disconnected. Reconnecting in {}s...", delay_ms / 1000)));
        let callback = Closure::once_into_js(move || {
            with_app(|app| {
                let _ = app.borrow_mut().connect();
            });
        });
        self.window.set_timeout_with_callback_and_timeout_and_arguments_0(
            callback.unchecked_ref::<Function>(),
            delay_ms as i32,
        )?;
        Ok(())
    }

    pub(super) fn handle_layout(&mut self, payload: LayoutPayload) -> Result<(), JsValue> {
        let rects = payload.rects.into_iter().map(RenderRect::from_wire).collect::<Vec<_>>();
        if let Some(from) = self.view.pending_old_rects.take() {
            self.view.zoom_anim = Some(ZoomAnim::new(from, &rects, self.now()));
            self.view.dirty = true;
            self.schedule_tick()?;
        }
        let root_size = payload.root_size;
        let dir_count = payload.dir_count;
        let new_scan_done = payload.scan_done;
        self.view.rects = rects;
        self.view.view_root_size = root_size;
        self.view.breadcrumb = payload.breadcrumb;
        self.rebuild_view_caches();
        self.build_breadcrumb()?;
        self.recompute_hover()?;
        self.view.buffer_dirty = true;
        self.view.dirty = true;
        self.schedule_tick()?;

        set_hidden(&self.chrome.rescan_button, !new_scan_done)?;
        set_hidden(&self.chrome.change_button, !new_scan_done)?;
        if !new_scan_done {
            self.start_scan_timer()?;
        } else if !self.view.scan_done {
            self.stop_scan_timer();
            let elapsed = (self.now() - self.session.scan_start_time) / 1000.0;
            self.chrome.status_element.set_text_content(Some(&format!(
                "{dir_count} dirs in {elapsed:.1}s — {}",
                format_size_impl(root_size as f64)
            )));
        } else {
            self.chrome.status_element.set_text_content(Some(&format!(
                "{dir_count} dirs — {}",
                format_size_impl(root_size as f64)
            )));
        }
        self.view.scan_done = new_scan_done;
        Ok(())
    }
}

pub(super) async fn fetch_volumes() -> Result<Vec<VolumeEntry>, JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("missing window"))?;
    let response = JsFuture::from(window.fetch_with_str("/volumes")).await?;
    let response = response
        .dyn_into::<Response>()
        .map_err(|_| JsValue::from_str("fetch did not return Response"))?;
    let json = JsFuture::from(response.json()?).await?;
    let values = Array::from(&json);
    let mut volumes = Vec::with_capacity(values.length() as usize);
    for value in values.iter() {
        let name = field_string(&value, "name")?;
        let mount_point = field_string(&value, "mountPoint")?;
        let total_bytes = field_number(&value, "totalBytes")? as u64;
        let used_bytes = field_number(&value, "usedBytes")? as u64;
        let fs_type = field_string(&value, "fsType").unwrap_or_default();
        volumes.push(VolumeEntry {
            name,
            mount_point,
            total_bytes,
            used_bytes,
            fs_type,
        });
    }
    Ok(volumes)
}

fn field_string(value: &JsValue, key: &str) -> Result<String, JsValue> {
    Reflect::get(value, &JsValue::from_str(key))?
        .as_string()
        .ok_or_else(|| JsValue::from_str(&format!("expected string field '{key}'")))
}

fn field_number(value: &JsValue, key: &str) -> Result<f64, JsValue> {
    Reflect::get(value, &JsValue::from_str(key))?
        .as_f64()
        .ok_or_else(|| JsValue::from_str(&format!("expected numeric field '{key}'")))
}
