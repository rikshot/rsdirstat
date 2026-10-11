use super::*;

pub(super) fn with_app(f: impl FnOnce(&Rc<RefCell<TreemapApp>>)) {
    APP.with(|slot| {
        if let Some(app) = slot.borrow().as_ref() {
            f(app);
        }
    });
}

pub(super) fn by_id<T: JsCast>(document: &Document, id: &str) -> Result<T, JsValue> {
    document
        .get_element_by_id(id)
        .ok_or_else(|| JsValue::from_str(&format!("missing element #{id}")))?
        .dyn_into::<T>()
        .map_err(|_| JsValue::from_str(&format!("failed to cast element #{id}")))
}

pub(super) fn query<T: JsCast>(root: &HtmlElement, selector: &str) -> Result<T, JsValue> {
    root.query_selector(selector)?
        .ok_or_else(|| JsValue::from_str(&format!("missing selector {selector}")))?
        .dyn_into::<T>()
        .map_err(|_| JsValue::from_str(&format!("failed to cast selector {selector}")))
}

pub(super) fn canvas_context(canvas: &HtmlCanvasElement) -> Result<CanvasRenderingContext2d, JsValue> {
    // The canvas is always cleared to an opaque background, so disabling the alpha channel lets
    // the browser skip per-frame compositing of transparency.
    let options = js_sys::Object::new();
    js_sys::Reflect::set(&options, &JsValue::from_str("alpha"), &JsValue::FALSE)?;
    canvas
        .get_context_with_context_options("2d", &options)?
        .ok_or_else(|| JsValue::from_str("missing 2d context"))?
        .dyn_into::<CanvasRenderingContext2d>()
        .map_err(|_| JsValue::from_str("failed to cast 2d context"))
}

/// Show or hide an element via the `hidden` class.
pub(super) fn set_hidden(element: &Element, hidden: bool) -> Result<(), JsValue> {
    element.class_list().toggle_with_force("hidden", hidden).map(|_| ())
}

pub(super) fn is_hidden(element: &Element) -> bool {
    element.class_list().contains("hidden")
}

/// A callback for the browser that forwards each call to `handler` on the app. It captures nothing
/// but `handler`, so one can be made once and handed over any number of times.
pub(super) fn app_callback<E: FromWasmAbi + 'static>(
    handler: fn(&mut TreemapApp, E) -> Result<(), JsValue>,
) -> Closure<dyn FnMut(E)> {
    Closure::new(move |event: E| {
        with_app(|app| {
            let _ = handler(&mut app.borrow_mut(), event);
        });
    })
}

/// Attach a page-lifetime listener that forwards each event to `handler` on the app. The closure
/// is deliberately leaked: these listeners live as long as the page does.
pub(super) fn listen<E: FromWasmAbi + 'static>(
    target: &EventTarget,
    event: &str,
    handler: fn(&mut TreemapApp, E) -> Result<(), JsValue>,
) -> Result<(), JsValue> {
    let closure = app_callback(handler);
    target.add_event_listener_with_callback(event, closure.as_ref().unchecked_ref())?;
    closure.forget();
    Ok(())
}

/// For a delegated listener: `attribute` on the event target or its nearest ancestor that has it.
pub(super) fn delegated_attribute(event: &Event, attribute: &str) -> Option<String> {
    let target = event.target()?.dyn_into::<Element>().ok()?;
    target
        .closest(&format!("[{attribute}]"))
        .ok()??
        .get_attribute(attribute)
}

pub(super) fn window_inner_size(window: &Window) -> Result<(f64, f64), JsValue> {
    let width = window.inner_width()?.as_f64();
    let height = window.inner_height()?.as_f64();
    width
        .zip(height)
        .ok_or_else(|| JsValue::from_str("missing innerWidth/innerHeight"))
}
