mod common;

use std::path::{Path, PathBuf};

use common::{TestServer, create_test_dir, wait_for_scan_done};
use playwright_rs::{Animations, Playwright, ScreenshotAssertionOptions, Viewport, expect_page};

fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots")
}

fn screenshot_options() -> ScreenshotAssertionOptions {
    ScreenshotAssertionOptions::builder()
        .max_diff_pixel_ratio(0.02)
        .threshold(0.3)
        .animations(Animations::Disabled)
        .build()
}

#[tokio::test]
async fn visual_treemap_chromium() -> Result<(), Box<dyn std::error::Error>> {
    let dir = create_test_dir();
    let server = TestServer::start(dir.path());

    let pw = Playwright::launch().await?;
    let browser = pw.chromium().launch().await?;
    let page = browser.new_page().await?;

    page.set_viewport_size(Viewport {
        width: 1280,
        height: 720,
    })
    .await?;
    page.goto(&server.url, None).await?;
    wait_for_scan_done(&page).await;

    expect_page(&page)
        .to_have_screenshot(snapshot_dir().join("treemap-chromium.png"), Some(screenshot_options()))
        .await?;

    browser.close().await?;
    Ok(())
}

#[tokio::test]
async fn visual_treemap_firefox() -> Result<(), Box<dyn std::error::Error>> {
    let dir = create_test_dir();
    let server = TestServer::start(dir.path());

    let pw = Playwright::launch().await?;
    let browser = pw.firefox().launch().await?;
    let page = browser.new_page().await?;

    page.set_viewport_size(Viewport {
        width: 1280,
        height: 720,
    })
    .await?;
    page.goto(&server.url, None).await?;
    wait_for_scan_done(&page).await;

    expect_page(&page)
        .to_have_screenshot(snapshot_dir().join("treemap-firefox.png"), Some(screenshot_options()))
        .await?;

    browser.close().await?;
    Ok(())
}

#[tokio::test]
async fn visual_treemap_webkit() -> Result<(), Box<dyn std::error::Error>> {
    let dir = create_test_dir();
    let server = TestServer::start(dir.path());

    let pw = Playwright::launch().await?;
    let browser = pw.webkit().launch().await?;
    let page = browser.new_page().await?;

    page.set_viewport_size(Viewport {
        width: 1280,
        height: 720,
    })
    .await?;
    page.goto(&server.url, None).await?;
    wait_for_scan_done(&page).await;

    expect_page(&page)
        .to_have_screenshot(snapshot_dir().join("treemap-webkit.png"), Some(screenshot_options()))
        .await?;

    browser.close().await?;
    Ok(())
}

#[tokio::test]
async fn visual_responsive_small() -> Result<(), Box<dyn std::error::Error>> {
    let dir = create_test_dir();
    let server = TestServer::start(dir.path());

    let pw = Playwright::launch().await?;
    let browser = pw.chromium().launch().await?;
    let page = browser.new_page().await?;

    page.set_viewport_size(Viewport {
        width: 640,
        height: 480,
    })
    .await?;
    page.goto(&server.url, None).await?;
    wait_for_scan_done(&page).await;

    expect_page(&page)
        .to_have_screenshot(snapshot_dir().join("treemap-small.png"), Some(screenshot_options()))
        .await?;

    browser.close().await?;
    Ok(())
}
