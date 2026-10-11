mod controls;
mod dom;
mod interaction;
mod network;
mod render;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use js_sys::{Array, ArrayBuffer, Date, Function, Reflect, Uint8Array};
use rsdirstat_protocol::{self as wire, BreadcrumbEntry, ClientMessage, LayoutPayload, ServerMessage, VolumeInfo};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::convert::FromWasmAbi;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    BinaryType, CanvasRenderingContext2d, Document, Element, Event, EventTarget, HtmlButtonElement, HtmlCanvasElement,
    HtmlElement, HtmlInputElement, HtmlSelectElement, MessageEvent, MouseEvent, Response, UrlSearchParams, WebSocket,
    Window,
};

use self::dom::{by_id, canvas_context, query, with_app};
use crate::logic::{InterpolationPlan, RenderRect, format_size};

const BACKGROUND: &str = "#1a1a2e";
const BREADCRUMB_HEIGHT: f64 = 32.0;
const TOOLBAR_HEIGHT: f64 = 28.0;
const PATH_BAR_HEIGHT: f64 = 24.0;
const ZOOM_DURATION: f64 = 300.0;
const GAP: f64 = 0.5;
const RADIUS: f64 = 3.0;
/// Rects smaller than this in either dimension are filled with a plain (square, non-anti-aliased)
/// `fillRect` instead of a rounded `fill()` path — the corner radius is invisible at small sizes
/// and the rounded-path rasterization is the dominant render cost.
const ROUND_MIN: f64 = 16.0;
const FONT: &str = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";

thread_local! {
    static APP: RefCell<Option<Rc<RefCell<TreemapApp>>>> = const { RefCell::new(None) };
}

/// A navigation zoom in progress: `ZOOM_DURATION` ms of interpolation from `start_time`.
struct ZoomAnim {
    plan: InterpolationPlan,
    start_time: f64,
}

// Closures are held only to keep them alive for the WebSocket's lifetime (RAII); they are
// invoked by JS, never read from Rust.
#[allow(dead_code)]
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
    raf_pending: bool,
}

struct ViewState {
    rects: Vec<RenderRect>,
    rect_index_by_id: HashMap<u64, usize>,
    breadcrumb: Vec<BreadcrumbEntry>,
    rendered_breadcrumb: Vec<BreadcrumbEntry>,
    view_root_size: u64,
    zoom_anim: Option<ZoomAnim>,
    pending_old_rects: Option<Vec<RenderRect>>,
    dirty: bool,
    buffer_dirty: bool,
    hovered_index: Option<usize>,
    hovered_ancestor_indices: Vec<usize>,
    /// Last pointer position over the canvas, if the pointer is on it.
    last_mouse: Option<(f64, f64)>,
    scan_done: bool,
    /// Layouts received so far. Mirrored, with the last one fully drawn and whether the scan is
    /// done, into `data-*` attributes on the canvas, which the e2e tests wait on.
    layout_seq: u64,
}

struct SessionState {
    wait_mode: bool,
    scan_start_time: f64,
    scan_timer_id: Option<i32>,
    filter_timer_id: Option<i32>,
    ws: Option<WebSocket>,
    ws_callbacks: Option<WebSocketCallbacks>,
    /// Consecutive failed reconnect attempts; drives exponential backoff and an eventual give-up.
    reconnect_attempts: u32,
}

struct TreemapApp {
    window: Window,
    document: Document,
    chrome: ChromeElements,
    surface: SurfaceState,
    view: ViewState,
    session: SessionState,
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
    let mut app = app.borrow_mut();
    app.install_event_listeners()?;
    app.resize()?;
    app.connect()?;
    app.schedule_tick()
}

impl TreemapApp {
    fn new() -> Result<Self, JsValue> {
        let window = web_sys::window().ok_or_else(|| JsValue::from_str("missing window"))?;
        let document = window.document().ok_or_else(|| JsValue::from_str("missing document"))?;

        let canvas: HtmlCanvasElement = by_id(&document, "treemap")?;
        let ctx = canvas_context(&canvas)?;
        let tooltip_element: HtmlElement = by_id(&document, "tooltip")?;
        let picker_element: HtmlElement = by_id(&document, "picker")?;
        let chrome = ChromeElements {
            breadcrumb_bar: by_id(&document, "crumbs")?,
            status_element: by_id(&document, "status")?,
            path_text_element: by_id(&document, "path-text")?,
            path_size_element: by_id(&document, "path-size")?,
            tooltip_name: query(&tooltip_element, ".tip-name")?,
            tooltip_size: query(&tooltip_element, ".tip-size")?,
            tooltip_percent: query(&tooltip_element, ".tip-percent")?,
            tooltip_mtime: query(&tooltip_element, ".tip-mtime")?,
            treemap_view: by_id(&document, "treemap-view")?,
            change_button: by_id(&document, "change")?,
            rescan_button: by_id(&document, "rescan")?,
            depth_select: by_id(&document, "depth")?,
            color_mode_select: by_id(&document, "color-mode")?,
            filter_ext_input: by_id(&document, "filter-ext")?,
            filter_name_input: by_id(&document, "filter-name")?,
            filter_min_input: by_id(&document, "filter-min")?,
            filter_min_unit_select: by_id(&document, "filter-min-unit")?,
            filter_clear_button: by_id(&document, "filter-clear")?,
            picker_grid: query(&picker_element, ".picker-grid")?,
            picker_refresh_button: query(&picker_element, ".picker-refresh")?,
            tooltip_element,
            picker_element,
        };

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
            chrome,
            surface: SurfaceState {
                canvas,
                ctx,
                buffer_canvas,
                buffer_context,
                pixel_ratio,
                canvas_width: 0.0,
                canvas_height: 0.0,
                raf_pending: false,
            },
            view: ViewState {
                rects: Vec::new(),
                rect_index_by_id: HashMap::new(),
                breadcrumb: Vec::new(),
                rendered_breadcrumb: Vec::new(),
                view_root_size: 0,
                zoom_anim: None,
                pending_old_rects: None,
                dirty: true,
                buffer_dirty: true,
                hovered_index: None,
                hovered_ancestor_indices: Vec::new(),
                last_mouse: None,
                scan_done: false,
                layout_seq: 0,
            },
            session: SessionState {
                wait_mode,
                scan_start_time: 0.0,
                scan_timer_id: None,
                filter_timer_id: None,
                ws: None,
                ws_callbacks: None,
                reconnect_attempts: 0,
            },
        })
    }

    fn now(&self) -> f64 {
        self.window
            .performance()
            .map(|performance| performance.now())
            .unwrap_or(0.0)
    }
}
