mod common;

use std::path::{Path, PathBuf};

use common::{TestServer, create_test_dir, launch_browser_sized, wait_for_scan_done};
use playwright_rs::{Animations, ScreenshotAssertionOptions, expect_page};

/// Set to re-record the baselines instead of comparing against them (the "Update visual baselines"
/// workflow sets it on each OS's runner).
const UPDATE_ENV: &str = "RSDIRSTAT_UPDATE_SNAPSHOTS";

/// Where a baseline lives. CI compares against the committed set for its OS
/// (`tests/snapshots/{linux,macos,windows}/`), recorded on CI's own runners. A developer machine
/// renders text differently from those, so local runs use a git-ignored `tests/snapshots/local/`
/// instead, recorded on the first run: it still catches "I just broke the rendering".
fn baseline_path(name: &str) -> PathBuf {
    let dir = if std::env::var_os("CI").is_some() {
        std::env::consts::OS
    } else {
        "local"
    };
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(dir)
        .join(name)
}

fn screenshot_options(update: bool) -> ScreenshotAssertionOptions {
    ScreenshotAssertionOptions::builder()
        .max_diff_pixel_ratio(0.02)
        .threshold(0.3)
        .animations(Animations::Disabled)
        .update_snapshots(update)
        .build()
}

/// Scan a fixed test tree, render it in `browser` at the given viewport, and
/// assert the treemap matches `snapshot`. Traced (retain-on-failure) under a
/// `<browser>-<case>` label, so the label always tracks the browser it ran under.
async fn run_visual(
    browser: &str,
    width: u32,
    height: u32,
    snapshot: &str,
    case: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let baseline = baseline_path(snapshot);
    let update = std::env::var_os(UPDATE_ENV).is_some();
    // Playwright records a missing baseline and passes, so in CI, where the baselines are
    // committed, a missing one has to fail here or the test would check nothing.
    if std::env::var_os("CI").is_some() && !update && !baseline.exists() {
        panic!(
            "missing baseline {}: record it with the \"Update visual baselines\" workflow",
            baseline.display()
        );
    }

    let dir = create_test_dir();
    let server = TestServer::start(dir.path());
    let (_pw, browser_handle, page) = launch_browser_sized(browser, width, height).await;

    let label = common::trace_label(browser, case);
    common::with_trace(&page, &label, async {
        page.goto(&server.url, None).await?;
        wait_for_scan_done(&page).await;
        expect_page(&page)
            .to_have_screenshot(&baseline, Some(screenshot_options(update)))
            .await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })
    .await?;

    browser_handle.close().await?;
    Ok(())
}

#[tokio::test]
async fn visual_treemap_chromium() -> Result<(), Box<dyn std::error::Error>> {
    run_visual("chromium", 1280, 720, "treemap-chromium.png", "visual_treemap").await
}

#[tokio::test]
async fn visual_treemap_firefox() -> Result<(), Box<dyn std::error::Error>> {
    run_visual("firefox", 1280, 720, "treemap-firefox.png", "visual_treemap").await
}

#[tokio::test]
async fn visual_treemap_webkit() -> Result<(), Box<dyn std::error::Error>> {
    run_visual("webkit", 1280, 720, "treemap-webkit.png", "visual_treemap").await
}

#[tokio::test]
async fn visual_responsive_small() -> Result<(), Box<dyn std::error::Error>> {
    run_visual("chromium", 640, 480, "treemap-small.png", "visual_responsive_small").await
}
