use super::*;

impl TreemapApp {
    pub(super) fn schedule_tick(&mut self) -> Result<(), JsValue> {
        if self.surface.raf_pending {
            return Ok(());
        }
        self.surface.raf_pending = true;
        let callback = Closure::once_into_js(move |_: f64| {
            with_app(|app| {
                let _ = app.borrow_mut().tick();
            });
        });
        self.window
            .request_animation_frame(callback.unchecked_ref::<Function>())?;
        Ok(())
    }

    pub(super) fn tick(&mut self) -> Result<(), JsValue> {
        self.surface.raf_pending = false;
        if self.view.dirty || self.view.zoom_anim.is_some() {
            self.render()?;
            self.view.dirty = self.view.buffer_dirty;
        }
        if self.view.zoom_anim.is_some() || self.view.dirty {
            self.schedule_tick()?;
        }
        Ok(())
    }

    pub(super) fn render(&mut self) -> Result<(), JsValue> {
        if let Some(zoom_anim) = &self.view.zoom_anim {
            self.surface.ctx.set_fill_style_str(BACKGROUND);
            self.surface
                .ctx
                .fill_rect(0.0, 0.0, self.surface.canvas_width, self.surface.canvas_height);
            let progress = ((self.now() - zoom_anim.start_time) / zoom_anim.duration).min(1.0);
            let interpolated = zoom_anim.interpolate(ease_out(progress));
            draw_rects(&self.surface.ctx, &interpolated, 1.0)?;
            if progress >= 1.0 {
                self.view.zoom_anim = None;
                self.view.buffer_dirty = true;
            } else {
                self.view.dirty = true;
            }
        } else {
            if self.view.buffer_dirty {
                self.surface.buffer_context.set_fill_style_str(BACKGROUND);
                self.surface
                    .buffer_context
                    .fill_rect(0.0, 0.0, self.surface.canvas_width, self.surface.canvas_height);
                draw_rects(&self.surface.buffer_context, &self.view.rects, 1.0)?;
                self.view.buffer_dirty = false;
            }
            self.surface.ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0)?;
            self.surface
                .ctx
                .draw_image_with_html_canvas_element(&self.surface.buffer_canvas, 0.0, 0.0)?;
            self.surface
                .ctx
                .set_transform(self.surface.pixel_ratio, 0.0, 0.0, self.surface.pixel_ratio, 0.0, 0.0)?;
            draw_hover_overlay(
                &self.surface.ctx,
                self.view.hovered_index.and_then(|index| self.view.rects.get(index)),
                &self.view.hovered_ancestor_indices,
                &self.view.rects,
            )?;
        }
        Ok(())
    }

    pub(super) fn resize(&mut self) -> Result<(), JsValue> {
        let width = self
            .window
            .inner_width()?
            .as_f64()
            .ok_or_else(|| JsValue::from_str("missing innerWidth"))?;
        let height = self
            .window
            .inner_height()?
            .as_f64()
            .ok_or_else(|| JsValue::from_str("missing innerHeight"))?
            - BREADCRUMB_HEIGHT
            - TOOLBAR_HEIGHT
            - PATH_BAR_HEIGHT;
        self.surface
            .canvas
            .style()
            .set_property("width", &format!("{width}px"))?;
        self.surface
            .canvas
            .style()
            .set_property("height", &format!("{height}px"))?;
        self.surface
            .canvas
            .set_width((width * self.surface.pixel_ratio).round() as u32);
        self.surface
            .canvas
            .set_height((height * self.surface.pixel_ratio).round() as u32);
        self.surface.canvas_width = width;
        self.surface.canvas_height = height;
        self.surface
            .ctx
            .set_transform(self.surface.pixel_ratio, 0.0, 0.0, self.surface.pixel_ratio, 0.0, 0.0)?;

        self.surface.buffer_canvas.set_width(self.surface.canvas.width());
        self.surface.buffer_canvas.set_height(self.surface.canvas.height());
        self.surface.buffer_context.set_transform(
            self.surface.pixel_ratio,
            0.0,
            0.0,
            self.surface.pixel_ratio,
            0.0,
            0.0,
        )?;

        let bounds = self.surface.canvas.get_bounding_client_rect();
        self.surface.canvas_left = bounds.left();
        self.surface.canvas_top = bounds.top();
        self.view.buffer_dirty = true;
        self.view.dirty = true;
        self.schedule_tick()?;
        self.send_viewport()
    }

    pub(super) fn handle_resize(&mut self) -> Result<(), JsValue> {
        self.surface.pixel_ratio = effective_pixel_ratio(&self.window);
        self.resize()
    }
}

fn ease_out(progress: f64) -> f64 {
    1.0 - (1.0 - progress).powi(3)
}

fn inset_rect(rect: &RenderRect) -> (f64, f64, f64, f64) {
    (
        rect.x + GAP,
        rect.y + GAP,
        (rect.w - GAP * 2.0).max(0.0),
        (rect.h - GAP * 2.0).max(0.0),
    )
}

fn truncate_label(ctx: &CanvasRenderingContext2d, label: &str, max_width: f64) -> Result<(String, f64), JsValue> {
    let mut label = label.to_string();
    let mut text_width = ctx.measure_text(&label)?.width();
    if text_width <= max_width {
        return Ok((label, text_width));
    }
    let char_count = ((label.chars().count() as f64 * (max_width - 10.0)) / text_width)
        .floor()
        .max(1.0) as usize;
    label = format!("{}…", label.chars().take(char_count).collect::<String>());
    text_width = ctx.measure_text(&label)?.width();
    Ok((label, text_width))
}

fn draw_single_rect(ctx: &CanvasRenderingContext2d, rect: &RenderRect, alpha: f64) -> Result<(), JsValue> {
    let (x, y, w, h) = inset_rect(rect);
    if w < 0.5 || h < 0.5 {
        return Ok(());
    }

    ctx.set_global_alpha(alpha);
    if w < 4.0 || h < 4.0 {
        ctx.set_fill_style_str(if rect.is_container {
            rect.color_background.as_deref().unwrap_or(&rect.color_dark)
        } else {
            &rect.color_dark
        });
        ctx.fill_rect(x, y, w, h);
        ctx.set_global_alpha(1.0);
        return Ok(());
    }

    let radius = RADIUS.min(w / 2.0).min(h / 2.0);
    if rect.is_container {
        ctx.begin_path();
        ctx.round_rect_with_f64(x, y, w, h, radius)?;
        ctx.set_fill_style_str(rect.color_background.as_deref().unwrap_or(&rect.color_dark));
        ctx.fill();

        if rect.header_height > 0.0 {
            let visible_header_height = rect.header_height - GAP;
            ctx.begin_path();
            ctx.round_rect_with_f64(x, y, w, visible_header_height, radius)?;
            ctx.set_fill_style_str(rect.color_header.as_deref().unwrap_or(&rect.color_dark));
            ctx.fill();

            let available_width = w - 8.0;
            if available_width > 20.0 && visible_header_height > 8.0 {
                let font_size = 12.0f64.min((visible_header_height - 4.0).max(8.0)).round() as i32;
                ctx.set_font(&format!("600 {font_size}px {FONT}"));
                ctx.set_fill_style_str("rgba(255,255,255,0.85)");
                ctx.set_text_baseline("middle");
                let (label, text_width) = truncate_label(ctx, &rect.name, available_width)?;
                if text_width <= available_width {
                    ctx.fill_text(&label, x + 4.0, y + visible_header_height / 2.0)?;
                    let size_label = format_size_impl(rect.size as f64);
                    if text_width + ctx.measure_text(&format!("  {size_label}"))?.width() <= available_width {
                        ctx.set_fill_style_str("rgba(255,255,255,0.45)");
                        ctx.fill_text(
                            &format!("  {size_label}"),
                            x + 4.0 + text_width,
                            y + visible_header_height / 2.0,
                        )?;
                    }
                }
            }
        }
    } else {
        ctx.begin_path();
        ctx.round_rect_with_f64(x, y, w, h, radius)?;
        ctx.set_fill_style_str(&rect.color_dark);
        ctx.fill();
        ctx.set_stroke_style_str(&rect.color_border);
        ctx.set_line_width(0.5);
        ctx.stroke();

        let available_width = w - 8.0;
        let available_height = h - 4.0;
        if available_width > 28.0 && available_height > 13.0 {
            let font_size = 14.0_f64
                .min(
                    9.0_f64.max((available_width / (rect.name.len().max(1) as f64 * 0.6)).min(available_height * 0.45)),
                )
                .round() as i32;
            ctx.set_font(&format!("600 {font_size}px {FONT}"));
            ctx.set_fill_style_str("rgba(255,255,255,0.92)");
            ctx.set_text_baseline("top");
            let (label, text_width) = truncate_label(ctx, &rect.name, available_width)?;
            if text_width <= available_width {
                ctx.fill_text(&label, x + 4.0, y + 3.0)?;
            }

            if available_height > 26.0 && rect.size > 0 {
                let small_font_size = (font_size - 2).max(8);
                ctx.set_font(&format!("{small_font_size}px {FONT}"));
                ctx.set_fill_style_str("rgba(255,255,255,0.55)");
                let size_label = format_size_impl(rect.size as f64);
                if ctx.measure_text(&size_label)?.width() <= available_width {
                    ctx.fill_text(&size_label, x + 4.0, y + 3.0 + font_size as f64 + 2.0)?;
                }
            }
        }
        ctx.set_global_alpha(1.0);
        return Ok(());
    }

    ctx.begin_path();
    ctx.round_rect_with_f64(x, y, w, h, radius)?;
    ctx.set_stroke_style_str(&rect.color_border);
    ctx.set_line_width(0.5);
    ctx.stroke();
    ctx.set_global_alpha(1.0);
    Ok(())
}

fn draw_rects(ctx: &CanvasRenderingContext2d, rects: &[RenderRect], alpha: f64) -> Result<(), JsValue> {
    for rect in rects {
        if rect.w >= 1.0 && rect.h >= 1.0 {
            draw_single_rect(ctx, rect, alpha)?;
        }
    }
    Ok(())
}

fn draw_hover_overlay(
    ctx: &CanvasRenderingContext2d,
    hovered_rect: Option<&RenderRect>,
    hovered_ancestor_indices: &[usize],
    rects: &[RenderRect],
) -> Result<(), JsValue> {
    for &index in hovered_ancestor_indices {
        let Some(rect) = rects.get(index) else {
            continue;
        };
        let (x, y, w, h) = inset_rect(rect);
        if w > 0.0 && h > 0.0 {
            let radius = RADIUS.min(w / 2.0).min(h / 2.0);
            if rect.header_height > 0.0 {
                ctx.begin_path();
                ctx.round_rect_with_f64(x, y, w, rect.header_height - GAP, radius)?;
                ctx.set_fill_style_str("rgba(255,255,255,0.05)");
                ctx.fill();
            }
            ctx.begin_path();
            ctx.round_rect_with_f64(x, y, w, h, radius)?;
            ctx.set_stroke_style_str("rgba(255,255,255,0.3)");
            ctx.set_line_width(1.0);
            ctx.stroke();
        }
    }

    if let Some(rect) = hovered_rect {
        let (x, y, w, h) = inset_rect(rect);
        if w > 0.0 && h > 0.0 {
            let radius = RADIUS.min(w / 2.0).min(h / 2.0);
            let fill_height = if rect.is_container && rect.header_height > 0.0 {
                rect.header_height - GAP
            } else {
                h
            };
            ctx.begin_path();
            ctx.round_rect_with_f64(x, y, w, fill_height, radius)?;
            ctx.set_fill_style_str("rgba(255,255,255,0.08)");
            ctx.fill();
            ctx.begin_path();
            ctx.round_rect_with_f64(x, y, w, h, radius)?;
            ctx.set_stroke_style_str("rgba(255,255,255,0.7)");
            ctx.set_line_width(1.5);
            ctx.stroke();
        }
    }
    Ok(())
}
