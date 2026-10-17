mod controls;
mod dom;
mod interaction;
mod network;
mod render;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use js_sys::{ArrayBuffer, Date, Uint8Array};
use rsdirstat_protocol::{self as wire, BreadcrumbEntry, ClientMessage, LayoutPayload, ServerMessage, VolumeInfo};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::convert::FromWasmAbi;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    BinaryType, CanvasRenderingContext2d, Document, Element, Event, EventTarget, HtmlButtonElement, HtmlCanvasElement,
    HtmlElement, HtmlInputElement, HtmlSelectElement, MessageEvent, MouseEvent, Response, WebSocket, Window,
};

use self::dom::{app_callback, by_id, canvas_context, query};
use crate::logic::{HoverState, InterpolationPlan, RenderRect, format_size, percent};
use crate::style::{BACKGROUND, BREADCRUMB_HEIGHT, FONT, PATH_BAR_HEIGHT, TOOLBAR_HEIGHT};

const ZOOM_DURATION: f64 = 300.0;
const GAP: f64 = 0.5;
const RADIUS: f64 = 3.0;
/// Rects smaller than this in either dimension are filled with a plain (square, non-anti-aliased)
/// `fillRect` instead of a rounded `fill()` path — the corner radius is invisible at small sizes
/// and the rounded-path rasterization is the dominant render cost.
const ROUND_MIN: f64 = 16.0;

thread_local! {
    static APP: RefCell<Option<Rc<RefCell<TreemapApp>>>> = const { RefCell::new(None) };
}

/// A navigation zoom in progress: `ZOOM_DURATION` ms of interpolation from `start_time`.
struct ZoomAnim {
    plan: InterpolationPlan,
    start_time: f64,
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
    /// The server's path separator, for the hovered rect's path.
    separator: char,
    view_root_size: u64,
    zoom_anim: Option<ZoomAnim>,
    pending_old_rects: Option<Vec<RenderRect>>,
    dirty: bool,
    buffer_dirty: bool,
    /// The rect under the pointer, with its open ancestors and its path.
    hover: Option<HoverState>,
    /// The tooltip's size, measured when its content last changed: measuring on every pointer move
    /// would make the page lay out on each one.
    tooltip_size: (f64, f64),
    /// Last pointer position over the canvas, if the pointer is on it.
    last_mouse: Option<(f64, f64)>,
    scan_done: bool,
    /// Layouts received so far. Mirrored, with the last one fully drawn and whether the scan is
    /// done, into `data-*` attributes on the canvas, which the e2e tests wait on.
    layout_seq: u64,
}

struct SessionState {
    scan_start_time: f64,
    scan_timer_id: Option<i32>,
    filter_timer_id: Option<i32>,
    ws: Option<WebSocket>,
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
    callbacks: Callbacks,
}

/// Every callback the app hands to the browser, made once (see `app_callback`): one closure serves
/// every call, so none is allocated per frame or per connection, and none is leaked by a timer
/// cleared before it fires. Timer callbacks take the (undefined) argument the browser passes none
/// of.
struct Callbacks {
    /// One animation frame, for `requestAnimationFrame`.
    tick: Closure<dyn FnMut(f64)>,
    /// The "Scanning... Ns" status, for `setInterval` while a scan runs.
    scan_timer: Closure<dyn FnMut(JsValue)>,
    /// The debounced filter send, for `setTimeout`.
    filter_send: Closure<dyn FnMut(JsValue)>,
    /// A reconnection attempt, for `setTimeout`.
    reconnect: Closure<dyn FnMut(JsValue)>,
    ws_open: Closure<dyn FnMut(Event)>,
    ws_message: Closure<dyn FnMut(MessageEvent)>,
    ws_close: Closure<dyn FnMut(Event)>,
    ws_error: Closure<dyn FnMut(Event)>,
    /// A change of `devicePixelRatio`, for a `matchMedia` listener (see `watch_pixel_ratio`).
    pixel_ratio: Closure<dyn FnMut(Event)>,
}

impl Callbacks {
    fn new() -> Self {
        Self {
            tick: app_callback(|app, _: f64| app.tick()),
            scan_timer: app_callback(|app, _: JsValue| app.show_scan_time()),
            filter_send: app_callback(|app, _: JsValue| app.send_filter_now()),
            reconnect: app_callback(|app, _: JsValue| app.connect()),
            ws_open: app_callback(|app, _: Event| app.handle_ws_open()),
            ws_message: app_callback(TreemapApp::handle_ws_message),
            ws_close: app_callback(|app, _: Event| app.handle_ws_close()),
            ws_error: app_callback(|app, _: Event| {
                app.chrome.status_element.set_text_content(Some("Connection error."));
                Ok(())
            }),
            pixel_ratio: app_callback(|app, _: Event| app.handle_pixel_ratio_change()),
        }
    }
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
    app.watch_pixel_ratio()?;
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
                separator: '/',
                view_root_size: 0,
                zoom_anim: None,
                pending_old_rects: None,
                dirty: true,
                buffer_dirty: true,
                hover: None,
                tooltip_size: (0.0, 0.0),
                last_mouse: None,
                scan_done: false,
                layout_seq: 0,
            },
            session: SessionState {
                scan_start_time: 0.0,
                scan_timer_id: None,
                filter_timer_id: None,
                ws: None,
                reconnect_attempts: 0,
            },
            callbacks: Callbacks::new(),
        })
    }

    fn now(&self) -> f64 {
        self.window
            .performance()
            .map(|performance| performance.now())
            .unwrap_or(0.0)
    }
}
