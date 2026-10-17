#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(any(target_arch = "wasm32", test))]
mod logic;
#[cfg(any(target_arch = "wasm32", test))]
mod style;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// In-browser self-test hook for the e2e suite: runs `logic::self_checks` under wasm32 and reports
/// `{ total, failed, failures }`.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = runBrowserTests)]
pub fn run_browser_tests_js() -> Result<JsValue, JsValue> {
    use js_sys::{Array, Object, Reflect};

    let checks = logic::self_checks();
    let failures = Array::new();
    for (name, _) in checks.iter().filter(|(_, passed)| !passed) {
        failures.push(&JsValue::from_str(name));
    }
    let result = Object::new();
    Reflect::set(&result, &"total".into(), &(checks.len() as u32).into())?;
    Reflect::set(&result, &"failed".into(), &failures.length().into())?;
    Reflect::set(&result, &"failures".into(), &failures)?;
    Ok(result.into())
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    app::start_browser_app()
}
