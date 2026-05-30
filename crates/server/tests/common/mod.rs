#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Ensure the trunk frontend bundle exists; the server serves it at runtime from
/// `crates/wasm/dist`, so the e2e/visual tests need it present. This is test-only orchestration
/// (it never runs during a normal `cargo build`), so it does not reintroduce a build script.
///
/// nextest runs every test in its own process, so a `std::sync::Once` cannot serialize this —
/// dozens of test processes would launch `trunk build` concurrently and race on trunk's shared
/// wasm-bindgen download. So: skip if the bundle is already built (CI builds it once up front), and
/// otherwise take a cross-process lock (atomic `create_dir`) so only one process builds at a time.
fn ensure_frontend_built() {
    // Trunk.toml lives at the workspace root (two levels up from this crate).
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bundle = root.join("crates/wasm/dist/rsdirstat-wasm.js");
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
        let bin = env!("CARGO_BIN_EXE_rsdirstat-server");
        let mut child = Command::new(bin)
            .args(args)
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("failed to start rsdirstat-server");

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
    let pw = playwright_rs::Playwright::launch().await.unwrap();
    let browser = match browser_name {
        "chromium" => pw.chromium().launch().await.unwrap(),
        "firefox" => pw.firefox().launch().await.unwrap(),
        "webkit" => pw.webkit().launch().await.unwrap(),
        _ => panic!("unknown browser: {browser_name}"),
    };
    let page = browser.new_page().await.unwrap();
    page.set_viewport_size(playwright_rs::Viewport {
        width: 1280,
        height: 720,
    })
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
