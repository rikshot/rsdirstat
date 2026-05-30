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
            draw_rects(&self.surface.ctx, &interpolated, 1.0, false)?;
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
                draw_rects(&self.surface.buffer_context, &self.view.rects, 1.0, true)?;
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

/// Tracks the last `fillStyle`/`font` set on the context so we can skip redundant assignments.
/// Setting either re-parses a string in the engine (a CSS colour, a font shorthand), so deduping
/// across colour-batched draws is a real saving.
#[derive(Default)]
struct DrawState {
    fill: Option<Box<str>>,
    font: Option<Box<str>>,
}

impl DrawState {
    fn fill(&mut self, ctx: &CanvasRenderingContext2d, color: &str) {
        if self.fill.as_deref() != Some(color) {
            ctx.set_fill_style_str(color);
            self.fill = Some(color.into());
        }
    }

    fn font(&mut self, ctx: &CanvasRenderingContext2d, font: &str) {
        if self.font.as_deref() != Some(font) {
            ctx.set_font(font);
            self.font = Some(font.into());
        }
    }
}

fn body_color(rect: &RenderRect) -> &str {
    if rect.is_container {
        rect.color_background.as_deref().unwrap_or(&rect.color_dark)
    } else {
        &rect.color_dark
    }
}

fn header_color(rect: &RenderRect) -> &str {
    rect.color_header.as_deref().unwrap_or(&rect.color_dark)
}

/// Fill an axis-aligned box. Above `ROUND_MIN` in both dimensions we use a rounded `fill()` path
/// (the corner radius is visible); below it a plain `fillRect`, which the rasterizer blits without
/// the anti-aliased convex-edge walking that dominates rounded-path fills.
fn fill_shape(ctx: &CanvasRenderingContext2d, x: f64, y: f64, w: f64, h: f64) -> Result<(), JsValue> {
    if w >= ROUND_MIN && h >= ROUND_MIN {
        let radius = RADIUS.min(w / 2.0).min(h / 2.0);
        ctx.begin_path();
        ctx.round_rect_with_f64(x, y, w, h, radius)?;
        ctx.fill();
    } else {
        ctx.fill_rect(x, y, w, h);
    }
    Ok(())
}

fn draw_container_label(
    ctx: &CanvasRenderingContext2d,
    state: &mut DrawState,
    rect: &RenderRect,
    x: f64,
    y: f64,
    w: f64,
) -> Result<(), JsValue> {
    if rect.header_height <= 0.0 {
        return Ok(());
    }
    let visible_header_height = rect.header_height - GAP;
    let available_width = w - 8.0;
    if available_width <= 20.0 || visible_header_height <= 8.0 {
        return Ok(());
    }
    let font_size = 12.0f64.min((visible_header_height - 4.0).max(8.0)).round() as i32;
    state.font(ctx, &format!("600 {font_size}px {FONT}"));
    state.fill(ctx, "rgba(255,255,255,0.85)");
    ctx.set_text_baseline("middle");
    let (label, text_width) = truncate_label(ctx, &rect.name, available_width)?;
    if text_width <= available_width {
        ctx.fill_text(&label, x + 4.0, y + visible_header_height / 2.0)?;
        let size_label = format_size_impl(rect.size as f64);
        if text_width + ctx.measure_text(&format!("  {size_label}"))?.width() <= available_width {
            state.fill(ctx, "rgba(255,255,255,0.45)");
            ctx.fill_text(
                &format!("  {size_label}"),
                x + 4.0 + text_width,
                y + visible_header_height / 2.0,
            )?;
        }
    }
    Ok(())
}

fn draw_leaf_label(
    ctx: &CanvasRenderingContext2d,
    state: &mut DrawState,
    rect: &RenderRect,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<(), JsValue> {
    let available_width = w - 8.0;
    let available_height = h - 4.0;
    if available_width <= 28.0 || available_height <= 13.0 {
        return Ok(());
    }
    let font_size = 14.0_f64
        .min(9.0_f64.max((available_width / (rect.name.len().max(1) as f64 * 0.6)).min(available_height * 0.45)))
        .round() as i32;
    state.font(ctx, &format!("600 {font_size}px {FONT}"));
    state.fill(ctx, "rgba(255,255,255,0.92)");
    ctx.set_text_baseline("top");
    let (label, text_width) = truncate_label(ctx, &rect.name, available_width)?;
    if text_width <= available_width {
        ctx.fill_text(&label, x + 4.0, y + 3.0)?;
    }

    if available_height > 26.0 && rect.size > 0 {
        let small_font_size = (font_size - 2).max(8);
        state.font(ctx, &format!("{small_font_size}px {FONT}"));
        state.fill(ctx, "rgba(255,255,255,0.55)");
        let size_label = format_size_impl(rect.size as f64);
        if ctx.measure_text(&size_label)?.width() <= available_width {
            ctx.fill_text(&size_label, x + 4.0, y + 3.0 + font_size as f64 + 2.0)?;
        }
    }
    Ok(())
}

/// Render all rects in colour-batched passes. Bodies are filled in depth order (a parent's
/// background is drawn before the children that sit on top of it) and grouped by colour within a
/// depth, so `fillStyle` is set roughly once per colour rather than once per rect. Per-rect borders
/// are gone — the `GAP` between rects already separates them (a hovered rect is still outlined by
/// the overlay). During the zoom animation `draw_labels` is false: text is the costliest per-rect
/// work and is illegible mid-transition, so it is skipped until the view settles.
fn draw_rects(
    ctx: &CanvasRenderingContext2d,
    rects: &[RenderRect],
    alpha: f64,
    draw_labels: bool,
) -> Result<(), JsValue> {
    if alpha != 1.0 {
        ctx.set_global_alpha(alpha);
    }
    let mut state = DrawState::default();

    let mut order: Vec<usize> = (0..rects.len())
        .filter(|&i| rects[i].w >= 1.0 && rects[i].h >= 1.0)
        .collect();
    order.sort_by(|&a, &b| {
        rects[a]
            .depth
            .cmp(&rects[b].depth)
            .then_with(|| body_color(&rects[a]).cmp(body_color(&rects[b])))
    });

    // Pass 1 — bodies.
    for &i in &order {
        let (x, y, w, h) = inset_rect(&rects[i]);
        if w < 0.5 || h < 0.5 {
            continue;
        }
        state.fill(ctx, body_color(&rects[i]));
        fill_shape(ctx, x, y, w, h)?;
    }

    // Pass 2 — container headers. Headers sit on their own body and never overlap other rects, so a
    // single colour-sorted pass is safe regardless of depth.
    let mut headers: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&i| rects[i].is_container && rects[i].header_height > 0.0)
        .collect();
    headers.sort_by(|&a, &b| header_color(&rects[a]).cmp(header_color(&rects[b])));
    for &i in &headers {
        let (x, y, w, h) = inset_rect(&rects[i]);
        if w < 4.0 || h < 4.0 {
            continue;
        }
        let header_height = rects[i].header_height - GAP;
        if header_height <= 0.0 {
            continue;
        }
        state.fill(ctx, header_color(&rects[i]));
        fill_shape(ctx, x, y, w, header_height)?;
    }

    // Pass 3 — labels.
    if draw_labels {
        for &i in &order {
            let (x, y, w, h) = inset_rect(&rects[i]);
            if w < 4.0 || h < 4.0 {
                continue;
            }
            if rects[i].is_container {
                draw_container_label(ctx, &mut state, &rects[i], x, y, w)?;
            } else {
                draw_leaf_label(ctx, &mut state, &rects[i], x, y, w, h)?;
            }
        }
    }

    if alpha != 1.0 {
        ctx.set_global_alpha(1.0);
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
