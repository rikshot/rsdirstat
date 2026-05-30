#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(any(target_arch = "wasm32", test))]
mod logic;

#[cfg(any(target_arch = "wasm32", test))]
use std::cell::RefCell;
#[cfg(any(target_arch = "wasm32", test))]
use std::collections::HashMap;

#[cfg(target_arch = "wasm32")]
use js_sys::{Array, Object, Reflect};
#[cfg(any(target_arch = "wasm32", test))]
use rsdirstat_protocol::{self as wire, BreadcrumbEntry, ClientMessage, LayoutRect, ServerMessage};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg(any(target_arch = "wasm32", test))]
thread_local! {
    static HSL_CACHE: RefCell<HashMap<u32, String>> = RefCell::new(HashMap::new());
}

#[cfg(target_arch = "wasm32")]
fn set(obj: &Object, key: &str, value: impl Into<JsValue>) -> Result<(), JsValue> {
    Reflect::set(obj, &JsValue::from_str(key), &value.into()).map(|_| ())
}

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn format_size_impl(bytes: f64) -> String {
    let mut value = bytes.max(0.0);
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut index = 0usize;
    while value >= 1024.0 && index < units.len() - 1 {
        value /= 1024.0;
        index += 1;
    }
    if index == 0 {
        format!("{value:.0} {}", units[index])
    } else if value < 10.0 {
        format!("{value:.2} {}", units[index])
    } else if value < 100.0 {
        format!("{value:.1} {}", units[index])
    } else {
        format!("{value:.0} {}", units[index])
    }
}

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn hsl_impl(hue: u16, saturation: u8, lightness: u8) -> String {
    let key = (hue as u32) * 10_000 + (saturation as u32) * 100 + lightness as u32;
    HSL_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache
            .entry(key)
            .or_insert_with(|| format!("hsl({hue},{saturation}%,{lightness}%)"))
            .clone()
    })
}

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn hit_test_impl(x: f64, y: f64, w: f64, h: f64, mouse_x: f64, mouse_y: f64) -> bool {
    mouse_x >= x && mouse_x < x + w && mouse_y >= y && mouse_y < y + h
}

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn collapse_slashes(path: String) -> String {
    let mut out = String::with_capacity(path.len());
    let mut last_was_slash = false;
    for ch in path.chars() {
        if ch == '/' {
            if !last_was_slash {
                out.push(ch);
            }
            last_was_slash = true;
        } else {
            out.push(ch);
            last_was_slash = false;
        }
    }
    out
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = runBrowserTests)]
pub fn run_browser_tests_js() -> Result<JsValue, JsValue> {
    let failures = Array::new();
    let mut total = 0u32;
    let mut record = |name: &str, passed: bool| {
        total += 1;
        if !passed {
            failures.push(&JsValue::from_str(name));
        }
    };

    record("formatSize zero", format_size_impl(0.0) == "0 B");
    record("formatSize kib", format_size_impl(1024.0) == "1.00 KiB");
    record("hsl", hsl_impl(120, 50, 40) == "hsl(120,50%,40%)");
    record("hitTest", hit_test_impl(10.0, 20.0, 50.0, 30.0, 35.0, 35.0));
    record(
        "viewport roundtrip",
        matches!(
            ClientMessage::decode(&wire::encode_viewport(800.0, 600.0)),
            Some(ClientMessage::Viewport { width, height }) if width == 800.0 && height == 600.0
        ),
    );
    record(
        "navigate roundtrip",
        matches!(
            ClientMessage::decode(&wire::encode_navigate(42)),
            Some(ClientMessage::Navigate { id }) if id == 42
        ),
    );

    let breadcrumb = vec![BreadcrumbEntry {
        id: 7,
        name: "root".into(),
    }];
    let rects = vec![LayoutRect {
        id: 42,
        parent_id: 7,
        x: 10.0,
        y: 20.0,
        w: 100.0,
        h: 60.0,
        name: "src".into(),
        hue: 120,
        size: 5_000,
        depth: 2,
        is_container: true,
        header_height: 18.0,
        is_files: false,
        is_file: false,
        mtime: 1_700_000_000,
    }];
    let layout_ok = matches!(
        wire::decode_server_message(&wire::encode_layout(10_000, 3, true, &breadcrumb, &rects)),
        Some(ServerMessage::Layout(decoded))
            if decoded.root_size == 10_000
                && decoded.dir_count == 3
                && decoded.scan_done
                && decoded.breadcrumb == breadcrumb
                && decoded.rects == rects
    );
    record("layout roundtrip", layout_ok);
    record("collapse slashes", collapse_slashes("///a//b".into()) == "/a/b");

    let result = Object::new();
    set(&result, "total", total)?;
    set(&result, "failed", failures.length())?;
    set(&result, "failures", failures)?;
    Ok(result.into())
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    app::start_browser_app()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_smoke_message_has_viewport_tag() {
        assert_eq!(wire::encode_viewport(1280.0, 720.0)[0], 1);
    }

    #[test]
    fn format_size_matches_existing_js_behavior() {
        assert_eq!(format_size_impl(0.0), "0 B");
        assert_eq!(format_size_impl(512.0), "512 B");
        assert_eq!(format_size_impl(1024.0), "1.00 KiB");
        assert_eq!(format_size_impl(1024.0 * 1024.0), "1.00 MiB");
        assert_eq!(format_size_impl(-1.0), "0 B");
    }

    #[test]
    fn hsl_format_matches_existing_js_behavior() {
        assert_eq!(hsl_impl(120, 50, 40), "hsl(120,50%,40%)");
        assert_eq!(hsl_impl(120, 50, 40), "hsl(120,50%,40%)");
    }

    #[test]
    fn hit_test_matches_existing_js_behavior() {
        assert!(hit_test_impl(10.0, 20.0, 50.0, 30.0, 35.0, 35.0));
        assert!(hit_test_impl(10.0, 20.0, 50.0, 30.0, 10.0, 20.0));
        assert!(!hit_test_impl(10.0, 20.0, 50.0, 30.0, 60.0, 50.0));
    }

    #[test]
    fn collapse_slashes_matches_existing_js_behavior() {
        assert_eq!(collapse_slashes("///a//b".into()), "/a/b");
        assert_eq!(collapse_slashes("/".into()), "/");
    }

    #[test]
    fn viewport_message_roundtrip() {
        assert!(matches!(
            ClientMessage::decode(&wire::encode_viewport(800.0, 600.0)),
            Some(ClientMessage::Viewport { width, height }) if width == 800.0 && height == 600.0
        ));
    }

    #[test]
    fn navigate_message_roundtrip() {
        assert!(matches!(
            ClientMessage::decode(&wire::encode_navigate(42)),
            Some(ClientMessage::Navigate { id }) if id == 42
        ));
    }

    #[test]
    fn layout_message_roundtrip() {
        let breadcrumb = vec![BreadcrumbEntry {
            id: 7,
            name: "root".into(),
        }];
        let rects = vec![LayoutRect {
            id: 42,
            parent_id: 7,
            x: 10.0,
            y: 20.0,
            w: 100.0,
            h: 60.0,
            name: "src".into(),
            hue: 120,
            size: 5_000,
            depth: 2,
            is_container: true,
            header_height: 18.0,
            is_files: false,
            is_file: false,
            mtime: 1_700_000_000,
        }];

        assert!(matches!(
            wire::decode_server_message(&wire::encode_layout(10_000, 3, true, &breadcrumb, &rects)),
            Some(ServerMessage::Layout(decoded))
                if decoded.root_size == 10_000
                    && decoded.dir_count == 3
                    && decoded.scan_done
                    && decoded.breadcrumb == breadcrumb
                    && decoded.rects == rects
        ));
    }
}
