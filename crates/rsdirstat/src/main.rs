#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use parking_lot::{Mutex, RwLock};
use rust_embed::RustEmbed;
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast, watch};
use tower_http::compression::CompressionLayer;

use rsdirstat_core::layout::{self, LayoutConfig};
use rsdirstat_core::tree::FilterConfig;
use rsdirstat_protocol::{self as wire, ClientMessage, ScanEvent};

#[cfg(target_os = "linux")]
use rsdirstat_linux as scanner;
#[cfg(target_os = "macos")]
use rsdirstat_macos as scanner;
#[cfg(target_os = "windows")]
use rsdirstat_windows as scanner;

#[derive(Parser)]
#[command(name = "rsdirstat", version, about = "Interactive treemap disk usage server")]
struct Args {
    /// Path to scan (omit to show volume picker)
    path: Option<PathBuf>,

    /// Cross filesystem boundaries
    #[arg(long)]
    all: bool,

    /// Port for the GUI server (0 = random)
    #[arg(long, default_value_t = 0)]
    port: u16,

    /// Don't auto-open the browser
    #[arg(long)]
    no_open: bool,

    /// Wait for manual start before scanning (for profiling)
    #[arg(long)]
    wait: bool,
}

#[derive(Clone, PartialEq)]
struct ViewConfig {
    viewport: (f64, f64),
    view_root: Option<u64>,
    max_depth: u8,
    color_mode: u8,
    filter: FilterConfig,
}

/// Grace period after the last client disconnects before the server exits. Long enough to
/// survive a page reload (which briefly drops the WebSocket) without shutting down underneath
/// the user.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Where the server is in a scan's life. Scans never overlap: one starts only from a phase other
/// than `Running`.
enum ScanPhase {
    /// No path to scan: the volume picker is up. At startup without a path, and after a scan
    /// couldn't start.
    AwaitingPath,
    /// A path is set, and the scan waits for the page's Start button (`--wait`).
    AwaitingStart,
    /// Scanning. `cancel` stops it, and also identifies this scan to its relay task.
    Running { cancel: Arc<AtomicBool> },
    /// The last scan has ended (complete, or cut short by a cancel or stall).
    Done,
}

struct ScanState {
    tree: RwLock<layout::DirTree>,
    version: AtomicU64,
    phase: Mutex<ScanPhase>,
    /// The canonical path being (or last) scanned.
    root: Mutex<PathBuf>,
    cross_filesystems: bool,
}

/// Encoded messages are `Bytes` so the snapshot and every receiver share one buffer.
struct LayoutBroadcast {
    tx: broadcast::Sender<Bytes>,
    last: Mutex<Option<Bytes>>,
    notify: Notify,
}

struct AppState {
    scan: ScanState,
    view: Mutex<ViewConfig>,
    layout: LayoutBroadcast,
    /// Open WebSocket connections (see `Connection`); the layout loop idles at zero and the server
    /// exits after a grace period once the last one closes.
    connections: AtomicU64,
    /// Connections ever opened. A shutdown timer only fires if this hasn't moved since it was armed.
    connects: AtomicU64,
    shutdown: watch::Sender<bool>,
}

impl AppState {
    fn invalidate_layout(&self) {
        // Release so the layout loop's Acquire load of `version` also observes the tree writes
        // that happened before this call.
        self.scan.version.fetch_add(1, Ordering::Release);
        self.layout.notify.notify_one();
    }

    /// Send every tab to the volume picker and wait for a path, unless a scan is running.
    fn await_path(&self) {
        let mut phase = self.scan.phase.lock();
        if matches!(*phase, ScanPhase::Running { .. }) {
            return;
        }
        *phase = ScanPhase::AwaitingPath;
        drop(phase);
        let _ = self.layout.tx.send(wire::ServerMessage::PickerMode.encode().into());
    }

    /// Mutate the view config under its lock, then trigger a relayout if anything changed.
    /// Collapses the otherwise repeated lock-mutate-invalidate dance shared by every view-changing
    /// client message (the client sends all three filter fields on any filter edit, for example).
    fn update_view(&self, f: impl FnOnce(&mut ViewConfig)) {
        let mut view = self.view.lock();
        let before = view.clone();
        f(&mut view);
        let changed = *view != before;
        drop(view);
        if changed {
            self.invalidate_layout();
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_server(args))
}

async fn run_server(args: Args) -> Result<()> {
    let Args {
        path,
        all: cross_filesystems,
        port,
        no_open,
        wait,
    } = args;
    let (layout_tx, _) = broadcast::channel::<Bytes>(64);
    let picker_mode = path.is_none();
    // Canonicalize once, here at the boundary, and fail fast: a mistyped path is an error on the
    // command line, not a browser tab that opens on the volume picker.
    let (phase, scan_root) = match path {
        None => (ScanPhase::AwaitingPath, PathBuf::new()),
        Some(path) => {
            let root = std::fs::canonicalize(&path).with_context(|| format!("can't scan {}", path.display()))?;
            (ScanPhase::AwaitingStart, root)
        }
    };

    let state = Arc::new(AppState {
        scan: ScanState {
            tree: RwLock::new(layout::DirTree::new()),
            version: AtomicU64::new(0),
            phase: Mutex::new(phase),
            root: Mutex::new(scan_root),
            cross_filesystems,
        },
        view: Mutex::new(ViewConfig {
            viewport: (0.0, 0.0),
            view_root: None,
            max_depth: wire::DEFAULT_DEPTH,
            color_mode: wire::COLOR_MODE_TYPE,
            filter: FilterConfig::default(),
        }),
        layout: LayoutBroadcast {
            tx: layout_tx,
            last: Mutex::new(None),
            notify: Notify::new(),
        },
        connections: AtomicU64::new(0),
        connects: AtomicU64::new(0),
        shutdown: watch::channel(false).0,
    });

    tokio::task::spawn(layout_loop(Arc::clone(&state)));

    if !wait {
        start_scan(&state, None, |phase| matches!(phase, ScanPhase::AwaitingStart));
    }

    if Assets::get("index.html").is_none() {
        eprintln!("Warning: frontend bundle not embedded — run `trunk build` before building; the GUI will not load.");
    }

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/start", get(start_handler))
        .route("/volumes", get(volumes_handler))
        .with_state(Arc::clone(&state))
        .fallback(static_handler)
        .layer(CompressionLayer::new());

    // Bind loopback only: the UI is always pointed at localhost, and the server exposes full
    // directory listings plus arbitrary-path scans with no auth — there's no reason to accept
    // connections from other hosts on the network.
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let actual_port = listener.local_addr()?.port();
    eprintln!("Listening on http://localhost:{actual_port}");

    if !no_open {
        let url = if picker_mode {
            format!("http://localhost:{actual_port}/?picker")
        } else if wait {
            format!("http://localhost:{actual_port}/?wait")
        } else {
            format!("http://localhost:{actual_port}")
        };
        if let Err(e) = open::that(&url) {
            eprintln!("Failed to open browser: {e}");
        }
    }

    let shutdown = Arc::clone(&state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await?;
    Ok(())
}

/// Recompute and broadcast the layout whenever the tree or view changes: at most every ~45ms while
/// a scan streams in, immediately otherwise, and not at all with no clients connected.
async fn layout_loop(state: Arc<AppState>) {
    let mut last_version = 0u64;
    let mut last_compute = Instant::now();

    loop {
        tokio::select! {
            _ = state.layout.notify.notified() => {},
            _ = tokio::time::sleep(Duration::from_millis(50)) => {},
        }

        let current_version = state.scan.version.load(Ordering::Acquire);
        if current_version == last_version || state.connections.load(Ordering::Relaxed) == 0 {
            continue;
        }
        // No layouts while waiting for a path or for Start: the tree may still hold an earlier
        // scan, and a layout would bring its treemap back over the picker.
        let scan_done = match *state.scan.phase.lock() {
            ScanPhase::Running { .. } => false,
            ScanPhase::Done => true,
            ScanPhase::AwaitingPath | ScanPhase::AwaitingStart => continue,
        };
        if !scan_done && last_compute.elapsed() < Duration::from_millis(45) {
            continue;
        }

        let view = state.view.lock().clone();
        let (vw, vh) = view.viewport;
        if vw <= 0.0 || vh <= 0.0 {
            continue;
        }

        let tree = state.scan.tree.read();
        let Some(root_id) = tree.root_id else {
            continue;
        };
        let view_root = view
            .view_root
            .filter(|id| tree.nodes.contains_key(id))
            .unwrap_or(root_id);

        let config = LayoutConfig {
            max_depth: view.max_depth,
            color_mode: view.color_mode,
            filter: view.filter,
            mtime_range: tree.mtime_range,
        };
        let rects = layout::compute_layout(&tree, view_root, vw, vh, &config);
        let breadcrumb = tree.breadcrumb(view_root);
        let root_size = tree.recursive_sizes.get(&view_root).copied().unwrap_or(0);
        let dir_count = tree.nodes.len() as u32;
        drop(tree);

        let message = Bytes::from(
            wire::ServerMessage::Layout(wire::LayoutPayload {
                root_size,
                dir_count,
                scan_done,
                breadcrumb,
                rects,
            })
            .encode(),
        );
        *state.layout.last.lock() = Some(message.clone());
        let _ = state.layout.tx.send(message);

        last_version = current_version;
        last_compute = Instant::now();
    }
}

async fn shutdown_signal(state: Arc<AppState>) {
    let mut rx = state.shutdown.subscribe();
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = terminate => {},
        _ = tokio::signal::ctrl_c() => {},
        _ = rx.changed() => {},
    }
    // Signal all WS handlers to close
    let _ = state.shutdown.send(true);
    // Stop an in-flight scan too. It runs on the blocking pool, which the runtime waits on at
    // exit, so without this a Ctrl+C during a large scan hangs until the whole walk finishes.
    if let ScanPhase::Running { cancel } = &*state.scan.phase.lock() {
        cancel.store(true, Ordering::Relaxed);
    }
}

/// Start scanning, from `root` if given or else the current root, if `allowed` accepts the current
/// phase. The check and the move to `Running` happen under one lock, so two requests can't both
/// start a scan.
fn start_scan(state: &Arc<AppState>, root: Option<PathBuf>, allowed: impl FnOnce(&ScanPhase) -> bool) {
    let cancel = Arc::new(AtomicBool::new(false));
    let path = {
        let mut phase = state.scan.phase.lock();
        if !allowed(&phase) {
            return;
        }
        *phase = ScanPhase::Running {
            cancel: Arc::clone(&cancel),
        };
        let mut current_root = state.scan.root.lock();
        if let Some(root) = root {
            *current_root = root;
        }
        current_root.clone()
    };

    let (tx, rx) = std::sync::mpsc::channel::<ScanEvent>();
    let relay_state = Arc::clone(state);
    let relay_cancel = Arc::clone(&cancel);
    tokio::task::spawn_blocking(move || relay_scan(&relay_state, &rx, &relay_cancel));

    let cross_filesystems = state.scan.cross_filesystems;
    tokio::task::spawn_blocking(move || {
        // On failure the scanner drops `tx` without sending anything; the relay takes it from there.
        if let Err(e) = scanner::scan::scan_cancellable(&path, cross_filesystems, tx, cancel) {
            eprintln!("Scan error: {e:#}");
        }
    });
}

/// Apply one scan's events to the shared tree until it ends, then end its phase: `Done` at
/// `ScanDone`, or, if the scanner stops without one, back to the picker when it never started (it
/// couldn't open the root) and `Done` with what it found when it did (it panicked).
fn relay_scan(state: &AppState, rx: &std::sync::mpsc::Receiver<ScanEvent>, cancel: &Arc<AtomicBool>) {
    // Only this scan's relay may end its phase.
    let end_phase = |next: ScanPhase| {
        let mut phase = state.scan.phase.lock();
        let current = matches!(&*phase, ScanPhase::Running { cancel: running } if Arc::ptr_eq(running, cancel));
        if current {
            *phase = next;
        }
        current
    };

    // Reused across iterations so the per-batch drain doesn't reallocate each wakeup.
    let mut events = Vec::new();
    let mut started = false;
    let mut done = false;
    while !done && let Ok(event) = rx.recv() {
        events.clear();
        events.push(event);
        events.extend(rx.try_iter());

        let mut tree = state.scan.tree.write();
        for event in events.drain(..) {
            match &event {
                ScanEvent::ScanStart { path } => {
                    started = true;
                    state.view.lock().view_root = None;
                    let _ = state
                        .layout
                        .tx
                        .send(wire::ServerMessage::ScanStart { path: path.clone() }.encode().into());
                }
                ScanEvent::ScanDone => done = true,
                _ => {}
            }
            tree.apply(event);
            // `ScanDone` is the last event to apply: a cancelled or stalled scan can leave a
            // worker that is still inside a directory, and what it sends afterwards would change
            // a result already shown as finished. End the phase under the tree lock, so the
            // layout loop never sees the final tree still marked as running.
            if done {
                end_phase(ScanPhase::Done);
                break;
            }
        }
        drop(tree);
        state.invalidate_layout();
    }

    if done {
        return;
    }
    if started {
        state.scan.tree.write().apply(ScanEvent::ScanDone);
        if end_phase(ScanPhase::Done) {
            state.invalidate_layout();
        }
    } else if end_phase(ScanPhase::AwaitingPath) {
        let _ = state.layout.tx.send(wire::ServerMessage::PickerMode.encode().into());
    }
}

/// The trunk-built frontend bundle (`index.html`, wasm, js, css), baked into the binary at
/// compile time. In release builds the files are embedded; in debug builds `rust-embed` reads
/// them from `dist/` on disk, so `trunk watch` hot-reloads during development.
#[derive(RustEmbed)]
#[folder = "dist"]
// `allow_missing` lets the crate compile when `dist/` hasn't been built yet — a fresh clone,
// `cargo clippy`/`cargo test`, or rust-analyzer, none of which run `trunk build`. The bundle is
// only needed for a working GUI, which the build/publish steps produce (trunk build first);
// `run_server` warns at startup if it wasn't embedded.
#[allow_missing = true]
struct Assets;

/// Serve the embedded frontend. Unknown paths 404, matching the previous `ServeDir` behavior.
async fn static_handler(uri: Uri) -> Response {
    let path = match uri.path().trim_start_matches('/') {
        "" => "index.html",
        p => p,
    };
    match Assets::get(path) {
        Some(content) => {
            // Content-type is baked in at compile time by rust-embed's `mime-guess` feature.
            let mime = content.metadata.mimetype().to_owned();
            ([(header::CONTENT_TYPE, mime)], content.data).into_response()
        }
        None => (StatusCode::NOT_FOUND, "Not Found").into_response(),
    }
}

async fn start_handler(axum::extract::State(state): axum::extract::State<Arc<AppState>>) -> &'static str {
    start_scan(&state, None, |phase| matches!(phase, ScanPhase::AwaitingStart));
    "ok"
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> axum::response::Response {
    // Reject cross-origin upgrades. Binding to loopback stops remote hosts, but the server has no
    // auth and exposes the filesystem, so a malicious page in the user's own browser could
    // otherwise connect to ws://127.0.0.1 and drive the protocol (cross-origin WS / DNS rebinding).
    if !origin_allowed(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin WebSocket rejected").into_response();
    }
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// Allow an upgrade with no `Origin` (non-browser clients never send one) or a loopback `Origin`.
/// Browsers always send `Origin` on a WS handshake, so this rejects the cross-origin browser case
/// without affecting local tooling.
fn origin_allowed(headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => origin.to_str().map(origin_is_loopback).unwrap_or(false),
    }
}

fn origin_is_loopback(origin: &str) -> bool {
    let rest = origin.split_once("://").map_or(origin, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(after) = authority.strip_prefix('[') {
        after.split(']').next().unwrap_or("") // IPv6 literal: [host]:port
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    // Compare the host exactly: a prefix test on "127." would also accept a hostname such as
    // `127.0.0.1.attacker.example`, which an attacker controls and can point at their own page.
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// True only if `name` is a single, non-traversing path component (no separators, not `.`/`..`,
/// not absolute). Used to keep client-supplied reveal targets inside their resolved directory.
fn is_safe_component(name: &str) -> bool {
    let mut components = std::path::Path::new(name).components();
    matches!(components.next(), Some(std::path::Component::Normal(_))) && components.next().is_none()
}

/// Holds one count in `AppState::connections` for the life of a WebSocket handler, whichever way it
/// exits. Dropping the last one arms the auto-shutdown timer.
struct Connection(Arc<AppState>);

impl Connection {
    fn open(state: &Arc<AppState>) -> Self {
        state.connections.fetch_add(1, Ordering::Relaxed);
        state.connects.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(state))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if self.0.connections.fetch_sub(1, Ordering::Relaxed) != 1 {
            return;
        }
        // A handler dropped while the runtime itself shuts down has no timer to arm.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let state = Arc::clone(&self.0);
        let armed_at = state.connects.load(Ordering::Relaxed);
        runtime.spawn(async move {
            tokio::time::sleep(SHUTDOWN_GRACE).await;
            // Shut down only if nobody connected during the grace period. Checking just the count
            // would let an earlier disconnect's timer fire right after a later reconnect-and-leave
            // (two quick reloads), well inside that second disconnect's grace period.
            if state.connects.load(Ordering::Relaxed) == armed_at {
                let _ = state.shutdown.send(true);
            }
        });
    }
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    // Held to the end of the handler, including the early returns when a send fails.
    let _connection = Connection::open(&state);

    // Subscribe before reading the snapshot: a layout produced between the snapshot read and the
    // subscribe would otherwise be lost to this client (absent from the snapshot, broadcast before
    // we were listening) — and if it were the final one, the client would be stuck on a stale view.
    // A layout that lands in the overlap is merely delivered twice, which the client tolerates.
    let mut layout_rx = state.layout.tx.subscribe();
    let mut shutdown_rx = state.shutdown.subscribe();

    // A new client starts on the picker while the server waits for a path, else on the latest
    // layout (a reload while a previous scan's result is still current).
    let awaiting_path = matches!(*state.scan.phase.lock(), ScanPhase::AwaitingPath);
    let initial = if awaiting_path {
        Some(wire::ServerMessage::PickerMode.encode().into())
    } else {
        state.layout.last.lock().clone()
    };
    if let Some(message) = initial
        && socket.send(Message::Binary(message)).await.is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Binary(data))) => {
                        handle_client_message(&state, &data);
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
            result = layout_rx.recv() => {
                match result {
                    Ok(message) => {
                        if socket.send(Message::Binary(message)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
            _ = shutdown_rx.changed() => break,
        }
    }
}

/// Reveal directory `dir_id`, or the file `name` inside it, in the platform file manager.
fn reveal(state: &AppState, dir_id: u64, name: Option<&str>) {
    let root = state.scan.root.lock().clone();
    let dir = state.scan.tree.read().full_path(dir_id, &root);
    if let Some(mut path) = dir {
        if let Some(name) = name {
            path.push(name);
        }
        reveal_in_file_manager(&path);
    }
}

fn reveal_in_file_manager(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open")
            .arg(path.parent().unwrap_or(path))
            .spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg("/select,").arg(path).spawn();
    }
}

fn handle_client_message(state: &Arc<AppState>, data: &[u8]) {
    let Some(msg) = ClientMessage::decode(data) else { return };
    match msg {
        ClientMessage::Viewport { width, height } => {
            state.update_view(|v| v.viewport = (width as f64, height as f64));
        }
        ClientMessage::Navigate { id } => state.update_view(|v| v.view_root = Some(id)),
        ClientMessage::RevealDir { id } => reveal(state, id, None),
        ClientMessage::RevealFile { dir_id, name } => {
            // `name` is attacker-controllable over the WS; only join a single, non-traversing
            // component so it can't escape the resolved directory with `..` or an absolute path.
            if is_safe_component(&name) {
                reveal(state, dir_id, Some(&name));
            }
        }
        ClientMessage::Rescan => start_scan(state, None, |phase| matches!(phase, ScanPhase::Done)),
        ClientMessage::SetDepth { depth } => state.update_view(|v| v.max_depth = depth),
        ClientMessage::ColorMode { mode } => state.update_view(|v| v.color_mode = mode),
        ClientMessage::FilterExt { extensions } => state.update_view(|v| v.filter.extensions = extensions),
        ClientMessage::FilterSize { min, max } => state.update_view(|v| {
            v.filter.min_size = min;
            v.filter.max_size = max;
        }),
        ClientMessage::FilterName { pattern } => state.update_view(|v| v.filter.name_pattern = pattern),
        ClientMessage::ClearFilter => state.update_view(|v| v.filter = FilterConfig::default()),
        ClientMessage::ScanPath { path } => {
            // Only from the picker, or once the current scan is done (never mid-scan).
            let allowed = |phase: &ScanPhase| matches!(phase, ScanPhase::AwaitingPath | ScanPhase::Done);
            if !allowed(&state.scan.phase.lock()) {
                return;
            }
            // Canonicalize once, here, and don't start a scan that can't succeed: the page has
            // already left the picker, so send it back there.
            match std::fs::canonicalize(&path) {
                Ok(root) => start_scan(state, Some(root), allowed),
                Err(e) => {
                    eprintln!("Can't scan {path}: {e}");
                    state.await_path();
                }
            }
        }
    }
}

async fn volumes_handler() -> Json<Vec<wire::VolumeInfo>> {
    Json(
        tokio::task::spawn_blocking(scanner::volumes::list_volumes)
            .await
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_origins_are_allowed() {
        for origin in [
            "http://localhost:8080",
            "http://LOCALHOST",
            "http://127.0.0.1:3000",
            "http://127.1.2.3",
            "http://[::1]:8080",
            "http://[::ffff:127.0.0.1]",
        ] {
            assert!(origin_is_loopback(origin), "{origin} should be allowed");
        }
    }

    #[test]
    fn non_loopback_origins_are_rejected() {
        for origin in [
            "http://127.0.0.1.attacker.example",
            "http://127.0.0.1.attacker.example:8080",
            "https://localhost.attacker.example",
            "http://example.com",
            "http://192.168.1.10:8080",
            "http://[::2]",
            "null",
            "",
        ] {
            assert!(!origin_is_loopback(origin), "{origin} should be rejected");
        }
    }
}
