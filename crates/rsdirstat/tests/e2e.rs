mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use common::{
    TestServer, assert_no_layout_after, create_test_dir, layout_seq, wait_for_layout_after, wait_for_scan_done,
};
use futures_util::future::LocalBoxFuture;
use futures_util::{FutureExt, StreamExt};
use playwright_rs::expect;
use playwright_rs::protocol::{HoverOptions, Position, WaitForFunctionOptions, WaitForOptions};

/// Run `f` on a new page in Chromium, traced under `chromium-<case>`; `f` navigates it.
async fn in_chromium(case: &str, f: impl AsyncFnOnce(&playwright_rs::Page)) {
    let (_pw, browser) = common::launch("chromium").await;
    let page = common::new_page(&browser, 1280, 720).await;
    common::with_trace_unit(&page, &common::trace_label("chromium", case), f(&page)).await;
    let _ = browser.close().await;
}

/// Serve a scan of the test tree, open it, wait for the scan, then run `f` on the page. Traced from
/// navigation onward, so a `wait_for_scan_done` timeout — the most likely cross-OS flake — is
/// captured, not just failures inside `f`.
async fn run_scanned(case: &str, f: impl AsyncFn(&playwright_rs::Page)) {
    let dir = create_test_dir();
    let server = TestServer::start(dir.path());
    in_chromium(case, async |page| {
        page.goto(&server.url, None).await.unwrap();
        wait_for_scan_done(page).await;
        f(page).await;
    })
    .await;
}

async fn run_picker<F: AsyncFn(&playwright_rs::Page)>(f: F) {
    let server = TestServer::start_picker();
    in_chromium(std::any::type_name::<F>(), async |page| {
        page.goto(&server.url, None).await.unwrap();
        // Best effort: `f` asserts on the picker itself, with a clearer message.
        let _ = page
            .locator("#picker")
            .wait_for(WaitForOptions::builder().timeout(5_000.0).build())
            .await;
        f(page).await;
    })
    .await;
}

/// The text of what `selector` finds on `page`; empty if it has none.
async fn text(page: &playwright_rs::Page, selector: &str) -> String {
    page.locator(selector).text_content().await.unwrap().unwrap_or_default()
}

/// Wait options for an element that should appear.
fn visible() -> WaitForOptions {
    WaitForOptions::builder().timeout(10_000.0).build()
}

/// Make the page smaller, which sends the server its new viewport.
async fn resize(page: &playwright_rs::Page) {
    page.set_viewport_size(playwright_rs::Viewport {
        width: 1000,
        height: 700,
    })
    .await
    .unwrap();
}

async fn page_title(page: &playwright_rs::Page) {
    assert_eq!(page.title().await.unwrap(), "rsdirstat");
}

async fn status_bar_shows_scan_results(page: &playwright_rs::Page) {
    let status = text(page, "#status").await;
    assert!(status.contains("dirs"), "status should show dir count: {status}");
    assert!(status.contains("MiB"), "status should show total size: {status}");
}

async fn breadcrumb_shows_root(page: &playwright_rs::Page) {
    assert!(
        !text(page, "#crumbs").await.is_empty(),
        "breadcrumb should not be empty"
    );
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

    let before = layout_seq(page).await;
    depth.select_option("1", None).await.unwrap();
    wait_for_layout_after(page, before).await;

    assert_eq!(depth.input_value(None).await.unwrap(), "1");
}

async fn color_mode_changes_layout(page: &playwright_rs::Page) {
    let canvas = page.locator("#treemap");
    let initial = canvas.screenshot(None).await.unwrap();

    let before = layout_seq(page).await;
    page.locator("#color-mode").select_option("1", None).await.unwrap();
    wait_for_layout_after(page, before).await;

    let after = canvas.screenshot(None).await.unwrap();
    assert_ne!(initial, after, "changing color mode should change the treemap");
}

async fn filter_by_extension(page: &playwright_rs::Page) {
    let canvas = page.locator("#treemap");
    let initial = canvas.screenshot(None).await.unwrap();

    let filter_ext = page.locator("#filter-ext");
    let before = layout_seq(page).await;
    filter_ext.fill("rs", None).await.unwrap();
    wait_for_layout_after(page, before).await;

    let filtered = canvas.screenshot(None).await.unwrap();
    assert_ne!(initial, filtered, "extension filter should change the treemap");
    // The total, and so each rect's share in the tooltip, counts only the matching files: the
    // three .rs files, 9 MB of the 17.3 MB.
    let status = text(page, "#status").await;
    assert!(status.contains("— 8.58 MiB"), "filtered total: {status}");

    let before = layout_seq(page).await;
    page.locator("#filter-clear").click(None).await.unwrap();
    wait_for_layout_after(page, before).await;

    assert!(filter_ext.input_value(None).await.unwrap().is_empty());
    let cleared = canvas.screenshot(None).await.unwrap();
    assert_eq!(initial, cleared, "clearing the filter should restore the treemap");
    let status = text(page, "#status").await;
    assert!(status.contains("— 16.5 MiB"), "total: {status}");
}

async fn filter_by_name(page: &playwright_rs::Page) {
    let canvas = page.locator("#treemap");
    let initial = canvas.screenshot(None).await.unwrap();

    let before = layout_seq(page).await;
    page.locator("#filter-name").fill("main", None).await.unwrap();
    wait_for_layout_after(page, before).await;

    let filtered = canvas.screenshot(None).await.unwrap();
    assert_ne!(initial, filtered, "name filter should change the treemap");
}

async fn filter_by_min_size(page: &playwright_rs::Page) {
    let canvas = page.locator("#treemap");
    let initial = canvas.screenshot(None).await.unwrap();

    let before = layout_seq(page).await;
    page.locator("#filter-min").fill("3", None).await.unwrap();
    wait_for_layout_after(page, before).await;

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
    expect(tooltip).not().to_have_css("display", "none").await.unwrap();

    assert!(!text(page, ".tip-name").await.is_empty());
    assert!(!text(page, ".tip-size").await.is_empty());
}

async fn hover_shows_path_bar(page: &playwright_rs::Page) {
    assert!(text(page, "#path-text").await.is_empty());

    page.locator("#treemap").hover(None).await.unwrap();
    expect(page.locator("#path-text"))
        .to_have_text_regex(r"\S")
        .await
        .unwrap();
    assert!(!text(page, "#path-size").await.is_empty());
}

/// Hover must resolve to the rect under the cursor. The canvas sits below the breadcrumb and tool
/// bars: just inside its top-left is the "src" folder's header strip, just inside its bottom-left is
/// `src/main.rs`. Regression: hover once landed on whatever was ~60px below the cursor.
async fn hover_hits_rect_under_cursor(page: &playwright_rs::Page) {
    let height = canvas_height(page).await;
    for (y, expected) in [
        (5.0, path_ending(&["src"])),
        (height - 5.0, path_ending(&["src", "main.rs"])),
    ] {
        hover_canvas_at(page, 20.0, y).await;
        expect(page.locator("#path-text"))
            .to_have_text_regex(&expected)
            .await
            .unwrap_or_else(|e| panic!("hover at canvas y={y}: {e}"));
    }
}

/// A pattern for a path-bar text ending in `parts`, joined as the server joins paths on this
/// platform.
fn path_ending(parts: &[&str]) -> String {
    let separator = if cfg!(windows) { r"\\" } else { "/" };
    let mut pattern: String = parts
        .iter()
        .map(|part| format!("{separator}{}", part.replace('.', r"\.")))
        .collect();
    pattern.push('$');
    pattern
}

async fn canvas_height(page: &playwright_rs::Page) -> f64 {
    page.evaluate_value("() => document.querySelector('#treemap').clientHeight")
        .await
        .unwrap()
        .parse()
        .unwrap()
}

async fn hover_canvas_at(page: &playwright_rs::Page, x: f64, y: f64) {
    let at = HoverOptions::builder().position(Position { x, y }).build();
    page.locator("#treemap").hover(Some(at)).await.unwrap();
}

/// A reloaded page must send its toolbar's values (defaults, or whatever the browser restored), or
/// the treemap and the toolbar can disagree. Regression: after a reload the toolbar showed an empty
/// name filter while the server kept filtering by the old one.
async fn reload_keeps_treemap_in_sync_with_toolbar(page: &playwright_rs::Page) {
    let before = layout_seq(page).await;
    page.locator("#filter-name")
        .fill("matches-nothing", None)
        .await
        .unwrap();
    wait_for_layout_after(page, before).await;

    page.reload(None).await.unwrap();
    wait_for_scan_done(page).await;

    // With the filter gone, the bottom-left of the canvas is `src/main.rs` again; with it still
    // applied, there is nothing there to hover.
    let filter = page.locator("#filter-name").input_value(None).await.unwrap();
    hover_canvas_at(page, 20.0, canvas_height(page).await - 5.0).await;
    let path_text = expect(page.locator("#path-text"));
    if filter.is_empty() {
        path_text
            .to_have_text_regex(&path_ending(&["src", "main.rs"]))
            .await
            .unwrap();
    } else {
        path_text.to_have_text("").await.unwrap();
    }
}

async fn click_navigates_into_directory(page: &playwright_rs::Page) {
    let initial_text = text(page, "#crumbs").await;
    let initial_parts: Vec<&str> = initial_text.split('/').collect();

    let before = layout_seq(page).await;
    page.locator("#treemap").click(None).await.unwrap();
    wait_for_layout_after(page, before).await;

    let after_text = text(page, "#crumbs").await;
    let after_parts: Vec<&str> = after_text.split('/').collect();

    assert!(
        after_parts.len() > initial_parts.len(),
        "clicking should navigate deeper: before={initial_text}, after={after_text}"
    );
}

async fn breadcrumb_click_navigates_back(page: &playwright_rs::Page) {
    let before = layout_seq(page).await;
    page.locator("#treemap").click(None).await.unwrap();
    wait_for_layout_after(page, before).await;

    let deep_text = text(page, "#crumbs").await;

    let before = layout_seq(page).await;
    page.locator("#crumbs span:first-child").click(None).await.unwrap();
    wait_for_layout_after(page, before).await;

    let back_text = text(page, "#crumbs").await;
    assert!(
        back_text.len() < deep_text.len(),
        "breadcrumb back: deep={deep_text}, back={back_text}"
    );
}

async fn picker_volume_click_starts_scan(page: &playwright_rs::Page) {
    page.locator(".volume-card:first-child").click(None).await.unwrap();

    // The page shows the treemap view as soon as a card is clicked, so wait for what only the
    // server's `ScanStart` brings: the scan's running time in the status bar (or, for a quick scan,
    // its summary). It comes before the scan produces anything, so this is safe for CI whatever the
    // disk's size.
    page.wait_for_function(
        "() => /Scanning|dirs/.test(document.querySelector('#status')?.textContent ?? '')",
        WaitForFunctionOptions::default().timeout(10_000.0),
    )
    .await
    .expect("clicking a volume should start a scan");
}

async fn picker_has_volume_details(page: &playwright_rs::Page) {
    assert!(page.locator("#picker").is_visible().await.unwrap());
    // The cards arrive with the page's `/volumes` request, after the picker itself.
    page.locator(".volume-card")
        .first()
        .wait_for(visible())
        .await
        .expect("should show at least one volume card");

    let name = text(page, ".volume-card:first-child .volume-name").await;
    assert!(!name.is_empty(), "volume card should have a name");

    let path = text(page, ".volume-card:first-child .volume-path").await;
    assert!(
        path.starts_with('/') || path.contains(":\\"),
        "volume should have a mount path: {path}"
    );

    let sizes = text(page, ".volume-card:first-child .volume-sizes").await;
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

    let before = layout_seq(page).await;
    rescan.click(None).await.unwrap();
    wait_for_layout_after(page, before).await;

    let status = text(page, "#status").await;
    assert!(status.contains(" dirs in "), "after rescan: {status}");
}

/// The frontend's own checks of its logic and codec, run as WebAssembly in the browser.
async fn wasm_self_checks(page: &playwright_rs::Page) {
    let result = page
        .evaluate_value(
            "() => { const r = window.wasmBindings.runBrowserTests(); return r.failed === 0 ? `PASS ${r.total}` : `FAIL ${r.failed}/${r.total}: ${r.failures.join('; ')}`; }",
        )
        .await
        .unwrap();

    assert!(result.starts_with("PASS"), "wasm self-checks failed: {result}");
}

/// A case of the cross-browser suite: `run` on the page of a finished scan of `fixture()`.
struct Case {
    name: &'static str,
    fixture: fn() -> tempfile::TempDir,
    run: for<'a> fn(&'a playwright_rs::Page) -> LocalBoxFuture<'a, ()>,
}

/// How many cases run at once in one browser.
const CONCURRENT_CASES: usize = 4;

/// Run every case in `browser_name`, or only those named in `RSDIRSTAT_E2E_CASES` (comma-separated).
/// The browser is launched once: a launch per case was most of the suite's time, Firefox's above
/// all. Each case gets its own server and a page in its own context, so cases share nothing but
/// the browser process, and a failure is reported by case name, with its own trace.
async fn run_cases(browser_name: &str) {
    let cases: Vec<&Case> = match std::env::var("RSDIRSTAT_E2E_CASES") {
        Err(_) => CASES.iter().collect(),
        Ok(names) => names
            .split(',')
            .map(|name| {
                CASES
                    .iter()
                    .find(|case| case.name == name)
                    .unwrap_or_else(|| panic!("no case named {name:?}"))
            })
            .collect(),
    };
    let (_pw, browser) = common::launch(browser_name).await;
    let failed: Vec<&str> = futures_util::stream::iter(cases)
        .map(|case| async { (case.name, run_case(&browser, browser_name, case).await) })
        .buffer_unordered(CONCURRENT_CASES)
        .filter_map(|(name, passed)| async move { (!passed).then_some(name) })
        .collect()
        .await;
    let _ = browser.close().await;
    assert!(failed.is_empty(), "failed in {browser_name}: {}", failed.join(", "));
}

/// Run one case; whether it passed. Its panic message is printed as usual.
async fn run_case(browser: &playwright_rs::Browser, browser_name: &str, case: &Case) -> bool {
    let fixture = case.fixture;
    // Creating the files and starting the server block, so off this thread, which runs every case.
    let (dir, server) = tokio::task::spawn_blocking(move || {
        let dir = fixture();
        let server = TestServer::start(dir.path());
        (dir, server)
    })
    .await
    .unwrap();
    let page = common::new_page(browser, 1280, 720).await;
    let label = common::trace_label(browser_name, case.name);
    let passed = AssertUnwindSafe(common::with_trace_unit(&page, &label, async {
        page.goto(&server.url, None).await.unwrap();
        wait_for_scan_done(&page).await;
        (case.run)(&page).await;
    }))
    .catch_unwind()
    .await
    .is_ok();
    if !passed {
        eprintln!("case {} failed in {browser_name}", case.name);
    }
    if let Ok(context) = page.context() {
        let _ = context.close().await;
    }
    drop((server, dir));
    passed
}

/// The cross-browser suite: one test per browser, `e2e_<browser>` (CI selects them by the `e2e`
/// prefix), running every case listed here (see `run_cases`). A case runs against
/// `create_test_dir()` unless it names another fixture with `in`.
macro_rules! browser_tests {
    (@fixture) => { create_test_dir };
    (@fixture $fixture:ident) => { $fixture };
    ($($name:ident => $case:ident $(in $fixture:ident)?),* $(,)?) => {
        const CASES: &[Case] = &[$(
            Case {
                name: stringify!($name),
                fixture: browser_tests!(@fixture $($fixture)?),
                run: |page| Box::pin($case(page)),
            },
        )*];

        #[tokio::test]
        async fn e2e_chromium() {
            run_cases("chromium").await;
        }

        #[tokio::test]
        async fn e2e_firefox() {
            run_cases("firefox").await;
        }

        #[tokio::test]
        async fn e2e_webkit() {
            run_cases("webkit").await;
        }
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
    hover_position => hover_hits_rect_under_cursor,
    navigate => click_navigates_into_directory,
    breadcrumb_back => breadcrumb_click_navigates_back,
    click_leaf_dir => click_navigates_into_directory in leaf_dir,
    rescan => rescan_button_works,
    reload => reload_keeps_treemap_in_sync_with_toolbar,
    // In-browser unit checks of the actual wasm exports.
    wasm_self_checks => wasm_self_checks,
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

/// Pick a volume that vanished between the picker listing it and the click: one whose root can't be
/// opened.
async fn pick_vanished_volume(page: &playwright_rs::Page) {
    page.locator(".volume-card").first().wait_for(visible()).await.unwrap();
    page.evaluate_expression(
        "document.querySelector('.picker-grid').insertAdjacentHTML('afterbegin', \
         '<div class=\"volume-card\" id=\"gone\" data-mount=\"/nonexistent/rsdirstat-e2e\">gone</div>')",
    )
    .await
    .unwrap();
    page.locator("#gone").click(None).await.unwrap();
}

/// A scan whose root can't be opened must not wedge the server: the tab is sent back to the volume
/// picker, a reloaded tab starts there too, and picking a real volume from it starts a scan.
async fn unopenable_root_falls_back_to_picker(page: &playwright_rs::Page) {
    let picker = page.locator("#picker");
    pick_vanished_volume(page).await;
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
}

#[tokio::test]
async fn e2e_unopenable_root_falls_back_to_picker() {
    run_picker(unopenable_root_falls_back_to_picker).await;
}

/// "Change" opens the picker in the page only; the server still has the finished scan and answers a
/// view change (here a resize) with its layout, which must not bring the treemap back over the
/// picker.
#[tokio::test]
async fn e2e_layouts_stay_behind_the_picker() {
    run_scanned("layouts_stay_behind_the_picker", async |page: &playwright_rs::Page| {
        page.locator("#change").click(None).await.unwrap();
        let before = layout_seq(page).await;
        resize(page).await;
        wait_for_layout_after(page, before).await;
        assert!(page.locator("#picker").is_visible().await.unwrap());
        assert!(!page.locator("#treemap-view").is_visible().await.unwrap());
    })
    .await;
}

/// After a finished scan, picking a path that can't be scanned sends the tabs back to the picker,
/// and they must stay there: the server still holds the old scan's tree, and any view change (here
/// a resize) once made it send that stale layout, which brought the treemap back.
#[tokio::test]
async fn e2e_failed_rescan_keeps_the_picker() {
    run_scanned("failed_rescan_keeps_the_picker", async |page: &playwright_rs::Page| {
        page.locator("#change").click(None).await.unwrap();
        pick_vanished_volume(page).await;
        page.locator("#picker").wait_for(visible()).await.unwrap();

        page.reload(None).await.unwrap();
        page.locator(".volume-card").first().wait_for(visible()).await.unwrap();
        resize(page).await;
        assert_no_layout_after(page, 0, "a reloaded tab got a layout while the server waits for a path").await;
        assert!(!page.locator("#treemap-view").is_visible().await.unwrap());
    })
    .await;
}

/// `--wait` (for profiling) holds the scan until the page's Start button is clicked, so that button
/// must be visible before any layout exists.
#[tokio::test]
async fn e2e_wait_mode_start_button_starts_scan() {
    let dir = create_test_dir();
    let server = TestServer::start_waiting(dir.path());
    in_chromium("wait_mode_start_button_starts_scan", async |page| {
        page.goto(&server.url, None).await.unwrap();
        let start = page.locator("#status button");
        start
            .wait_for(visible())
            .await
            .expect("the Start button should be visible");
        start.click(None).await.unwrap();
        wait_for_scan_done(page).await;
    })
    .await;
}

// Server lifecycle (no browser). Stopping the server mid-scan (SIGTERM, or Ctrl+Break on Windows)
// must cancel the walk: it runs on tokio's blocking pool, which the runtime waits on at exit, so an
// uncancelled whole-disk scan kept the process alive until it finished (60–100s on CI runners). And
// it must be a clean exit, not a kill, or the server's coverage data is lost.
#[test]
fn e2e_stop_mid_scan_exits_promptly() {
    let mut server = TestServer::start(std::path::Path::new("/"));
    std::thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    let status = server.stop();
    let took = started.elapsed();
    assert!(
        status.success(),
        "the server should exit cleanly, not be killed: {status}"
    );
    assert!(
        took < Duration::from_secs(5),
        "server took {took:?} to exit after being stopped mid-scan"
    );
}

/// A TCP connection to the server, as `TestServer::url`'s host names it.
fn connect(server: &TestServer) -> TcpStream {
    let port = server.url.rsplit_once(':').unwrap().1;
    TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap()
}

/// Open the WebSocket a tab holds while it is open. Dropping the stream closes it, as closing the
/// tab does.
fn open_tab(server: &TestServer) -> TcpStream {
    let mut stream = connect(server);
    let host = server.url.trim_start_matches("http://");
    write!(
        stream,
        "GET /ws HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    .unwrap();
    let mut status = String::new();
    BufReader::new(&stream).read_line(&mut status).unwrap();
    assert!(status.starts_with("HTTP/1.1 101"), "WebSocket refused: {status}");
    stream
}

/// The server exits a few seconds after its last tab closes, but not when a tab reloads: that
/// closes its connection and opens a new one within the grace period.
#[test]
fn e2e_server_exits_after_its_last_tab_closes() {
    let mut server = TestServer::start_picker();
    drop(open_tab(&server));
    let first_closed = Instant::now();
    std::thread::sleep(Duration::from_secs(2));
    let reloaded = open_tab(&server);
    std::thread::sleep(Duration::from_secs(1));
    drop(reloaded);
    let last_closed = Instant::now();

    // The grace period after the first close (5s) ends halfway through this wait.
    std::thread::sleep(Duration::from_millis(6_500).saturating_sub(first_closed.elapsed()));
    assert!(
        server.is_running(),
        "the server exited after a tab's first connection closed, though the tab reloaded"
    );
    let status = server
        .wait_for_exit(Duration::from_secs(10))
        .expect("the server should exit once its last tab has closed");
    assert!(status.success(), "{status}");
    assert!(
        last_closed.elapsed() >= Duration::from_millis(4_500),
        "the server exited {:?} after the last tab closed, before the grace period ended",
        last_closed.elapsed()
    );
}

/// Send a raw GET with the given extra headers and return the status code. `host` overrides the
/// `Host` header.
fn http_status(server: &TestServer, path: &str, host: Option<&str>, headers: &[(&str, &str)]) -> u16 {
    http_response(server, path, host, headers)
        .split(' ')
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap()
}

/// The response to a GET of `path`, as text (lossy, as a body needn't be).
fn http_response(server: &TestServer, path: &str, host: Option<&str>, headers: &[(&str, &str)]) -> String {
    let authority = server.url.trim_start_matches("http://");
    let mut stream = connect(server);
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        host.unwrap_or(authority)
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    // Bytes: a body (the volume list) needn't be text.
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    String::from_utf8_lossy(&response).into_owned()
}

/// A DNS-rebinding page sees this server as same-origin under the attacker's hostname; only its
/// `Host` header gives it away, so every request must name this machine.
#[test]
fn e2e_requests_for_another_host_are_refused() {
    let server = TestServer::start_picker();
    // Nor shown in another site's frame.
    let page = http_response(&server, "/", None, &[]).to_ascii_lowercase();
    assert!(page.contains("x-frame-options: deny"), "{page}");
    assert!(
        page.contains("content-security-policy: frame-ancestors 'none'"),
        "{page}"
    );
    assert_eq!(http_status(&server, "/volumes", None, &[]), 200);
    assert_eq!(http_status(&server, "/volumes", Some("127.0.0.1:1"), &[]), 200);
    for host in [
        "attacker.example",
        "attacker.example:8080",
        "127.0.0.1.attacker.example",
    ] {
        assert_eq!(http_status(&server, "/volumes", Some(host), &[]), 403, "Host: {host}");
        assert_eq!(http_status(&server, "/", Some(host), &[]), 403, "Host: {host}");
    }
}

/// The API endpoints are only for the app's own page. A page on another site can make a browser
/// send them a request (an `<img src=…/start>` carries no `Origin`), but the browser labels it
/// cross-site.
#[test]
fn e2e_cross_site_api_requests_are_refused() {
    let server = TestServer::start_picker();
    for site in ["same-origin", "none"] {
        assert_eq!(
            http_status(&server, "/volumes", None, &[("Sec-Fetch-Site", site)]),
            200,
            "{site}"
        );
    }
    for path in ["/start", "/volumes"] {
        for site in ["cross-site", "same-site"] {
            assert_eq!(
                http_status(&server, path, None, &[("Sec-Fetch-Site", site)]),
                403,
                "{path} {site}"
            );
        }
        let origin = [("Origin", "http://attacker.example")];
        assert_eq!(
            http_status(&server, path, None, &origin),
            403,
            "{path} from another origin"
        );
    }
}

/// Each tab has its own view: zooming into a folder or filtering in one tab leaves another tab
/// where it was.
#[tokio::test]
async fn e2e_tabs_have_independent_views() {
    let dir = create_test_dir();
    let server = TestServer::start(dir.path());
    let (_pw, browser) = common::launch("chromium").await;
    let first = common::new_page(&browser, 1280, 720).await;
    let second = common::new_page(&browser, 1280, 720).await;
    let label = common::trace_label("chromium", "tabs_have_independent_views");
    common::with_trace_unit(&first, &label, async {
        for page in [&first, &second] {
            page.goto(&server.url, None).await.unwrap();
            wait_for_scan_done(page).await;
        }
        let second_crumbs = text(&second, "#crumbs").await;
        let second_canvas = second.locator("#treemap").screenshot(None).await.unwrap();

        let before = layout_seq(&first).await;
        first.locator("#treemap").click(None).await.unwrap();
        wait_for_layout_after(&first, before).await;
        let before = layout_seq(&first).await;
        first.locator("#filter-name").fill("main", None).await.unwrap();
        wait_for_layout_after(&first, before).await;
        assert_ne!(text(&first, "#crumbs").await, second_crumbs, "the first tab zoomed in");

        assert_no_layout_after(
            &second,
            layout_seq(&second).await,
            "the second tab got a layout for the first tab's view",
        )
        .await;
        assert_eq!(text(&second, "#crumbs").await, second_crumbs);
        assert_eq!(
            second.locator("#treemap").screenshot(None).await.unwrap(),
            second_canvas
        );
    })
    .await;
    let _ = browser.close().await;
}
