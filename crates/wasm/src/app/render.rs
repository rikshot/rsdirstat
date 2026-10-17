use super::dom::window_inner_size;
use super::*;

impl TreemapApp {
    pub(super) fn schedule_tick(&mut self) -> Result<(), JsValue> {
        if self.surface.raf_pending {
            return Ok(());
        }
        self.surface.raf_pending = true;
        self.window
            .request_animation_frame(self.callbacks.tick.as_ref().unchecked_ref())?;
        Ok(())
    }

    /// Draw again on the next frame: the hover overlay, and with `buffer` the treemap under it too.
    pub(super) fn redraw(&mut self, buffer: bool) -> Result<(), JsValue> {
        self.view.buffer_dirty |= buffer;
        self.view.dirty = true;
        self.schedule_tick()
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
            let progress = ((self.now() - zoom_anim.start_time) / ZOOM_DURATION).min(1.0);
            let interpolated = zoom_anim.plan.interpolate(ease_out(progress));
            draw_rects(&self.surface.ctx, &interpolated, false)?;
            // `tick` keeps scheduling frames while the animation is live.
            if progress >= 1.0 {
                self.view.zoom_anim = None;
                self.view.buffer_dirty = true;
            }
        } else {
            if self.view.buffer_dirty {
                self.surface.buffer_context.set_fill_style_str(BACKGROUND);
                self.surface
                    .buffer_context
                    .fill_rect(0.0, 0.0, self.surface.canvas_width, self.surface.canvas_height);
                draw_rects(&self.surface.buffer_context, &self.view.rects, true)?;
                self.view.buffer_dirty = false;
                self.surface
                    .canvas
                    .set_attribute("data-rendered-seq", &self.view.layout_seq.to_string())?;
            }
            self.surface.ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0)?;
            self.surface
                .ctx
                .draw_image_with_html_canvas_element(&self.surface.buffer_canvas, 0.0, 0.0)?;
            scale_to_device(&self.surface.ctx, self.surface.pixel_ratio)?;
            let hover = self.view.hover.as_ref();
            draw_hover_overlay(
                &self.surface.ctx,
                hover.and_then(|hover| self.view.rects.get(hover.hovered_index)),
                hover.map_or(&[], |hover| &hover.hovered_ancestor_indices),
                &self.view.rects,
            )?;
        }
        Ok(())
    }

    pub(super) fn resize(&mut self) -> Result<(), JsValue> {
        let (width, inner_height) = window_inner_size(&self.window)?;
        let height = inner_height - BREADCRUMB_HEIGHT - TOOLBAR_HEIGHT - PATH_BAR_HEIGHT;
        let surface = &mut self.surface;
        let style = surface.canvas.style();
        style.set_property("width", &format!("{width}px"))?;
        style.set_property("height", &format!("{height}px"))?;
        surface.canvas_width = width;
        surface.canvas_height = height;
        // Resizing a canvas resets its context's transform, so both are scaled again after.
        for canvas in [&surface.canvas, &surface.buffer_canvas] {
            canvas.set_width((width * surface.pixel_ratio).round() as u32);
            canvas.set_height((height * surface.pixel_ratio).round() as u32);
        }
        scale_to_device(&surface.ctx, surface.pixel_ratio)?;
        scale_to_device(&surface.buffer_context, surface.pixel_ratio)?;

        self.redraw(true)?;
        self.send_viewport()
    }

    pub(super) fn handle_resize(&mut self) -> Result<(), JsValue> {
        self.surface.pixel_ratio = effective_pixel_ratio(&self.window);
        self.resize()
    }
}

/// Scale `ctx` so it draws in CSS pixels on a canvas of `pixel_ratio` device pixels to each.
fn scale_to_device(ctx: &CanvasRenderingContext2d, pixel_ratio: f64) -> Result<(), JsValue> {
    ctx.set_transform(pixel_ratio, 0.0, 0.0, pixel_ratio, 0.0, 0.0)
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
    let text_width = ctx.measure_text(label)?.width();
    if text_width <= max_width {
        return Ok((label.to_string(), text_width));
    }
    // Linear estimate, then shrink until it actually fits. The estimate is approximate for
    // proportional fonts, so without the shrink loop the result could still exceed max_width and the
    // caller would draw nothing rather than an ellipsized label.
    let chars: Vec<char> = label.chars().collect();
    let mut count = (((chars.len() as f64) * (max_width - 10.0)) / text_width)
        .floor()
        .clamp(1.0, chars.len() as f64) as usize;
    loop {
        let candidate: String = chars[..count].iter().collect::<String>() + "…";
        let candidate_width = ctx.measure_text(&candidate)?.width();
        if candidate_width <= max_width || count == 1 {
            return Ok((candidate, candidate_width));
        }
        count -= 1;
    }
}

/// Tracks the last `fillStyle`/`font` set on the context so we can skip redundant assignments.
/// Setting either re-parses a string in the engine (a CSS colour, a font shorthand), so deduping
/// across colour-batched draws is a real saving.
#[derive(Default)]
struct DrawState {
    fill: Option<&'static str>,
    font: Option<Box<str>>,
}

impl DrawState {
    fn fill(&mut self, ctx: &CanvasRenderingContext2d, color: &'static str) {
        if self.fill != Some(color) {
            ctx.set_fill_style_str(color);
            self.fill = Some(color);
        }
    }

    fn font(&mut self, ctx: &CanvasRenderingContext2d, font: &str) {
        if self.font.as_deref() != Some(font) {
            ctx.set_font(font);
            self.font = Some(font.into());
        }
    }
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
        let size_label = format_size(rect.size);
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
        .min(
            9.0_f64
                .max((available_width / (rect.name.chars().count().max(1) as f64 * 0.6)).min(available_height * 0.45)),
        )
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
        let size_label = format_size(rect.size);
        if ctx.measure_text(&size_label)?.width() <= available_width {
            ctx.fill_text(&size_label, x + 4.0, y + 3.0 + font_size as f64 + 2.0)?;
        }
    }
    Ok(())
}

/// Render all rects in colour-batched passes. Bodies are filled in depth order (a parent's
/// background is drawn before the children that sit on top of it) and grouped by colour within a
/// depth, so `fillStyle` is set roughly once per colour rather than once per rect. Rects have no
/// borders: the `GAP` between them separates them (a hovered rect is outlined by the overlay).
/// During the zoom animation `draw_labels` is false: text is the costliest per-rect
/// work and is illegible mid-transition, so it is skipped until the view settles.
fn draw_rects(ctx: &CanvasRenderingContext2d, rects: &[RenderRect], draw_labels: bool) -> Result<(), JsValue> {
    let mut state = DrawState::default();

    let mut order: Vec<usize> = (0..rects.len())
        .filter(|&i| rects[i].w >= 1.0 && rects[i].h >= 1.0)
        .collect();
    // Colours are interned (`hsl`), so equal ones are the same string: grouping them by address
    // groups them as well as comparing their text, at a fraction of the cost.
    order.sort_unstable_by_key(|&i| (rects[i].depth, rects[i].fill.as_ptr()));

    // Pass 1 — bodies.
    for &i in &order {
        let (x, y, w, h) = inset_rect(&rects[i]);
        if w < 0.5 || h < 0.5 {
            continue;
        }
        state.fill(ctx, rects[i].fill);
        fill_shape(ctx, x, y, w, h)?;
    }

    // Pass 2 — container headers. Headers sit on their own body and never overlap other rects, so a
    // single colour-sorted pass is safe regardless of depth.
    let mut headers: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&i| rects[i].header_height > 0.0)
        .collect();
    headers.sort_unstable_by_key(|&i| rects[i].header_fill.as_ptr());
    for &i in &headers {
        let (x, y, w, h) = inset_rect(&rects[i]);
        if w < 4.0 || h < 4.0 {
            continue;
        }
        let header_height = rects[i].header_height - GAP;
        if header_height <= 0.0 {
            continue;
        }
        state.fill(ctx, rects[i].header_fill);
        fill_shape(ctx, x, y, w, header_height)?;
    }

    // Pass 3 — labels.
    if draw_labels {
        for &i in &order {
            let (x, y, w, h) = inset_rect(&rects[i]);
            if w < 4.0 || h < 4.0 {
                continue;
            }
            if rects[i].kind.is_container() {
                draw_container_label(ctx, &mut state, &rects[i], x, y, w)?;
            } else {
                draw_leaf_label(ctx, &mut state, &rects[i], x, y, w, h)?;
            }
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
            let fill_height = if rect.header_height > 0.0 {
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
