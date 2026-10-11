#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime};

use futures_util::FutureExt;
use playwright_rs::protocol::WaitForFunctionOptions;

/// Ensure the trunk frontend bundle is built and up to date; debug builds of the server read it
/// from `crates/rsdirstat/dist` on disk (rust-embed only embeds in release), so the e2e/visual
/// tests need it, and a bundle older than the frontend sources would test stale code. This is
/// test-only orchestration (it never runs during a normal `cargo build`), so it does not
/// reintroduce a build script.
///
/// nextest runs every test in its own process, so a `std::sync::Once` cannot serialize this —
/// dozens of test processes would launch `trunk build` concurrently and race on trunk's shared
/// wasm-bindgen download. Each process takes an OS file lock, which the OS releases when the
/// process exits (so a killed run can't leave the next one waiting), and the first to find the
/// bundle stale rebuilds it. CI builds it up front, so there it is always fresh.
fn ensure_frontend_built() {
    // Trunk.toml lives at the workspace root (two levels up from this crate).
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::create_dir_all(root.join("target")).unwrap();
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("target/.trunk-build.lock"))
        .unwrap();
    lock.lock().unwrap();
    if frontend_is_fresh(&root) {
        return;
    }
    let status = Command::new("trunk")
        .arg("build")
        .current_dir(&root)
        .status()
        .expect("failed to run `trunk build` — is trunk installed? (cargo install trunk)");
    assert!(status.success(), "trunk build failed with status {status}");
}

/// Whether the bundle exists and is newer than everything trunk builds it from.
fn frontend_is_fresh(root: &Path) -> bool {
    let Ok(built) =
        std::fs::metadata(root.join("crates/rsdirstat/dist/rsdirstat-wasm_bg.wasm")).and_then(|m| m.modified())
    else {
        return false;
    };
    ["crates/wasm", "crates/protocol/src", "Trunk.toml", "Cargo.lock"]
        .iter()
        .all(|input| newest_mtime(&root.join(input)) <= built)
}

fn newest_mtime(path: &Path) -> SystemTime {
    let Ok(meta) = std::fs::metadata(path) else {
        return SystemTime::UNIX_EPOCH;
    };
    let own = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if !meta.is_dir() {
        return own;
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| newest_mtime(&entry.path()))
        .fold(own, SystemTime::max)
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

    /// Like `start`, but with `--wait`: the scan begins when the page's Start button is clicked
    /// (load the page with `?wait`).
    pub fn start_waiting(scan_path: &Path) -> Self {
        Self::spawn(&["--port", "0", "--no-open", "--wait", scan_path.to_str().unwrap()])
    }

    fn spawn(args: &[&str]) -> Self {
        ensure_frontend_built();
        let mut command = Command::new(env!("CARGO_BIN_EXE_rsdirstat"));
        command.args(args).stderr(Stdio::piped()).stdout(Stdio::null());
        // In a process group of its own, so `stop` can send it Ctrl+Break.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(
            &mut command,
            windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP,
        );
        let mut child = command.spawn().expect("failed to start rsdirstat");

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

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Wait up to `timeout` for the server to exit; how it exited, if it did.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Stop the server the way a terminal would (SIGTERM, or Ctrl+Break on Windows) and wait for it
    /// to exit, killing it after 10s. A clean exit writes the server's coverage data, which a kill
    /// would lose.
    pub fn stop(&mut self) -> ExitStatus {
        // Never signal a process that has already exited: its id may belong to another by now.
        if let Ok(Some(status)) = self.child.try_wait() {
            return status;
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        // Sent to the server's own process group (see `spawn`).
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent(
                windows_sys::Win32::System::Console::CTRL_BREAK_EVENT,
                self.child.id(),
            );
        }
        self.wait_for_exit(Duration::from_secs(10)).unwrap_or_else(|| {
            let _ = self.child.kill();
            self.child.wait().expect("failed to wait for rsdirstat")
        })
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
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
    let (pw, browser) = launch(browser_name).await;
    let page = new_page(&browser, width, height).await;
    (pw, browser, page)
}

/// Launch `browser_name` ("chromium", "firefox" or "webkit"), with no page open.
pub async fn launch(browser_name: &str) -> (playwright_rs::Playwright, playwright_rs::Browser) {
    let pw = playwright_rs::Playwright::launch().await.unwrap();
    let browser = match browser_name {
        "chromium" => pw.chromium().launch().await.unwrap(),
        "firefox" => pw.firefox().launch().await.unwrap(),
        "webkit" => pw.webkit().launch().await.unwrap(),
        _ => panic!("unknown browser: {browser_name}"),
    };
    (pw, browser)
}

/// A new page in `browser`, with a `width` × `height` viewport, in a context of its own (a fresh
/// profile: no cookies or storage shared with any other page).
pub async fn new_page(browser: &playwright_rs::Browser, width: u32, height: u32) -> playwright_rs::Page {
    let page = browser.new_page().await.unwrap();
    page.set_viewport_size(playwright_rs::Viewport { width, height })
        .await
        .unwrap();
    page
}

/// Wait for the scan to finish and its last layout to be drawn, so callers can assert on or
/// screenshot a stable treemap.
pub async fn wait_for_scan_done(page: &playwright_rs::Page) {
    wait_for_layout_after(page, 0).await;
}

/// How many layouts the page has received (`data-layout-seq` on the canvas, 0 before the first).
/// Read it before an action that should produce a new layout, then pass it to
/// [`wait_for_layout_after`].
pub async fn layout_seq(page: &playwright_rs::Page) -> u64 {
    page.evaluate_value("() => Number(document.querySelector('#treemap')?.dataset.layoutSeq ?? 0)")
        .await
        .unwrap()
        .parse()
        .unwrap()
}

/// Wait until the page has received a layout newer than `after`, the scan is done, and that
/// layout's final frame is drawn (any zoom animation has finished).
///
/// The frontend writes these markers as `data-*` attributes on the canvas, so the wait polls
/// inside the page: one driver round-trip, and the driver enforces the timeout, so a hang reports
/// `Timeout 10000ms exceeded` instead of a bare panic.
pub async fn wait_for_layout_after(page: &playwright_rs::Page, after: u64) {
    let settled = format!(
        "() => {{ const d = document.querySelector('#treemap')?.dataset; \
         return !!d && Number(d.layoutSeq) > {after} && d.scanDone === 'true' \
         && d.renderedSeq === d.layoutSeq; }}"
    );
    page.wait_for_function(&settled, WaitForFunctionOptions::default().timeout(10_000.0))
        .await
        .unwrap_or_else(|e| panic!("no settled layout after #{after} within 10 seconds: {e}"));
}

/// Assert that the page gets no layout newer than `after` within a second: long enough for one sent
/// in response to whatever the caller just did to arrive.
pub async fn assert_no_layout_after(page: &playwright_rs::Page, after: u64, message: &str) {
    let newer = format!("() => Number(document.querySelector('#treemap')?.dataset.layoutSeq ?? 0) > {after}");
    let arrived = page
        .wait_for_function(&newer, WaitForFunctionOptions::default().timeout(1_000.0))
        .await;
    assert!(arrived.is_err(), "{message}");
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
