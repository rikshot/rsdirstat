mod common;

use std::time::Duration;

use common::{TestServer, create_test_dir, launch_browser, wait_for_scan_done};
use playwright_rs::protocol::{WaitForFunctionOptions, WaitForOptions};

/// Serve a scan of `dir`, open it in `browser_name`, wait for the scan, then run `f` on the page.
/// Traced under `<browser>-<case>` from navigation onward, so a `wait_for_scan_done` timeout — the
/// most likely cross-OS flake — is captured, not just failures inside `f`.
async fn run_in<F: AsyncFn(&playwright_rs::Page)>(browser_name: &str, case: &str, dir: tempfile::TempDir, f: F) {
    let server = TestServer::start(dir.path());
    let (_pw, browser, page) = launch_browser(browser_name).await;
    common::with_trace_unit(&page, &common::trace_label(browser_name, case), async {
        page.goto(&server.url, None).await.unwrap();
        wait_for_scan_done(&page).await;
        f(&page).await;
    })
    .await;
    let _ = browser.close().await;
}

async fn run_picker<F: AsyncFn(&playwright_rs::Page)>(f: F) {
    let server = TestServer::start_picker();
    let (_pw, browser, page) = launch_browser("chromium").await;
    let label = common::trace_label("chromium", std::any::type_name::<F>());
    common::with_trace_unit(&page, &label, async {
        page.goto(&format!("{}/?picker", server.url), None).await.unwrap();
        // Best effort: `f` asserts on the picker itself, with a clearer message.
        let _ = page
            .locator("#picker")
            .wait_for(WaitForOptions::builder().timeout(5_000.0).build())
            .await;
        f(&page).await;
    })
    .await;
    let _ = browser.close().await;
}

async fn page_title(page: &playwright_rs::Page) {
    assert_eq!(page.title().await.unwrap(), "rsdirstat");
}

async fn status_bar_shows_scan_results(page: &playwright_rs::Page) {
    let text = page.locator("#status").text_content().await.unwrap().unwrap();
    assert!(text.contains("dirs"), "status should show dir count: {text}");
    assert!(text.contains("MiB"), "status should show total size: {text}");
}

async fn breadcrumb_shows_root(page: &playwright_rs::Page) {
    let text = page.locator("#crumbs").text_content().await.unwrap().unwrap();
    assert!(!text.is_empty(), "breadcrumb should not be empty");
}

async fn toolbar_elements_visible(page: &playwright_rs::Page) {
    assert!(page.locator("#toolbar").is_visible().await.unwrap());
    assert!(page.locator("#breadcrumb-bar").is_visible().await.unwrap());
    assert!(page.locator("#treemap").is_visible().await.unwrap());
    assert!(page.locator("#depth").is_visible().await.unwrap());
    assert!(page.locator("#color-mode").is_visible().await.unwrap());
}

async fn depth_selector_changes_value(page: &playwright_rs::Page) {
    let depth = page.locator("#depth");
    assert_eq!(depth.input_value(None).await.unwrap(), "6");

    depth.select_option("1", None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(depth.input_value(None).await.unwrap(), "1");
}

async fn color_mode_changes_layout(page: &playwright_rs::Page) {
    let canvas = page.locator("#treemap");
    let initial = canvas.screenshot(None).await.unwrap();

    page.locator("#color-mode").select_option("1", None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let after = canvas.screenshot(None).await.unwrap();
    assert_ne!(initial, after, "changing color mode should change the treemap");
}

async fn filter_by_extension(page: &playwright_rs::Page) {
    let canvas = page.locator("#treemap");
    let initial = canvas.screenshot(None).await.unwrap();

    let filter_ext = page.locator("#filter-ext");
    filter_ext.fill("rs", None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    let filtered = canvas.screenshot(None).await.unwrap();
    assert_ne!(initial, filtered, "extension filter should change the treemap");

    page.locator("#filter-clear").click(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(filter_ext.input_value(None).await.unwrap().is_empty());
}

async fn filter_by_name(page: &playwright_rs::Page) {
    let canvas = page.locator("#treemap");
    let initial = canvas.screenshot(None).await.unwrap();

    page.locator("#filter-name").fill("main", None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    let filtered = canvas.screenshot(None).await.unwrap();
    assert_ne!(initial, filtered, "name filter should change the treemap");
}

async fn filter_by_min_size(page: &playwright_rs::Page) {
    let canvas = page.locator("#treemap");
    let initial = canvas.screenshot(None).await.unwrap();

    page.locator("#filter-min").fill("3", None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    let filtered = canvas.screenshot(None).await.unwrap();
    assert_ne!(initial, filtered, "size filter should change the treemap");
}

async fn hover_shows_tooltip(page: &playwright_rs::Page) {
    let tooltip = page.locator("#tooltip");
    let display = tooltip
        .evaluate::<String, ()>("el => getComputedStyle(el).display", None)
        .await
        .unwrap();
    assert_eq!(display, "none");

    page.locator("#treemap").hover(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let display = tooltip
        .evaluate::<String, ()>("el => getComputedStyle(el).display", None)
        .await
        .unwrap();
    assert_ne!(display, "none");

    assert!(
        !page
            .locator(".tip-name")
            .text_content()
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );
    assert!(
        !page
            .locator(".tip-size")
            .text_content()
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );
}

async fn hover_shows_path_bar(page: &playwright_rs::Page) {
    let path_text = page.locator("#path-text");
    assert!(path_text.text_content().await.unwrap().unwrap_or_default().is_empty());

    page.locator("#treemap").hover(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(!path_text.text_content().await.unwrap().unwrap_or_default().is_empty());
    assert!(
        !page
            .locator("#path-size")
            .text_content()
            .await
            .unwrap()
            .unwrap_or_default()
            .is_empty()
    );
}

async fn click_navigates_into_directory(page: &playwright_rs::Page) {
    let crumbs = page.locator("#crumbs");
    let initial_text = crumbs.text_content().await.unwrap().unwrap();
    let initial_parts: Vec<&str> = initial_text.split('/').collect();

    page.locator("#treemap").click(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let after_text = crumbs.text_content().await.unwrap().unwrap();
    let after_parts: Vec<&str> = after_text.split('/').collect();

    assert!(
        after_parts.len() > initial_parts.len(),
        "clicking should navigate deeper: before={initial_text}, after={after_text}"
    );
}

async fn breadcrumb_click_navigates_back(page: &playwright_rs::Page) {
    page.locator("#treemap").click(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let crumbs = page.locator("#crumbs");
    let deep_text = crumbs.text_content().await.unwrap().unwrap();

    page.locator("#crumbs span:first-child").click(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let back_text = crumbs.text_content().await.unwrap().unwrap();
    assert!(
        back_text.len() <= deep_text.len(),
        "breadcrumb back: deep={deep_text}, back={back_text}"
    );
}

async fn picker_volume_click_starts_scan(page: &playwright_rs::Page) {
    page.locator(".volume-card:first-child").click(None).await.unwrap();

    // Just wait for the treemap view to appear (MSG_SCAN_START triggers this immediately,
    // before the actual scan produces results — safe for CI regardless of disk size)
    page.locator("#treemap-view")
        .wait_for(WaitForOptions::builder().timeout(10_000.0).build())
        .await
        .expect("treemap view did not appear after clicking volume");
}

async fn picker_has_volume_details(page: &playwright_rs::Page) {
    assert!(page.locator("#picker").is_visible().await.unwrap());
    assert!(
        page.locator(".volume-card:first-child").is_visible().await.unwrap(),
        "should show at least one volume card"
    );

    let name = page
        .locator(".volume-card:first-child .volume-name")
        .text_content()
        .await
        .unwrap()
        .unwrap_or_default();
    assert!(!name.is_empty(), "volume card should have a name");

    let path = page
        .locator(".volume-card:first-child .volume-path")
        .text_content()
        .await
        .unwrap()
        .unwrap_or_default();
    assert!(
        path.starts_with('/') || path.contains(":\\"),
        "volume should have a mount path: {path}"
    );

    let sizes = page
        .locator(".volume-card:first-child .volume-sizes")
        .text_content()
        .await
        .unwrap()
        .unwrap_or_default();
    assert!(sizes.contains("used of"), "should show usage: {sizes}");
}

/// A tree whose largest folder ("big") has no sub-folders, so a click in the middle of the treemap
/// lands inside it (on its file). Regression fixture: such a folder once wasn't a container, and
/// clicking it navigated to its parent instead of into it.
fn leaf_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("big")).unwrap();
    std::fs::write(root.join("big").join("a.bin"), vec![0u8; 10_000_000]).unwrap();
    std::fs::create_dir(root.join("small")).unwrap();
    std::fs::write(root.join("small").join("b.bin"), vec![0u8; 1_000_000]).unwrap();
    dir
}

async fn rescan_button_works(page: &playwright_rs::Page) {
    let rescan = page.locator("#rescan");
    let classes = rescan.evaluate::<String, ()>("el => el.className", None).await.unwrap();
    assert!(!classes.contains("hidden"), "rescan should be visible: {classes}");

    rescan.click(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let text = page.locator("#status").text_content().await.unwrap().unwrap();
    assert!(
        text.contains("dirs") || text.contains("Scanning"),
        "after rescan: {text}"
    );
}

async fn js_unit_tests(page: &playwright_rs::Page) {
    let result = page
        .evaluate_value(
            "() => { const r = window.wasmBindings.runBrowserTests(); return r.failed === 0 ? `PASS ${r.total}` : `FAIL ${r.failed}/${r.total}: ${r.failures.join('; ')}`; }",
        )
        .await
        .unwrap();

    assert!(result.starts_with("PASS"), "JS unit tests failed: {result}");
}

/// Every case runs once per browser, as `e2e_<browser>::<name>` (CI selects them by the `e2e`
/// prefix). A case runs against `create_test_dir()` unless it names another fixture with `in`.
macro_rules! browser_tests {
    (@fixture) => { create_test_dir() };
    (@fixture $fixture:ident) => { $fixture() };
    (@browser $module:ident, $browser:literal, $($name:ident => $case:ident $(in $fixture:ident)?),*) => {
        mod $module {
            use super::*;
            $(
                #[tokio::test]
                async fn $name() {
                    run_in($browser, stringify!($name), browser_tests!(@fixture $($fixture)?), super::$case).await;
                }
            )*
        }
    };
    ($($name:ident => $case:ident $(in $fixture:ident)?),* $(,)?) => {
        browser_tests!(@browser e2e_chromium, "chromium", $($name => $case $(in $fixture)?),*);
        browser_tests!(@browser e2e_firefox, "firefox", $($name => $case $(in $fixture)?),*);
        browser_tests!(@browser e2e_webkit, "webkit", $($name => $case $(in $fixture)?),*);
    };
}

browser_tests! {
    page_title => page_title,
    status_bar => status_bar_shows_scan_results,
    breadcrumb => breadcrumb_shows_root,
    toolbar => toolbar_elements_visible,
    depth => depth_selector_changes_value,
    color_mode => color_mode_changes_layout,
    filter_ext => filter_by_extension,
    filter_name => filter_by_name,
    filter_size => filter_by_min_size,
    tooltip => hover_shows_tooltip,
    path_bar => hover_shows_path_bar,
    navigate => click_navigates_into_directory,
    breadcrumb_back => breadcrumb_click_navigates_back,
    click_leaf_dir => click_navigates_into_directory in leaf_dir,
    rescan => rescan_button_works,
    // In-browser unit checks of the actual wasm exports.
    js_unit_tests => js_unit_tests,
}

// Picker mode tests (chromium only — testing server/UI logic, not browser compat)
#[tokio::test]
async fn e2e_picker_volume_details() {
    run_picker(picker_has_volume_details).await;
}
#[tokio::test]
async fn e2e_picker_click_starts_scan() {
    run_picker(picker_volume_click_starts_scan).await;
}

/// A scan whose root can't be opened (here: a volume that vanished between listing and clicking)
/// must not wedge the server: the tab is sent back to the volume picker, a reloaded tab starts
/// there too, and picking a real volume from it starts a scan.
#[tokio::test]
async fn e2e_unopenable_root_falls_back_to_picker() {
    let server = TestServer::start_picker();
    let (_pw, browser, page) = launch_browser("chromium").await;
    let label = common::trace_label("chromium", "unopenable_root_falls_back_to_picker");
    common::with_trace_unit(&page, &label, async {
        let picker = page.locator("#picker");
        let visible = || WaitForOptions::builder().timeout(10_000.0).build();

        page.goto(&format!("{}/?picker", server.url), None).await.unwrap();
        page.locator(".volume-card").first().wait_for(visible()).await.unwrap();
        page.evaluate_expression(
            "document.querySelector('.picker-grid').insertAdjacentHTML('afterbegin', \
             '<div class=\"volume-card\" id=\"gone\" data-mount=\"/nonexistent/rsdirstat-e2e\">gone</div>')",
        )
        .await
        .unwrap();
        page.locator("#gone").click(None).await.unwrap();
        picker
            .wait_for(visible())
            .await
            .expect("the failed scan should send the tab back to the volume picker");

        page.reload(None).await.unwrap();
        picker
            .wait_for(visible())
            .await
            .expect("a new tab should start on the volume picker");

        page.locator(".volume-card").first().click(None).await.unwrap();
        page.wait_for_function(
            "() => /Scanning|dirs/.test(document.querySelector('#status')?.textContent ?? '')",
            WaitForFunctionOptions::default().timeout(10_000.0),
        )
        .await
        .expect("picking a volume should start a scan");
    })
    .await;
    let _ = browser.close().await;
}

/// `--wait` (for profiling) holds the scan until the page's Start button is clicked, so that button
/// must be visible before any layout exists.
#[tokio::test]
async fn e2e_wait_mode_start_button_starts_scan() {
    let dir = create_test_dir();
    let server = TestServer::start_waiting(dir.path());
    let (_pw, browser, page) = launch_browser("chromium").await;
    let label = common::trace_label("chromium", "wait_mode_start_button_starts_scan");
    common::with_trace_unit(&page, &label, async {
        page.goto(&format!("{}/?wait", server.url), None).await.unwrap();
        let start = page.locator("#status button");
        start
            .wait_for(WaitForOptions::builder().timeout(10_000.0).build())
            .await
            .expect("the Start button should be visible");
        start.click(None).await.unwrap();
        wait_for_scan_done(&page).await;
    })
    .await;
    let _ = browser.close().await;
}

// Server lifecycle (no browser). SIGTERM mid-scan must cancel the walk: it runs on tokio's blocking
// pool, which the runtime waits on at exit, so an uncancelled whole-disk scan kept the process alive
// until it finished (60–100s on CI runners).
#[cfg(unix)]
#[test]
fn e2e_sigterm_mid_scan_exits_promptly() {
    let server = TestServer::start(std::path::Path::new("/"));
    std::thread::sleep(Duration::from_millis(500));
    let started = std::time::Instant::now();
    drop(server); // SIGTERM, then wait for exit
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "server took {took:?} to exit after SIGTERM mid-scan"
    );
}
