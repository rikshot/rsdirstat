mod controls;
mod dom;
mod interaction;
mod network;
mod render;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Deref;
use std::rc::Rc;

use js_sys::{Array, ArrayBuffer, Date, Function, Reflect, Uint8Array};
use rsdirstat_protocol::{self as wire, BreadcrumbEntry, LayoutPayload, LayoutRect, ServerMessage};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    BinaryType, CanvasRenderingContext2d, Document, Element, Event, HtmlButtonElement, HtmlCanvasElement, HtmlElement,
    HtmlInputElement, HtmlSelectElement, MessageEvent, MouseEvent, Response, UrlSearchParams, WebSocket, Window,
};

use self::dom::{by_id, canvas_context, query, with_app};
use crate::logic::{HoverState, RectLike, TreemapRect};
use crate::{format_size_impl, hsl_impl};

const BACKGROUND: &str = "#1a1a2e";
const BREADCRUMB_HEIGHT: f64 = 32.0;
const TOOLBAR_HEIGHT: f64 = 28.0;
const PATH_BAR_HEIGHT: f64 = 24.0;
const ZOOM_DURATION: f64 = 300.0;
const GAP: f64 = 0.5;
const RADIUS: f64 = 3.0;
const FONT: &str = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";

thread_local! {
    static APP: RefCell<Option<Rc<RefCell<TreemapApp>>>> = const { RefCell::new(None) };
}

#[derive(Clone)]
struct RenderRect {
    rect: TreemapRect,
    color_dark: String,
    color_border: String,
    color_background: Option<String>,
    color_header: Option<String>,
}

impl RenderRect {
    fn from_wire(rect: LayoutRect) -> Self {
        let color_dark = hsl_impl(rect.hue, 62, 38);
        let color_border = hsl_impl(rect.hue, 60, 28);
        let color_background = rect.is_container.then(|| hsl_impl(rect.hue, 25, 13));
        let color_header = rect.is_container.then(|| hsl_impl(rect.hue, 35, 20));
        let rect = TreemapRect {
            id: rect.id,
            parent_id: rect.parent_id,
            x: rect.x as f64,
            y: rect.y as f64,
            w: rect.w as f64,
            h: rect.h as f64,
            name: rect.name,
            size: rect.size,
            is_container: rect.is_container,
            header_height: rect.header_height as f64,
            is_files: rect.is_files,
            is_file: rect.is_file,
            mtime: rect.mtime,
        };
        Self {
            rect,
            color_dark,
            color_border,
            color_background,
            color_header,
        }
    }

    fn with_geometry(&self, rect: TreemapRect) -> Self {
        Self {
            rect,
            color_dark: self.color_dark.clone(),
            color_border: self.color_border.clone(),
            color_background: self.color_background.clone(),
            color_header: self.color_header.clone(),
        }
    }
}

impl Deref for RenderRect {
    type Target = TreemapRect;

    fn deref(&self) -> &Self::Target {
        &self.rect
    }
}

impl RectLike for RenderRect {
    fn id(&self) -> i64 {
        self.rect.id
    }

    fn parent_id(&self) -> u64 {
        self.rect.parent_id
    }

    fn x(&self) -> f64 {
        self.rect.x
    }

    fn y(&self) -> f64 {
        self.rect.y
    }

    fn w(&self) -> f64 {
        self.rect.w
    }

    fn h(&self) -> f64 {
        self.rect.h
    }

    fn name(&self) -> &str {
        &self.rect.name
    }

    fn size(&self) -> u64 {
        self.rect.size
    }

    fn is_container(&self) -> bool {
        self.rect.is_container
    }

    fn header_height(&self) -> f64 {
        self.rect.header_height
    }

    fn is_files(&self) -> bool {
        self.rect.is_files
    }

    fn is_file(&self) -> bool {
        self.rect.is_file
    }

    fn mtime(&self) -> i64 {
        self.rect.mtime
    }
}

struct ZoomAnim {
    plan: crate::logic::InterpolationPlan,
    templates: HashMap<i64, RenderRect>,
    start_time: f64,
    duration: f64,
}

impl ZoomAnim {
    fn new(from: Vec<RenderRect>, to: &[RenderRect], start_time: f64) -> Self {
        let plan = crate::logic::InterpolationPlan::new(&from, to);
        let mut templates = HashMap::with_capacity(from.len() + to.len());
        for rect in from {
            templates.insert(rect.id, rect);
        }
        for rect in to {
            templates.insert(rect.id, rect.clone());
        }
        Self {
            plan,
            templates,
            start_time,
            duration: ZOOM_DURATION,
        }
    }

    fn interpolate(&self, progress: f64) -> Vec<RenderRect> {
        self.plan
            .interpolate(progress)
            .into_iter()
            .filter_map(|rect| {
                self.templates
                    .get(&rect.id)
                    .map(|template| template.with_geometry(rect))
            })
            .collect()
    }
}

struct WebSocketCallbacks {
    onopen: Closure<dyn FnMut(Event)>,
    onmessage: Closure<dyn FnMut(MessageEvent)>,
    onclose: Closure<dyn FnMut(Event)>,
    onerror: Closure<dyn FnMut(Event)>,
}

struct ChromeElements {
    breadcrumb_bar: HtmlElement,
    tooltip_element: HtmlElement,
    status_element: HtmlElement,
    path_text_element: HtmlElement,
    path_size_element: HtmlElement,
    tooltip_name: HtmlElement,
    tooltip_size: HtmlElement,
    tooltip_percent: HtmlElement,
    tooltip_mtime: HtmlElement,
    treemap_view: HtmlElement,
    picker_element: HtmlElement,
    change_button: HtmlButtonElement,
    rescan_button: HtmlButtonElement,
    depth_select: HtmlSelectElement,
    color_mode_select: HtmlSelectElement,
    filter_ext_input: HtmlInputElement,
    filter_name_input: HtmlInputElement,
    filter_min_input: HtmlInputElement,
    filter_min_unit_select: HtmlSelectElement,
    filter_clear_button: HtmlButtonElement,
    picker_grid: HtmlElement,
    picker_refresh_button: HtmlButtonElement,
}

struct SurfaceState {
    canvas: HtmlCanvasElement,
    ctx: CanvasRenderingContext2d,
    buffer_canvas: HtmlCanvasElement,
    buffer_context: CanvasRenderingContext2d,
    pixel_ratio: f64,
    canvas_width: f64,
    canvas_height: f64,
    canvas_left: f64,
    canvas_top: f64,
    raf_pending: bool,
}

struct ViewState {
    rects: Vec<RenderRect>,
    rect_index_by_id: HashMap<u64, usize>,
    breadcrumb: Vec<BreadcrumbEntry>,
    breadcrumb_parts: Vec<String>,
    rendered_breadcrumb: Vec<BreadcrumbEntry>,
    view_root_size: u64,
    zoom_anim: Option<ZoomAnim>,
    pending_old_rects: Option<Vec<RenderRect>>,
    dirty: bool,
    buffer_dirty: bool,
    hovered_index: Option<usize>,
    hovered_ancestor_indices: Vec<usize>,
    last_mouse_x: f64,
    last_mouse_y: f64,
    scan_done: bool,
}

struct SessionState {
    wait_mode: bool,
    scan_start_time: f64,
    scan_timer_id: Option<i32>,
    filter_timer_id: Option<i32>,
    ws: Option<WebSocket>,
    ws_callbacks: Option<WebSocketCallbacks>,
}

struct TreemapApp {
    window: Window,
    document: Document,
    chrome: ChromeElements,
    surface: SurfaceState,
    view: ViewState,
    session: SessionState,
}

#[derive(Clone)]
struct VolumeEntry {
    name: String,
    mount_point: String,
    total_bytes: u64,
    used_bytes: u64,
    fs_type: String,
}

/// `devicePixelRatio` can report 0 in some headless/embedded contexts; fall back to 1 so the
/// canvas backing store is never sized to zero.
fn effective_pixel_ratio(window: &Window) -> f64 {
    let ratio = window.device_pixel_ratio();
    if ratio > 0.0 { ratio } else { 1.0 }
}

pub fn start_browser_app() -> Result<(), JsValue> {
    let app = Rc::new(RefCell::new(TreemapApp::new()?));
    APP.with(|slot| *slot.borrow_mut() = Some(Rc::clone(&app)));
    TreemapApp::install_event_listeners()?;
    {
        let mut app = app.borrow_mut();
        app.resize()?;
        app.connect()?;
        app.schedule_tick()?;
    }
    Ok(())
}

impl TreemapApp {
    fn new() -> Result<Self, JsValue> {
        let window = web_sys::window().ok_or_else(|| JsValue::from_str("missing window"))?;
        let document = window.document().ok_or_else(|| JsValue::from_str("missing document"))?;

        let canvas: HtmlCanvasElement = by_id(&document, "treemap")?;
        let ctx = canvas_context(&canvas)?;
        let breadcrumb_bar: HtmlElement = by_id(&document, "crumbs")?;
        let tooltip_element: HtmlElement = by_id(&document, "tooltip")?;
        let status_element: HtmlElement = by_id(&document, "status")?;
        let path_text_element: HtmlElement = by_id(&document, "path-text")?;
        let path_size_element: HtmlElement = by_id(&document, "path-size")?;
        let tooltip_name: HtmlElement = query(&tooltip_element, ".tip-name")?;
        let tooltip_size: HtmlElement = query(&tooltip_element, ".tip-size")?;
        let tooltip_percent: HtmlElement = query(&tooltip_element, ".tip-percent")?;
        let tooltip_mtime: HtmlElement = query(&tooltip_element, ".tip-mtime")?;
        let treemap_view: HtmlElement = by_id(&document, "treemap-view")?;
        let picker_element: HtmlElement = by_id(&document, "picker")?;
        let change_button: HtmlButtonElement = by_id(&document, "change")?;
        let rescan_button: HtmlButtonElement = by_id(&document, "rescan")?;
        let depth_select: HtmlSelectElement = by_id(&document, "depth")?;
        let color_mode_select: HtmlSelectElement = by_id(&document, "color-mode")?;
        let filter_ext_input: HtmlInputElement = by_id(&document, "filter-ext")?;
        let filter_name_input: HtmlInputElement = by_id(&document, "filter-name")?;
        let filter_min_input: HtmlInputElement = by_id(&document, "filter-min")?;
        let filter_min_unit_select: HtmlSelectElement = by_id(&document, "filter-min-unit")?;
        let filter_clear_button: HtmlButtonElement = by_id(&document, "filter-clear")?;
        let picker_grid: HtmlElement = query(&picker_element, ".picker-grid")?;
        let picker_refresh_button: HtmlButtonElement = query(&picker_element, ".picker-refresh")?;

        let buffer_canvas = document
            .create_element("canvas")?
            .dyn_into::<HtmlCanvasElement>()
            .map_err(|_| JsValue::from_str("failed to create buffer canvas"))?;
        let buffer_context = canvas_context(&buffer_canvas)?;

        let search = window.location().search().unwrap_or_default();
        let wait_mode = UrlSearchParams::new_with_str(&search)
            .map(|params| params.has("wait"))
            .unwrap_or(false);

        let pixel_ratio = effective_pixel_ratio(&window);

        Ok(Self {
            window,
            document,
            chrome: ChromeElements {
                breadcrumb_bar,
                tooltip_element,
                status_element,
                path_text_element,
                path_size_element,
                tooltip_name,
                tooltip_size,
                tooltip_percent,
                tooltip_mtime,
                treemap_view,
                picker_element,
                change_button,
                rescan_button,
                depth_select,
                color_mode_select,
                filter_ext_input,
                filter_name_input,
                filter_min_input,
                filter_min_unit_select,
                filter_clear_button,
                picker_grid,
                picker_refresh_button,
            },
            surface: SurfaceState {
                canvas,
                ctx,
                buffer_canvas,
                buffer_context,
                pixel_ratio,
                canvas_width: 0.0,
                canvas_height: 0.0,
                canvas_left: 0.0,
                canvas_top: 0.0,
                raf_pending: false,
            },
            view: ViewState {
                rects: Vec::new(),
                rect_index_by_id: HashMap::new(),
                breadcrumb: Vec::new(),
                breadcrumb_parts: Vec::new(),
                rendered_breadcrumb: Vec::new(),
                view_root_size: 0,
                zoom_anim: None,
                pending_old_rects: None,
                dirty: true,
                buffer_dirty: true,
                hovered_index: None,
                hovered_ancestor_indices: Vec::new(),
                last_mouse_x: -1.0,
                last_mouse_y: -1.0,
                scan_done: false,
            },
            session: SessionState {
                wait_mode,
                scan_start_time: 0.0,
                scan_timer_id: None,
                filter_timer_id: None,
                ws: None,
                ws_callbacks: None,
            },
        })
    }

    fn now(&self) -> f64 {
        self.window
            .performance()
            .map(|performance| performance.now())
            .unwrap_or(0.0)
    }

    fn rebuild_view_caches(&mut self) {
        self.view.rect_index_by_id = crate::logic::build_rect_index(&self.view.rects);
        self.view.breadcrumb_parts = crate::logic::build_breadcrumb_parts(&self.view.breadcrumb);
    }
}
