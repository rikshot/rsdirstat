#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

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
use tokio::sync::{broadcast, watch};
use tower_http::compression::CompressionLayer;

use rsdirstat_core::layout::{self, LayoutConfig};
use rsdirstat_core::scan::ScanEvent;
use rsdirstat_core::tree::{DirTree, FilterConfig};
use rsdirstat_protocol::{self as wire, ClientMessage};

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

/// One tab's view of the tree. Every tab has its own, so zooming or filtering in one leaves the
/// others as they were.
#[derive(Clone, PartialEq)]
struct ViewConfig {
    viewport: (f64, f64),
    view_root: Option<u64>,
    max_depth: u8,
    color_mode: u8,
    filter: FilterConfig,
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            viewport: (0.0, 0.0),
            view_root: None,
            max_depth: wire::DEFAULT_DEPTH,
            color_mode: wire::COLOR_MODE_TYPE,
            filter: FilterConfig::default(),
        }
    }
}

/// How often a tab gets a new layout while a scan streams in. Once it's done, a tab gets one as soon
/// as its view changes.
const SCAN_LAYOUT_INTERVAL: Duration = Duration::from_millis(45);

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
    tree: RwLock<DirTree>,
    phase: Mutex<ScanPhase>,
    /// The canonical path being (or last) scanned.
    root: Mutex<PathBuf>,
    cross_filesystems: bool,
}

/// A message every tab must see, in order: `ScanStart` or `PickerMode`. Encoded once, as `Bytes`,
/// so every tab shares the buffer.
#[derive(Clone)]
struct TabEvent {
    message: Bytes,
    /// A new scan started, so each tab's view goes back to the scan root.
    new_scan: bool,
}

struct AppState {
    scan: ScanState,
    events: broadcast::Sender<TabEvent>,
    /// Bumped whenever the tree changes. Every tab watches it and lays out its own view again; a
    /// tab that falls behind skips straight to the newest tree instead of a queue of stale layouts.
    tree_version: watch::Sender<u64>,
    /// Open WebSocket connections (see `Connection`); the server exits after a grace period once the
    /// last one closes.
    connections: AtomicU64,
    /// Connections ever opened. A shutdown timer only fires if this hasn't moved since it was armed.
    connects: AtomicU64,
    shutdown: watch::Sender<bool>,
}

impl AppState {
    fn tree_changed(&self) {
        self.tree_version.send_modify(|version| *version += 1);
    }

    fn send_event(&self, message: wire::ServerMessage) {
        let new_scan = matches!(message, wire::ServerMessage::ScanStart { .. });
        let _ = self.events.send(TabEvent {
            message: message.encode().into(),
            new_scan,
        });
    }

    /// Send every tab to the volume picker and wait for a path, unless a scan is running.
    fn await_path(&self) {
        let mut phase = self.scan.phase.lock();
        if matches!(*phase, ScanPhase::Running { .. }) {
            return;
        }
        *phase = ScanPhase::AwaitingPath;
        drop(phase);
        self.send_event(wire::ServerMessage::PickerMode);
    }
}

/// How long shutdown waits for blocking tasks before exiting anyway (see `main`).
const BLOCKING_TASK_GRACE: Duration = Duration::from_secs(2);

fn main() -> Result<()> {
    let args = Args::parse();
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(run_server(args));
    // Dropping the runtime would wait for every blocking task, and one can be stuck in a syscall on
    // a dead network mount (a scan of it, or `statvfs` for /volumes) for as long as the mount takes
    // to time out, if it ever does. A healthy scan stops within ~200ms of being cancelled, so give
    // the tasks a moment, then exit regardless.
    rt.shutdown_timeout(BLOCKING_TASK_GRACE);
    result
}

async fn run_server(args: Args) -> Result<()> {
    let Args {
        path,
        all: cross_filesystems,
        port,
        no_open,
        wait,
    } = args;
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
            tree: RwLock::new(DirTree::new()),
            phase: Mutex::new(phase),
            root: Mutex::new(scan_root),
            cross_filesystems,
        },
        // Events are rare (one per scan or picker switch), so a small buffer never lags.
        events: broadcast::channel(16).0,
        tree_version: watch::channel(0).0,
        connections: AtomicU64::new(0),
        connects: AtomicU64::new(0),
        shutdown: watch::channel(false).0,
    });

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
        .layer(CompressionLayer::new())
        .layer(axum::middleware::from_fn(require_loopback_host));

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

/// A tab's layout of the tree for `view`, encoded. `None` while there is nothing to show: waiting
/// for a path or for Start (the tree may still hold an earlier scan, and a layout would bring its
/// treemap back over the picker), before the tab's first viewport, or before a scan's root arrives.
fn layout_for(state: &AppState, view: &ViewConfig) -> Option<Bytes> {
    let scan_done = match *state.scan.phase.lock() {
        ScanPhase::Running { .. } => false,
        ScanPhase::Done => true,
        ScanPhase::AwaitingPath | ScanPhase::AwaitingStart => return None,
    };
    let (vw, vh) = view.viewport;
    if vw <= 0.0 || vh <= 0.0 {
        return None;
    }

    let tree = state.scan.tree.read();
    let root_id = tree.root_id?;
    let view_root = view
        .view_root
        .filter(|id| tree.nodes.contains_key(id))
        .unwrap_or(root_id);
    let config = LayoutConfig {
        max_depth: view.max_depth,
        color_mode: view.color_mode,
        filter: view.filter.clone(),
        mtime_range: tree.mtime_range,
    };
    let rects = layout::compute_layout(&tree, view_root, vw, vh, &config);
    let breadcrumb = tree.breadcrumb(view_root);
    let root_size = tree.nodes.get(&view_root).map_or(0, |node| node.size);
    let dir_count = tree.nodes.len() as u32;
    drop(tree);

    let payload = wire::LayoutPayload {
        root_size,
        dir_count,
        scan_done,
        breadcrumb,
        rects,
    };
    Some(wire::ServerMessage::Layout(payload).encode().into())
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
                    state.send_event(wire::ServerMessage::ScanStart { path: path.clone() });
                }
                ScanEvent::ScanDone => done = true,
                _ => {}
            }
            tree.apply(event);
            // `ScanDone` is the last event to apply: a cancelled or stalled scan can leave a
            // worker that is still inside a directory, and what it sends afterwards would change
            // a result already shown as finished. End the phase under the tree lock, so no tab
            // lays out the final tree still marked as running.
            if done {
                end_phase(ScanPhase::Done);
                break;
            }
        }
        drop(tree);
        state.tree_changed();
    }

    if done {
        return;
    }
    if started {
        state.scan.tree.write().apply(ScanEvent::ScanDone);
        if end_phase(ScanPhase::Done) {
            state.tree_changed();
        }
    } else if end_phase(ScanPhase::AwaitingPath) {
        state.send_event(wire::ServerMessage::PickerMode);
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

async fn start_handler(
    headers: HeaderMap,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Response {
    if !api_request_allowed(&headers) {
        return forbidden();
    }
    start_scan(&state, None, |phase| matches!(phase, ScanPhase::AwaitingStart));
    "ok".into_response()
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "request refused: not from this app's own page").into_response()
}

/// Refuse a request addressed to any host but this machine. The server binds to loopback, but DNS
/// rebinding can point an attacker's hostname at 127.0.0.1: the browser then treats the attacker's
/// page as same-origin with this server (no `Origin` on a same-origin GET), and only the `Host`
/// header still carries the attacker's name. Requests without one (not from a browser) pass.
async fn require_loopback_host(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let allowed = request
        .headers()
        .get(header::HOST)
        .is_none_or(|host| host.to_str().is_ok_and(authority_is_loopback));
    if !allowed {
        return (StatusCode::FORBIDDEN, "request refused: unexpected Host").into_response();
    }
    next.run(request).await
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> axum::response::Response {
    // Reject cross-origin upgrades. Binding to loopback stops remote hosts, but the server has no
    // auth and exposes the filesystem, so a malicious page in the user's own browser could
    // otherwise connect to ws://127.0.0.1 and drive the protocol (cross-origin WS / DNS rebinding).
    if !api_request_allowed(&headers) {
        return forbidden();
    }
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// Whether a request to an API endpoint (`/ws`, `/start`, `/volumes`) comes from this app's own
/// page, a URL the user typed, or a non-browser client. Browsers label every request with
/// `Sec-Fetch-Site`, so a page on another site can't reach these even where CORS wouldn't stop it
/// (an `<img src=…/start>` carries no `Origin`); an `Origin`, when sent, must be loopback too.
fn api_request_allowed(headers: &HeaderMap) -> bool {
    let origin_ok = headers
        .get(header::ORIGIN)
        .is_none_or(|origin| origin.to_str().is_ok_and(origin_is_loopback));
    let site_ok = headers
        .get("sec-fetch-site")
        .is_none_or(|site| matches!(site.as_bytes(), b"same-origin" | b"none"));
    origin_ok && site_ok
}

fn origin_is_loopback(origin: &str) -> bool {
    let rest = origin.split_once("://").map_or(origin, |(_, r)| r);
    authority_is_loopback(rest.split(['/', '?', '#']).next().unwrap_or(""))
}

/// Whether `authority` (`host[:port]`, as in a `Host` header or an origin) names this machine:
/// `localhost` or a loopback IP.
fn authority_is_loopback(authority: &str) -> bool {
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

    let mut events = state.events.subscribe();
    let mut tree_changes = state.tree_version.subscribe();
    let mut shutdown_rx = state.shutdown.subscribe();

    // A new tab starts on the picker while the server waits for a path. Otherwise its first layout
    // follows the viewport it sends on connecting.
    let awaiting_path = matches!(*state.scan.phase.lock(), ScanPhase::AwaitingPath);
    if awaiting_path
        && socket
            .send(Message::Binary(wire::ServerMessage::PickerMode.encode().into()))
            .await
            .is_err()
    {
        return;
    }

    let mut view = ViewConfig::default();
    // Whether the tree or the view changed since the last layout sent.
    let mut stale = false;
    let mut last_layout = tokio::time::Instant::now();
    loop {
        // While a scan streams in, layouts are rate-limited; otherwise one goes out at once.
        let scanning = matches!(*state.scan.phase.lock(), ScanPhase::Running { .. });
        let layout_due = if scanning {
            last_layout + SCAN_LAYOUT_INTERVAL
        } else {
            tokio::time::Instant::now()
        };
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Binary(data))) => {
                        stale |= handle_client_message(&state, &mut view, &data);
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if !forward_event(&mut socket, &mut view, event).await {
                            break;
                        }
                        stale = true;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            changed = tree_changes.changed() => {
                if changed.is_err() {
                    break;
                }
                stale = true;
            }
            () = tokio::time::sleep_until(layout_due), if stale => {
                stale = false;
                last_layout = tokio::time::Instant::now();
                let Some(layout) = layout_for(&state, &view) else { continue };
                // Events and layouts travel separately, but an event sent before this layout was
                // computed must reach the page first: a new scan's `ScanStart` clears the page,
                // so arriving after a layout of the new tree it would wipe it. Send those first,
                // and if one started a new scan, lay out again instead of sending a layout that
                // may predate it.
                let mut new_scan = false;
                loop {
                    match events.try_recv() {
                        Ok(event) => {
                            new_scan |= event.new_scan;
                            if !forward_event(&mut socket, &mut view, event).await {
                                return;
                            }
                        }
                        Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                        Err(_) => break,
                    }
                }
                if new_scan {
                    stale = true;
                } else if socket.send(Message::Binary(layout)).await.is_err() {
                    break;
                }
            }
            _ = shutdown_rx.changed() => break,
        }
    }
}

/// Send `event` to the page, and reset the tab's view to the scan root if it starts a new scan.
/// Returns false once the socket is closed.
async fn forward_event(socket: &mut WebSocket, view: &mut ViewConfig, event: TabEvent) -> bool {
    if event.new_scan {
        view.view_root = None;
    }
    socket.send(Message::Binary(event.message)).await.is_ok()
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

/// Apply `change` to `view`, returning whether it changed anything. A page sends its toolbar's
/// values whenever one of them is edited, so most messages leave the view as it was.
fn change_view(view: &mut ViewConfig, change: impl FnOnce(&mut ViewConfig)) -> bool {
    let before = view.clone();
    change(view);
    *view != before
}

/// Act on one message from a tab. View changes apply to that tab's `view` only; returns whether it
/// changed, so the tab is laid out again.
fn handle_client_message(state: &Arc<AppState>, view: &mut ViewConfig, data: &[u8]) -> bool {
    let Some(msg) = ClientMessage::decode(data) else {
        return false;
    };
    match msg {
        ClientMessage::Viewport { width, height } => {
            return change_view(view, |v| v.viewport = (f64::from(width), f64::from(height)));
        }
        ClientMessage::Navigate { id } => return change_view(view, |v| v.view_root = Some(id)),
        ClientMessage::SetDepth { depth } => return change_view(view, |v| v.max_depth = depth),
        ClientMessage::ColorMode { mode } => return change_view(view, |v| v.color_mode = mode),
        ClientMessage::FilterExt { extensions } => return change_view(view, |v| v.filter.extensions = extensions),
        ClientMessage::FilterSize { min, max } => {
            return change_view(view, |v| {
                v.filter.min_size = min;
                v.filter.max_size = max;
            });
        }
        ClientMessage::FilterName { pattern } => return change_view(view, |v| v.filter.name_pattern = pattern),
        ClientMessage::ClearFilter => return change_view(view, |v| v.filter = FilterConfig::default()),
        ClientMessage::RevealDir { id } => reveal(state, id, None),
        ClientMessage::RevealFile { dir_id, name } => {
            // `name` is attacker-controllable over the WS; only join a single, non-traversing
            // component so it can't escape the resolved directory with `..` or an absolute path.
            if is_safe_component(&name) {
                reveal(state, dir_id, Some(&name));
            }
        }
        ClientMessage::Rescan => start_scan(state, None, |phase| matches!(phase, ScanPhase::Done)),
        ClientMessage::ScanPath { path } => {
            // Only from the picker, or once the current scan is done (never mid-scan).
            let allowed = |phase: &ScanPhase| matches!(phase, ScanPhase::AwaitingPath | ScanPhase::Done);
            if !allowed(&state.scan.phase.lock()) {
                return false;
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
    false
}

async fn volumes_handler(headers: HeaderMap) -> Response {
    if !api_request_allowed(&headers) {
        return forbidden();
    }
    let volumes = tokio::task::spawn_blocking(scanner::volumes::list_volumes)
        .await
        .unwrap_or_default();
    Json(volumes).into_response()
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
    fn host_headers_must_name_this_machine() {
        for host in ["localhost:8080", "LOCALHOST", "127.0.0.1:3000", "[::1]:8080"] {
            assert!(authority_is_loopback(host), "{host} should be allowed");
        }
        for host in [
            "attacker.example",
            "127.0.0.1.attacker.example:80",
            "[::2]:80",
            "0.0.0.0:80",
            "",
        ] {
            assert!(!authority_is_loopback(host), "{host} should be refused");
        }
    }

    #[test]
    fn api_requests_must_come_from_this_app() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut headers = HeaderMap::new();
            for &(name, value) in pairs {
                headers.insert(name, value.parse().unwrap());
            }
            headers
        };
        assert!(
            api_request_allowed(&headers(&[])),
            "non-browser clients send neither header"
        );
        assert!(api_request_allowed(&headers(&[
            ("sec-fetch-site", "same-origin"),
            ("origin", "http://localhost:8080"),
        ])));
        assert!(api_request_allowed(&headers(&[("sec-fetch-site", "none")])));
        assert!(!api_request_allowed(&headers(&[("sec-fetch-site", "cross-site")])));
        assert!(!api_request_allowed(&headers(&[("sec-fetch-site", "same-site")])));
        assert!(!api_request_allowed(&headers(&[("origin", "http://attacker.example")])));
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
