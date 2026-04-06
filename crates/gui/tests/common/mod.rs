use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

pub struct TestServer {
    child: Child,
    pub url: String,
}

impl TestServer {
    pub fn start(scan_path: &Path) -> Self {
        let bin = env!("CARGO_BIN_EXE_rsdirstat-gui");
        let mut child = Command::new(bin)
            .args(["--port", "0", "--no-open"])
            .arg(scan_path)
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("failed to start rsdirstat-gui");

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
                if !sent {
                    if let Some(u) = line.strip_prefix("Listening on ") {
                        let _ = tx.send(u.to_string());
                        sent = true;
                    }
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

pub async fn wait_for_scan_done(page: &playwright_rs::Page) {
    let status = page.locator("#status").await;
    for _ in 0..100 {
        if let Ok(Some(text)) = status.text_content().await {
            if text.contains(" dirs") {
                tokio::time::sleep(Duration::from_millis(500)).await;
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("scan did not complete within 10 seconds");
}
