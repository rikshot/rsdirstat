#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
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
#[command(name = "rsdirstat", about = "Interactive treemap disk usage server")]
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

struct ScanState {
    tree: RwLock<layout::DirTree>,
    version: AtomicU64,
    done: AtomicBool,
    started: AtomicBool,
    /// Bumped each time a scan starts; lets a superseded scan's relay task detect it has been
    /// replaced and stop writing into the shared tree.
    generation: AtomicU64,
    /// Cancel flag for the in-flight scan's worker threads, replaced on each new scan. The
    /// generation only stops the relay; this stops the OS-level filesystem walk too.
    cancel: Mutex<Arc<AtomicBool>>,
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
    /// Open WebSocket connections; the layout loop idles at zero and the server exits after a
    /// grace period once the last one closes.
    connections: AtomicU64,
    start: Notify,
    shutdown: watch::Sender<bool>,
    picker_mode: bool,
}

impl AppState {
    fn invalidate_layout(&self) {
        // Release so the layout loop's Acquire load of `version` also observes the tree and
        // `done` writes that happened before this call.
        self.scan.version.fetch_add(1, Ordering::Release);
        self.layout.notify.notify_one();
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
    let scan_root = path.map(|p| std::fs::canonicalize(&p).unwrap_or(p)).unwrap_or_default();

    let state = Arc::new(AppState {
        scan: ScanState {
            tree: RwLock::new(layout::DirTree::new()),
            version: AtomicU64::new(0),
            done: AtomicBool::new(false),
            started: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
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
        start: Notify::new(),
        shutdown: watch::channel(false).0,
        picker_mode,
    });

    let scan_state = Arc::clone(&state);
    tokio::task::spawn(async move {
        scan_state.start.notified().await;
        start_scan(&scan_state);
    });

    tokio::task::spawn(layout_loop(Arc::clone(&state)));

    if !picker_mode && !wait {
        state.start.notify_one();
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
        let scan_done = state.scan.done.load(Ordering::Acquire);

        if current_version == last_version || state.connections.load(Ordering::Relaxed) == 0 {
            continue;
        }
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
    state.scan.cancel.lock().store(true, Ordering::Relaxed);
}

fn start_scan(state: &Arc<AppState>) {
    // Supersede any in-flight scan: a new generation makes the previous relay task bail before
    // it writes more events into the shared tree.
    let generation = state.scan.generation.fetch_add(1, Ordering::Relaxed) + 1;
    state.scan.done.store(false, Ordering::Relaxed);
    state.scan.started.store(true, Ordering::Relaxed);

    // Trip the previous scan's cancel flag so its worker threads stop walking the filesystem (the
    // generation bump only stops the relay), then install a fresh flag for this scan.
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut slot = state.scan.cancel.lock();
        slot.store(true, Ordering::Relaxed);
        *slot = Arc::clone(&cancel);
    }

    let (tx, rx) = std::sync::mpsc::channel::<ScanEvent>();

    let relay_state = Arc::clone(state);
    tokio::task::spawn_blocking(move || {
        // Reused across iterations so the per-batch drain doesn't reallocate each wakeup.
        let mut events = Vec::new();
        while let Ok(event) = rx.recv() {
            if relay_state.scan.generation.load(Ordering::Relaxed) != generation {
                break;
            }
            events.clear();
            events.push(event);
            while let Ok(extra) = rx.try_recv() {
                events.push(extra);
            }

            let mut tree = relay_state.scan.tree.write();
            for event in events.drain(..) {
                let start_path = match &event {
                    ScanEvent::ScanStart { path } => Some(path.clone()),
                    _ => None,
                };
                let done = matches!(event, ScanEvent::ScanDone);
                tree.apply(event);
                if let Some(path) = start_path {
                    relay_state.view.lock().view_root = None;
                    relay_state.scan.done.store(false, Ordering::Relaxed);
                    let _ = relay_state
                        .layout
                        .tx
                        .send(wire::ServerMessage::ScanStart { path }.encode().into());
                }
                if done {
                    relay_state.scan.done.store(true, Ordering::Relaxed);
                }
            }
            drop(tree);
            relay_state.invalidate_layout();
        }
    });

    let path = state.scan.root.lock().clone();
    let cross_filesystems = state.scan.cross_filesystems;
    tokio::task::spawn_blocking(move || {
        if let Err(e) = scanner::scan::scan_cancellable(&path, cross_filesystems, tx, cancel) {
            eprintln!("Scan error: {e}");
        }
    });
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
    state.start.notify_one();
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
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// True only if `name` is a single, non-traversing path component (no separators, not `.`/`..`,
/// not absolute). Used to keep client-supplied reveal targets inside their resolved directory.
fn is_safe_component(name: &str) -> bool {
    let mut components = std::path::Path::new(name).components();
    matches!(components.next(), Some(std::path::Component::Normal(_))) && components.next().is_none()
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    state.connections.fetch_add(1, Ordering::Relaxed);

    // Subscribe before reading the snapshot: a layout produced between the snapshot read and the
    // subscribe would otherwise be lost to this client (absent from the snapshot, broadcast before
    // we were listening) — and if it were the final one, the client would be stuck on a stale view.
    // A layout that lands in the overlap is merely delivered twice, which the client tolerates.
    let mut layout_rx = state.layout.tx.subscribe();
    let mut shutdown_rx = state.shutdown.subscribe();

    let last = state.layout.last.lock().clone();
    if let Some(message) = last {
        if socket.send(Message::Binary(message)).await.is_err() {
            return;
        }
    } else if state.picker_mode
        && !state.scan.started.load(Ordering::Relaxed)
        && socket
            .send(Message::Binary(wire::ServerMessage::PickerMode.encode().into()))
            .await
            .is_err()
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

    if state.connections.fetch_sub(1, Ordering::Relaxed) == 1 {
        let server = Arc::clone(&state);
        tokio::spawn(async move {
            tokio::time::sleep(SHUTDOWN_GRACE).await;
            // This timer only spawns when the last connection drops, so a connection always existed.
            if server.connections.load(Ordering::Relaxed) == 0 {
                let _ = server.shutdown.send(true);
            }
        });
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
        ClientMessage::Rescan => {
            if state.scan.done.load(Ordering::Relaxed) {
                start_scan(state);
            }
        }
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
            let can_start = !state.scan.started.load(Ordering::Relaxed) || state.scan.done.load(Ordering::Relaxed);
            if can_start {
                let scan_root = std::fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
                *state.scan.root.lock() = scan_root;
                start_scan(state);
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
