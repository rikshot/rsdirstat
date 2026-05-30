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

pub(super) fn add_class(element: &HtmlElement, class_name: &str) -> Result<(), JsValue> {
    let element: &Element = element.unchecked_ref();
    element.class_list().add_1(class_name)
}

pub(super) fn remove_class(element: &HtmlElement, class_name: &str) -> Result<(), JsValue> {
    let element: &Element = element.unchecked_ref();
    element.class_list().remove_1(class_name)
}

pub(super) fn set_hidden<T: JsCast>(element: &T, hidden: bool) -> Result<(), JsValue> {
    let element = element
        .dyn_ref::<HtmlElement>()
        .ok_or_else(|| JsValue::from_str("element is not HtmlElement"))?;
    if hidden {
        add_class(element, "hidden")
    } else {
        remove_class(element, "hidden")
    }
}
