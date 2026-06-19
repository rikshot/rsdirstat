#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use clap::Parser;
use parking_lot::{Mutex, RwLock};
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast, watch};
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;

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
#[command(name = "rsdirstat-server", about = "Interactive treemap disk usage server")]
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

#[derive(Clone)]
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
    root: Mutex<PathBuf>,
    cross_filesystems: bool,
}

struct LayoutBroadcast {
    tx: broadcast::Sender<Vec<u8>>,
    last: Mutex<Option<Vec<u8>>>,
    notify: Notify,
}

struct ConnectionTracker {
    count: AtomicU64,
}

struct AppState {
    scan: ScanState,
    view: Mutex<ViewConfig>,
    layout: LayoutBroadcast,
    connections: ConnectionTracker,
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

    /// Mutate the view config under its lock, then trigger a relayout. Collapses the otherwise
    /// repeated lock-mutate-invalidate dance shared by every view-changing client message.
    fn update_view(&self, f: impl FnOnce(&mut ViewConfig)) {
        f(&mut self.view.lock());
        self.invalidate_layout();
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_server(args.path, args.all, args.port, args.no_open, args.wait))
}

async fn run_server(
    path: Option<PathBuf>,
    cross_filesystems: bool,
    port: u16,
    no_open: bool,
    wait: bool,
) -> Result<()> {
    let (layout_tx, _) = broadcast::channel::<Vec<u8>>(64);
    let picker_mode = path.is_none();
    let scan_root = path.map(|p| std::fs::canonicalize(&p).unwrap_or(p)).unwrap_or_default();

    let state = Arc::new(AppState {
        scan: ScanState {
            tree: RwLock::new(layout::DirTree::new()),
            version: AtomicU64::new(0),
            done: AtomicBool::new(false),
            started: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            root: Mutex::new(scan_root),
            cross_filesystems,
        },
        view: Mutex::new(ViewConfig {
            viewport: (0.0, 0.0),
            view_root: None,
            max_depth: 5,
            color_mode: 0,
            filter: FilterConfig::default(),
        }),
        layout: LayoutBroadcast {
            tx: layout_tx,
            last: Mutex::new(None),
            notify: Notify::new(),
        },
        connections: ConnectionTracker {
            count: AtomicU64::new(0),
        },
        start: Notify::new(),
        shutdown: watch::channel(false).0,
        picker_mode,
    });

    let scan_state = Arc::clone(&state);
    tokio::task::spawn(async move {
        scan_state.start.notified().await;
        start_scan(&scan_state);
    });

    let layout_state = Arc::clone(&state);
    tokio::task::spawn(async move {
        let mut last_version = 0u64;
        let mut last_compute = Instant::now();

        loop {
            tokio::select! {
                _ = layout_state.layout.notify.notified() => {},
                _ = tokio::time::sleep(Duration::from_millis(50)) => {},
            }

            let current_version = layout_state.scan.version.load(Ordering::Acquire);
            let scan_done = layout_state.scan.done.load(Ordering::Acquire);

            if current_version == last_version || layout_state.connections.count.load(Ordering::Relaxed) == 0 {
                continue;
            }
            if !scan_done && last_compute.elapsed() < Duration::from_millis(45) {
                continue;
            }

            let view = layout_state.view.lock().clone();
            let (vw, vh) = view.viewport;
            if vw <= 0.0 || vh <= 0.0 {
                continue;
            }

            let tree = layout_state.scan.tree.read();
            let root_id = match tree.root_id {
                Some(id) => id,
                None => continue,
            };
            let mut view_root = view.view_root.unwrap_or(root_id);
            if !tree.nodes.contains_key(&view_root) {
                view_root = root_id;
            }

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

            let message = wire::ServerMessage::Layout(wire::LayoutPayload {
                root_size,
                dir_count,
                scan_done,
                breadcrumb,
                rects,
            })
            .encode();
            *layout_state.layout.last.lock() = Some(message.clone());
            let _ = layout_state.layout.tx.send(message);

            last_version = current_version;
            last_compute = Instant::now();
        }
    });

    if !picker_mode && !wait {
        state.start.notify_one();
    }

    let static_dir = find_static_dir();

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/start", get(start_handler))
        .route("/volumes", get(volumes_handler))
        .with_state(Arc::clone(&state))
        .fallback_service(ServeDir::new(&static_dir))
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

async fn shutdown_signal(state: Arc<AppState>) {
    let mut rx = state.shutdown.subscribe();
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = sigterm.recv() => {},
            _ = tokio::signal::ctrl_c() => {},
            _ = rx.changed() => {},
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = rx.changed() => {},
        }
    }
    // Signal all WS handlers to close
    let _ = state.shutdown.send(true);
}

fn start_scan(state: &Arc<AppState>) {
    // Supersede any in-flight scan: a new generation makes the previous relay task bail before
    // it writes more events into the shared tree.
    let generation = state.scan.generation.fetch_add(1, Ordering::Relaxed) + 1;
    state.scan.done.store(false, Ordering::Relaxed);
    state.scan.started.store(true, Ordering::Relaxed);
    let (tx, rx) = std::sync::mpsc::channel::<ScanEvent>();

    let relay_state = Arc::clone(state);
    tokio::task::spawn_blocking(move || {
        while let Ok(event) = rx.recv() {
            if relay_state.scan.generation.load(Ordering::Relaxed) != generation {
                break;
            }
            let mut events = vec![event];
            while let Ok(extra) = rx.try_recv() {
                events.push(extra);
            }

            let mut tree = relay_state.scan.tree.write();
            for event in events {
                match event {
                    ScanEvent::ScanStart { path } => {
                        tree.clear();
                        tree.scan_path = path.clone();
                        relay_state.view.lock().view_root = None;
                        relay_state.scan.done.store(false, Ordering::Relaxed);
                        let _ = relay_state
                            .layout
                            .tx
                            .send(wire::ServerMessage::ScanStart { path }.encode());
                    }
                    ScanEvent::Dir {
                        id,
                        parent,
                        name,
                        size,
                        mtime,
                    } => {
                        tree.insert_dir(id, parent, &name, size, mtime);
                    }
                    ScanEvent::File {
                        parent,
                        name,
                        size,
                        mtime,
                    } => {
                        tree.insert_file(parent, &name, size, mtime);
                    }
                    ScanEvent::ScanDone => {
                        tree.recompute_sizes();
                        relay_state.scan.done.store(true, Ordering::Relaxed);
                    }
                }
            }
            drop(tree);
            relay_state.invalidate_layout();
        }
    });

    let path = state.scan.root.lock().clone();
    let cross_filesystems = state.scan.cross_filesystems;
    tokio::task::spawn_blocking(move || {
        if let Err(e) = scanner::scan::scan(&path, cross_filesystems, tx) {
            eprintln!("Scan error: {e}");
        }
    });
}

/// Locate the trunk-built frontend bundle. In a dev checkout it lives in the wasm crate's
/// `dist/` (produced by `trunk build`); a deployed binary expects a `dist/` next to it.
fn find_static_dir() -> PathBuf {
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../wasm/dist");
    if dev.is_dir() {
        return dev;
    }
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or(std::path::Path::new(".")).join("dist");
        if dir.is_dir() {
            return dir;
        }
    }
    eprintln!("Warning: frontend 'dist/' not found; run `trunk build` in crates/wasm");
    PathBuf::from(".")
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
    state.connections.count.fetch_add(1, Ordering::Relaxed);

    // Subscribe before reading the snapshot: a layout produced between the snapshot read and the
    // subscribe would otherwise be lost to this client (absent from the snapshot, broadcast before
    // we were listening) — and if it were the final one, the client would be stuck on a stale view.
    // A layout that lands in the overlap is merely delivered twice, which the client tolerates.
    let mut layout_rx = state.layout.tx.subscribe();
    let mut shutdown_rx = state.shutdown.subscribe();

    let last = state.layout.last.lock().clone();
    if let Some(message) = last {
        if socket.send(Message::Binary(message.into())).await.is_err() {
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
                        if socket.send(Message::Binary(message.into())).await.is_err() {
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

    if state.connections.count.fetch_sub(1, Ordering::Relaxed) == 1 {
        let server = Arc::clone(&state);
        tokio::spawn(async move {
            tokio::time::sleep(SHUTDOWN_GRACE).await;
            // This timer only spawns when the last connection drops, so a connection always existed.
            if server.connections.count.load(Ordering::Relaxed) == 0 {
                let _ = server.shutdown.send(true);
            }
        });
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
        ClientMessage::RevealDir { id } => {
            let root = state.scan.root.lock().clone();
            let tree = state.scan.tree.read();
            if let Some(path) = tree.full_path(id, &root) {
                drop(tree);
                reveal_in_file_manager(&path);
            }
        }
        ClientMessage::RevealFile { dir_id, name } => {
            // `name` is attacker-controllable over the WS; only join a single, non-traversing
            // component so it can't escape the resolved directory with `..` or an absolute path.
            if is_safe_component(&name) {
                let root = state.scan.root.lock().clone();
                let tree = state.scan.tree.read();
                if let Some(path) = tree.full_path(dir_id, &root).map(|p| p.join(&name)) {
                    drop(tree);
                    reveal_in_file_manager(&path);
                }
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

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\u{20}' => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

async fn volumes_handler(
    axum::extract::State(_state): axum::extract::State<Arc<AppState>>,
) -> ([(axum::http::header::HeaderName, &'static str); 1], String) {
    let volumes = tokio::task::spawn_blocking(scanner::volumes::list_volumes)
        .await
        .unwrap_or_default();
    let entries: Vec<String> = volumes
        .iter()
        .map(|v| {
            format!(
                r#"{{"name":"{}","mountPoint":"{}","totalBytes":{},"usedBytes":{},"fsType":"{}"}}"#,
                json_escape(&v.name),
                json_escape(&v.mount_point),
                v.total_bytes,
                v.used_bytes,
                json_escape(&v.fs_type),
            )
        })
        .collect();
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        format!("[{}]", entries.join(",")),
    )
}
