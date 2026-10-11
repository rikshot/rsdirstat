use super::dom::{is_hidden, listen, set_hidden};
use super::*;
use crate::logic::scan_summary;

impl TreemapApp {
    pub(super) fn send_message(&self, message: ClientMessage) -> Result<(), JsValue> {
        if let Some(ws) = &self.session.ws
            && ws.ready_state() == WebSocket::OPEN
        {
            ws.send_with_u8_array(&message.encode())?;
        }
        Ok(())
    }

    pub(super) fn send_viewport(&self) -> Result<(), JsValue> {
        if self.surface.canvas_width <= 0.0 {
            return Ok(());
        }
        self.send_message(ClientMessage::Viewport {
            width: self.surface.canvas_width as f32,
            height: self.surface.canvas_height as f32,
        })
    }

    pub(super) fn start_scan_timer(&mut self) -> Result<(), JsValue> {
        if self.session.scan_timer_id.is_some() {
            return Ok(());
        }
        self.session.scan_start_time = self.now();
        let id = self.window.set_interval_with_callback_and_timeout_and_arguments_0(
            self.callbacks.scan_timer.as_ref().unchecked_ref(),
            100,
        )?;
        self.session.scan_timer_id = Some(id);
        Ok(())
    }

    pub(super) fn show_scan_time(&mut self) -> Result<(), JsValue> {
        let seconds = (self.now() - self.session.scan_start_time) / 1000.0;
        self.chrome
            .status_element
            .set_text_content(Some(&format!("Scanning... {seconds:.1}s")));
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

        let callbacks = &self.callbacks;
        ws.set_onopen(Some(callbacks.ws_open.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(callbacks.ws_message.as_ref().unchecked_ref()));
        ws.set_onclose(Some(callbacks.ws_close.as_ref().unchecked_ref()));
        ws.set_onerror(Some(callbacks.ws_error.as_ref().unchecked_ref()));
        self.session.ws = Some(ws);
        Ok(())
    }

    pub(super) fn handle_ws_open(&mut self) -> Result<(), JsValue> {
        self.session.reconnect_attempts = 0;
        if self.session.wait_mode {
            // The Start button lives in the status bar, inside the treemap view — which otherwise
            // stays hidden until the first layout, and none comes before the scan is started.
            set_hidden(&self.chrome.treemap_view, false)?;
            self.chrome.status_element.set_text_content(Some(""));
            let button = self.document.create_element("button")?;
            button.set_text_content(Some("Start Scan"));
            button.set_class_name("action-button");
            listen(&button, "click", |app, _: Event| app.handle_manual_start())?;
            self.chrome.status_element.append_child(&button)?;
        } else {
            self.chrome
                .status_element
                .set_text_content(Some("Connected. Waiting for scan..."));
        }
        // Each connection starts from the server's default view, but the toolbar may hold other
        // values: ones the browser restored on a reload, or ones set before a reconnect. Send them,
        // before the viewport that triggers the first layout, so the treemap matches the toolbar.
        self.handle_depth_change()?;
        self.handle_color_mode_change()?;
        self.send_filter_now()?;
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
            Some(ServerMessage::ScanStart) => {
                self.hide_picker()?;
                self.view.rects.clear();
                self.view.rect_index_by_id.clear();
                self.view.breadcrumb.clear();
                self.view.view_root_size = 0;
                self.view.zoom_anim = None;
                self.view.pending_old_rects = None;
                self.view.hover = None;
                self.redraw(true)?;
                self.stop_scan_timer();
                self.start_scan_timer()?;
                self.build_breadcrumb()?;
                set_hidden(&self.chrome.change_button, true)?;
                self.view.scan_done = false;
                self.surface.canvas.set_attribute("data-scan-done", "false")?;
            }
            Some(ServerMessage::Layout(payload)) => {
                // The first layout reveals the treemap, but one that arrives while the picker is up
                // (after "Change", the server still has the finished scan) must stay behind it.
                // Picking a path hides the picker and shows the treemap.
                if is_hidden(&self.chrome.picker_element) {
                    set_hidden(&self.chrome.treemap_view, false)?;
                }
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
        self.window.set_timeout_with_callback_and_timeout_and_arguments_0(
            self.callbacks.reconnect.as_ref().unchecked_ref(),
            delay_ms as i32,
        )?;
        Ok(())
    }

    pub(super) fn handle_layout(&mut self, payload: LayoutPayload) -> Result<(), JsValue> {
        let rects = payload.rects.into_iter().map(RenderRect::from_wire).collect::<Vec<_>>();
        if let Some(from) = self.view.pending_old_rects.take() {
            self.view.zoom_anim = Some(ZoomAnim {
                plan: InterpolationPlan::new(&from, &rects),
                start_time: self.now(),
            });
        }
        let root_size = payload.root_size;
        let dir_count = payload.dir_count;
        let new_scan_done = payload.scan_done;
        self.view.rect_index_by_id = crate::logic::build_rect_index(&rects);
        self.view.rects = rects;
        self.view.view_root_size = root_size;
        self.view.breadcrumb = payload.breadcrumb;
        self.view.separator = payload.separator;
        self.build_breadcrumb()?;
        self.recompute_hover();
        self.redraw(true)?;

        set_hidden(&self.chrome.rescan_button, !new_scan_done)?;
        set_hidden(&self.chrome.change_button, !new_scan_done)?;
        if !new_scan_done {
            self.start_scan_timer()?;
            // The server's clock, so a tab that joined mid-scan counts from the scan's start.
            self.session.scan_start_time = self.now() - f64::from(payload.scan_ms);
        } else {
            self.stop_scan_timer();
            let seconds = f64::from(payload.scan_ms) / 1000.0;
            self.chrome.status_element.set_text_content(Some(&scan_summary(
                dir_count,
                seconds,
                root_size,
                payload.partial,
            )));
        }
        self.view.scan_done = new_scan_done;

        self.view.layout_seq += 1;
        let canvas = &self.surface.canvas;
        canvas.set_attribute("data-layout-seq", &self.view.layout_seq.to_string())?;
        canvas.set_attribute("data-scan-done", if new_scan_done { "true" } else { "false" })?;
        Ok(())
    }
}

pub(super) async fn fetch_volumes() -> Result<Vec<VolumeInfo>, JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("missing window"))?;
    let response = JsFuture::from(window.fetch_with_str("/volumes")).await?;
    let response = response
        .dyn_into::<Response>()
        .map_err(|_| JsValue::from_str("fetch did not return Response"))?;
    let body = JsFuture::from(response.array_buffer()?).await?;
    VolumeInfo::decode_list(&Uint8Array::new(&body).to_vec()).ok_or_else(|| JsValue::from_str("unreadable /volumes"))
}
