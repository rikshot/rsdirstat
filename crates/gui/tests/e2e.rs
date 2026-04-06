mod common;

use std::time::Duration;

use common::{TestServer, create_test_dir, wait_for_scan_done};
use playwright_rs::{Playwright, Viewport};

async fn setup_with(
    browser_name: &str,
) -> (
    tempfile::TempDir,
    TestServer,
    Playwright,
    playwright_rs::Browser,
    playwright_rs::Page,
) {
    let dir = create_test_dir();
    let server = TestServer::start(dir.path());

    let pw = Playwright::launch().await.unwrap();
    let browser = match browser_name {
        "chromium" => pw.chromium().launch().await.unwrap(),
        "firefox" => pw.firefox().launch().await.unwrap(),
        "webkit" => pw.webkit().launch().await.unwrap(),
        _ => panic!("unknown browser: {browser_name}"),
    };
    let page = browser.new_page().await.unwrap();
    page.set_viewport_size(Viewport {
        width: 1280,
        height: 720,
    })
    .await
    .unwrap();
    page.goto(&server.url, None).await.unwrap();
    wait_for_scan_done(&page).await;

    (dir, server, pw, browser, page)
}

async fn run(browser_name: &str, f: impl AsyncFn(&playwright_rs::Page)) {
    let (_dir, _server, _pw, browser, page) = setup_with(browser_name).await;
    f(&page).await;
    let _ = browser.close().await;
}

async fn page_title(page: &playwright_rs::Page) {
    assert_eq!(page.title().await.unwrap(), "rsdirstat");
}

async fn status_bar_shows_scan_results(page: &playwright_rs::Page) {
    let text = page.locator("#status").await.text_content().await.unwrap().unwrap();
    assert!(text.contains("dirs"), "status should show dir count: {text}");
    assert!(text.contains("MiB"), "status should show total size: {text}");
}

async fn breadcrumb_shows_root(page: &playwright_rs::Page) {
    let text = page.locator("#crumbs").await.text_content().await.unwrap().unwrap();
    assert!(!text.is_empty(), "breadcrumb should not be empty");
}

async fn toolbar_elements_visible(page: &playwright_rs::Page) {
    assert!(page.locator("#toolbar").await.is_visible().await.unwrap());
    assert!(page.locator("#breadcrumb-bar").await.is_visible().await.unwrap());
    assert!(page.locator("#treemap").await.is_visible().await.unwrap());
    assert!(page.locator("#depth").await.is_visible().await.unwrap());
    assert!(page.locator("#color-mode").await.is_visible().await.unwrap());
}

async fn depth_selector_changes_value(page: &playwright_rs::Page) {
    let depth = page.locator("#depth").await;
    assert_eq!(depth.input_value(None).await.unwrap(), "5");

    depth.select_option("1", None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(depth.input_value(None).await.unwrap(), "1");
}

async fn color_mode_changes_layout(page: &playwright_rs::Page) {
    let canvas = page.locator("#treemap").await;
    let initial = canvas.screenshot(None).await.unwrap();

    page.locator("#color-mode")
        .await
        .select_option("1", None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let after = canvas.screenshot(None).await.unwrap();
    assert_ne!(initial, after, "changing color mode should change the treemap");
}

async fn filter_by_extension(page: &playwright_rs::Page) {
    let canvas = page.locator("#treemap").await;
    let initial = canvas.screenshot(None).await.unwrap();

    let filter_ext = page.locator("#filter-ext").await;
    filter_ext.fill("rs", None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    let filtered = canvas.screenshot(None).await.unwrap();
    assert_ne!(initial, filtered, "extension filter should change the treemap");

    page.locator("#filter-clear").await.click(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(filter_ext.input_value(None).await.unwrap().is_empty());
}

async fn filter_by_name(page: &playwright_rs::Page) {
    let canvas = page.locator("#treemap").await;
    let initial = canvas.screenshot(None).await.unwrap();

    page.locator("#filter-name").await.fill("main", None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    let filtered = canvas.screenshot(None).await.unwrap();
    assert_ne!(initial, filtered, "name filter should change the treemap");
}

async fn filter_by_min_size(page: &playwright_rs::Page) {
    let canvas = page.locator("#treemap").await;
    let initial = canvas.screenshot(None).await.unwrap();

    page.locator("#filter-min").await.fill("3", None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    let filtered = canvas.screenshot(None).await.unwrap();
    assert_ne!(initial, filtered, "size filter should change the treemap");
}

async fn hover_shows_tooltip(page: &playwright_rs::Page) {
    let tooltip = page.locator("#tooltip").await;
    let display = tooltip
        .evaluate::<String, ()>("el => getComputedStyle(el).display", None)
        .await
        .unwrap();
    assert_eq!(display, "none");

    page.locator("#treemap").await.hover(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let display = tooltip
        .evaluate::<String, ()>("el => getComputedStyle(el).display", None)
        .await
        .unwrap();
    assert_ne!(display, "none");

    assert!(
        !page
            .locator(".tip-name")
            .await
            .text_content()
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );
    assert!(
        !page
            .locator(".tip-size")
            .await
            .text_content()
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );
}

async fn hover_shows_path_bar(page: &playwright_rs::Page) {
    let path_text = page.locator("#path-text").await;
    assert!(path_text.text_content().await.unwrap().unwrap_or_default().is_empty());

    page.locator("#treemap").await.hover(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(!path_text.text_content().await.unwrap().unwrap_or_default().is_empty());
    assert!(
        !page
            .locator("#path-size")
            .await
            .text_content()
            .await
            .unwrap()
            .unwrap_or_default()
            .is_empty()
    );
}

async fn click_navigates_into_directory(page: &playwright_rs::Page) {
    let crumbs = page.locator("#crumbs").await;
    let initial_text = crumbs.text_content().await.unwrap().unwrap();
    let initial_parts: Vec<&str> = initial_text.split('/').collect();

    page.locator("#treemap").await.click(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let after_text = crumbs.text_content().await.unwrap().unwrap();
    let after_parts: Vec<&str> = after_text.split('/').collect();

    assert!(
        after_parts.len() > initial_parts.len(),
        "clicking should navigate deeper: before={initial_text}, after={after_text}"
    );
}

async fn breadcrumb_click_navigates_back(page: &playwright_rs::Page) {
    page.locator("#treemap").await.click(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let crumbs = page.locator("#crumbs").await;
    let deep_text = crumbs.text_content().await.unwrap().unwrap();

    page.locator("#crumbs span:first-child")
        .await
        .click(None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let back_text = crumbs.text_content().await.unwrap().unwrap();
    assert!(
        back_text.len() <= deep_text.len(),
        "breadcrumb back: deep={deep_text}, back={back_text}"
    );
}

async fn rescan_button_works(page: &playwright_rs::Page) {
    let rescan = page.locator("#rescan").await;
    let classes = rescan.evaluate::<String, ()>("el => el.className", None).await.unwrap();
    assert!(!classes.contains("hidden"), "rescan should be visible: {classes}");

    rescan.click(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let text = page.locator("#status").await.text_content().await.unwrap().unwrap();
    assert!(
        text.contains("dirs") || text.contains("Scanning"),
        "after rescan: {text}"
    );
}

#[tokio::test]
async fn e2e_page_title_chromium() {
    run("chromium", page_title).await;
}
#[tokio::test]
async fn e2e_status_bar_chromium() {
    run("chromium", status_bar_shows_scan_results).await;
}
#[tokio::test]
async fn e2e_breadcrumb_chromium() {
    run("chromium", breadcrumb_shows_root).await;
}
#[tokio::test]
async fn e2e_toolbar_chromium() {
    run("chromium", toolbar_elements_visible).await;
}
#[tokio::test]
async fn e2e_depth_chromium() {
    run("chromium", depth_selector_changes_value).await;
}
#[tokio::test]
async fn e2e_color_mode_chromium() {
    run("chromium", color_mode_changes_layout).await;
}
#[tokio::test]
async fn e2e_filter_ext_chromium() {
    run("chromium", filter_by_extension).await;
}
#[tokio::test]
async fn e2e_filter_name_chromium() {
    run("chromium", filter_by_name).await;
}
#[tokio::test]
async fn e2e_filter_size_chromium() {
    run("chromium", filter_by_min_size).await;
}
#[tokio::test]
async fn e2e_tooltip_chromium() {
    run("chromium", hover_shows_tooltip).await;
}
#[tokio::test]
async fn e2e_path_bar_chromium() {
    run("chromium", hover_shows_path_bar).await;
}
#[tokio::test]
async fn e2e_navigate_chromium() {
    run("chromium", click_navigates_into_directory).await;
}
#[tokio::test]
async fn e2e_breadcrumb_back_chromium() {
    run("chromium", breadcrumb_click_navigates_back).await;
}
#[tokio::test]
async fn e2e_rescan_chromium() {
    run("chromium", rescan_button_works).await;
}

#[tokio::test]
async fn e2e_page_title_firefox() {
    run("firefox", page_title).await;
}
#[tokio::test]
async fn e2e_status_bar_firefox() {
    run("firefox", status_bar_shows_scan_results).await;
}
#[tokio::test]
async fn e2e_breadcrumb_firefox() {
    run("firefox", breadcrumb_shows_root).await;
}
#[tokio::test]
async fn e2e_toolbar_firefox() {
    run("firefox", toolbar_elements_visible).await;
}
#[tokio::test]
async fn e2e_depth_firefox() {
    run("firefox", depth_selector_changes_value).await;
}
#[tokio::test]
async fn e2e_color_mode_firefox() {
    run("firefox", color_mode_changes_layout).await;
}
#[tokio::test]
async fn e2e_filter_ext_firefox() {
    run("firefox", filter_by_extension).await;
}
#[tokio::test]
async fn e2e_filter_name_firefox() {
    run("firefox", filter_by_name).await;
}
#[tokio::test]
async fn e2e_filter_size_firefox() {
    run("firefox", filter_by_min_size).await;
}
#[tokio::test]
async fn e2e_tooltip_firefox() {
    run("firefox", hover_shows_tooltip).await;
}
#[tokio::test]
async fn e2e_path_bar_firefox() {
    run("firefox", hover_shows_path_bar).await;
}
#[tokio::test]
async fn e2e_navigate_firefox() {
    run("firefox", click_navigates_into_directory).await;
}
#[tokio::test]
async fn e2e_breadcrumb_back_firefox() {
    run("firefox", breadcrumb_click_navigates_back).await;
}
#[tokio::test]
async fn e2e_rescan_firefox() {
    run("firefox", rescan_button_works).await;
}

#[tokio::test]
async fn e2e_page_title_webkit() {
    run("webkit", page_title).await;
}
#[tokio::test]
async fn e2e_status_bar_webkit() {
    run("webkit", status_bar_shows_scan_results).await;
}
#[tokio::test]
async fn e2e_breadcrumb_webkit() {
    run("webkit", breadcrumb_shows_root).await;
}
#[tokio::test]
async fn e2e_toolbar_webkit() {
    run("webkit", toolbar_elements_visible).await;
}
#[tokio::test]
async fn e2e_depth_webkit() {
    run("webkit", depth_selector_changes_value).await;
}
#[tokio::test]
async fn e2e_color_mode_webkit() {
    run("webkit", color_mode_changes_layout).await;
}
#[tokio::test]
async fn e2e_filter_ext_webkit() {
    run("webkit", filter_by_extension).await;
}
#[tokio::test]
async fn e2e_filter_name_webkit() {
    run("webkit", filter_by_name).await;
}
#[tokio::test]
async fn e2e_filter_size_webkit() {
    run("webkit", filter_by_min_size).await;
}
#[tokio::test]
async fn e2e_tooltip_webkit() {
    run("webkit", hover_shows_tooltip).await;
}
#[tokio::test]
async fn e2e_path_bar_webkit() {
    run("webkit", hover_shows_path_bar).await;
}
#[tokio::test]
async fn e2e_navigate_webkit() {
    run("webkit", click_navigates_into_directory).await;
}
#[tokio::test]
async fn e2e_breadcrumb_back_webkit() {
    run("webkit", breadcrumb_click_navigates_back).await;
}
#[tokio::test]
async fn e2e_rescan_webkit() {
    run("webkit", rescan_button_works).await;
}
