#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::FutureExt;

/// Ensure the trunk frontend bundle exists; debug builds of the server read it from
/// `crates/rsdirstat/dist` on disk (rust-embed only embeds in release), so the e2e/visual tests
/// need it present. This is test-only orchestration (it never runs during a normal `cargo build`),
/// so it does not reintroduce a build script.
///
/// nextest runs every test in its own process, so a `std::sync::Once` cannot serialize this —
/// dozens of test processes would launch `trunk build` concurrently and race on trunk's shared
/// wasm-bindgen download. So: skip if the bundle is already built (CI builds it once up front), and
/// otherwise take a cross-process lock (atomic `create_dir`) so only one process builds at a time.
fn ensure_frontend_built() {
    // Trunk.toml lives at the workspace root (two levels up from this crate).
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bundle = root.join("crates/rsdirstat/dist/rsdirstat-wasm.js");
    if bundle.exists() {
        return;
    }
    let lock = root.join("target/.trunk-build.lock");
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if bundle.exists() {
            return;
        }
        match std::fs::create_dir(&lock) {
            Ok(()) => {
                let status = Command::new("trunk")
                    .arg("build")
                    .current_dir(&root)
                    .status()
                    .expect("failed to run `trunk build` — is trunk installed? (cargo install trunk)");
                // Release the lock before asserting so a failed build doesn't strand other waiters.
                let _ = std::fs::remove_dir(&lock);
                assert!(status.success(), "trunk build failed with status {status}");
                return;
            }
            Err(_) => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for another process to run `trunk build`"
                );
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

pub struct TestServer {
    child: Child,
    pub url: String,
}

impl TestServer {
    pub fn start(scan_path: &Path) -> Self {
        Self::spawn(&["--port", "0", "--no-open", scan_path.to_str().unwrap()])
    }

    pub fn start_picker() -> Self {
        Self::spawn(&["--port", "0", "--no-open"])
    }

    fn spawn(args: &[&str]) -> Self {
        ensure_frontend_built();
        let bin = env!("CARGO_BIN_EXE_rsdirstat");
        let mut child = Command::new(bin)
            .args(args)
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("failed to start rsdirstat");

        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            let mut sent = false;
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if !sent && let Some(u) = line.strip_prefix("Listening on ") {
                    let _ = tx.send(u.to_string());
                    sent = true;
                }
            }
        });

        let url = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("server did not print its URL within 10s");
        TestServer { child, url }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        #[cfg(not(unix))]
        let _ = self.child.kill();

        let _ = self.child.wait();
    }
}

pub fn create_test_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::create_dir(root.join("src").join("nested")).unwrap();
    std::fs::create_dir(root.join("docs")).unwrap();
    std::fs::create_dir(root.join("assets")).unwrap();

    std::fs::write(root.join("src").join("main.rs"), vec![0u8; 5_000_000]).unwrap();
    std::fs::write(root.join("src").join("lib.rs"), vec![0u8; 3_000_000]).unwrap();
    std::fs::write(root.join("src").join("nested").join("deep.rs"), vec![0u8; 1_000_000]).unwrap();
    std::fs::write(root.join("docs").join("readme.md"), vec![0u8; 2_000_000]).unwrap();
    std::fs::write(root.join("docs").join("guide.txt"), vec![0u8; 1_500_000]).unwrap();
    std::fs::write(root.join("assets").join("logo.png"), vec![0u8; 4_000_000]).unwrap();
    std::fs::write(root.join("assets").join("style.css"), vec![0u8; 800_000]).unwrap();

    dir
}

pub async fn launch_browser(
    browser_name: &str,
) -> (playwright_rs::Playwright, playwright_rs::Browser, playwright_rs::Page) {
    launch_browser_sized(browser_name, 1280, 720).await
}

pub async fn launch_browser_sized(
    browser_name: &str,
    width: u32,
    height: u32,
) -> (playwright_rs::Playwright, playwright_rs::Browser, playwright_rs::Page) {
    let pw = playwright_rs::Playwright::launch().await.unwrap();
    let browser = match browser_name {
        "chromium" => pw.chromium().launch().await.unwrap(),
        "firefox" => pw.firefox().launch().await.unwrap(),
        "webkit" => pw.webkit().launch().await.unwrap(),
        _ => panic!("unknown browser: {browser_name}"),
    };
    let page = browser.new_page().await.unwrap();
    page.set_viewport_size(playwright_rs::Viewport { width, height })
        .await
        .unwrap();
    (pw, browser, page)
}

pub async fn wait_for_scan_done(page: &playwright_rs::Page) {
    let status = page.locator("#status").await;
    for _ in 0..100 {
        if let Ok(Some(text)) = status.text_content().await
            && text.contains(" dirs")
        {
            tokio::time::sleep(Duration::from_millis(500)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("scan did not complete within 10 seconds");
}

/// Directory where Playwright failure traces are written. Only tests that fail
/// populate it; CI uploads it as an artifact so a red cross-OS browser run is
/// debuggable postmortem (DOM snapshots + screenshots + console/network timeline).
pub fn trace_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/traces")
}

/// Build a filesystem-safe, unique trace label from the browser and the test
/// body's name. `name` is typically `std::any::type_name::<F>()` (e.g.
/// `"e2e::page_title"`); we keep the final path segment.
pub fn trace_label(browser: &str, name: &str) -> String {
    let short = name.rsplit("::").next().unwrap_or(name);
    let safe: String = short
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{browser}-{safe}")
}

/// Run `body` with Playwright tracing active on `page`'s context, saving the
/// trace to `tests/traces/<label>.zip` only if the body fails — either by
/// panicking (an assertion) or by returning `Err`. On success the trace is
/// discarded. Panics are re-raised afterwards so the test still fails as usual.
/// This mirrors Playwright's own `retain-on-failure` trace policy.
///
/// Tracing is best-effort: if it can't be started (e.g. an unexpected driver
/// state) the body still runs untraced, so tracing never turns a green test red.
pub async fn with_trace<F, T, E>(page: &playwright_rs::Page, label: &str, body: F) -> Result<T, E>
where
    F: std::future::Future<Output = Result<T, E>>,
{
    let tracing = trace_start(page, label).await;
    let outcome = AssertUnwindSafe(body).catch_unwind().await;
    let failed = !matches!(outcome, Ok(Ok(_)));
    trace_stop(tracing, label, failed).await;
    match outcome {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// [`with_trace`] for bodies that signal failure only by panicking (the e2e
/// tests, which assert rather than return `Result`). Keeps the `Infallible`
/// plumbing in one place instead of at every call site.
pub async fn with_trace_unit<F>(page: &playwright_rs::Page, label: &str, body: F)
where
    F: std::future::Future<Output = ()>,
{
    let _ = with_trace(page, label, async move {
        body.await;
        Ok::<(), std::convert::Infallible>(())
    })
    .await;
}

async fn trace_start(page: &playwright_rs::Page, label: &str) -> Option<playwright_rs::Tracing> {
    let tracing = page.context().ok()?.tracing().await.ok()?;
    tracing
        .start(Some(
            playwright_rs::TracingStartOptions::default()
                .name(label)
                .screenshots(true)
                .snapshots(true),
        ))
        .await
        .ok()?;
    Some(tracing)
}

async fn trace_stop(tracing: Option<playwright_rs::Tracing>, label: &str, failed: bool) {
    let Some(tracing) = tracing else { return };
    let path = trace_dir().join(format!("{label}.zip"));
    if !failed {
        let _ = tracing.stop(None).await; // discard in-memory trace
        // Under `nextest` retries a flaky test can fail then pass; drop any zip an
        // earlier failed attempt left behind so a passed test never ships a trace.
        let _ = std::fs::remove_file(&path);
        return;
    }
    let _ = std::fs::create_dir_all(trace_dir());
    let opts = playwright_rs::TracingStopOptions::default().path(path.to_string_lossy().into_owned());
    if tracing.stop(Some(opts)).await.is_ok() {
        // Surface the path so it's greppable in CI logs alongside the failure.
        eprintln!("playwright trace saved: {}", path.display());
    }
}
