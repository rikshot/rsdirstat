#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::future::IntoFuture;
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
use tokio::sync::watch;
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
struct ViewConfig {
    /// Which scan's tree the tab shows (`DirTree::scans`).
    scan: u64,
    viewport: (f64, f64),
    view_root: Option<u64>,
    max_depth: u8,
    color_mode: u8,
    filter: FilterConfig,
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            scan: 0,
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
    /// Scanning since `started`. `cancel` stops it, and also identifies this scan to its relay task.
    Running { cancel: Arc<AtomicBool>, started: Instant },
    /// The last scan has ended (complete, or cut short by a cancel or stall), after running this
    /// long.
    Done(Duration),
}

struct ScanState {
    tree: RwLock<DirTree>,
    phase: Mutex<ScanPhase>,
    /// The path being (or last) scanned.
    root: Mutex<PathBuf>,
    cross_filesystems: bool,
}

struct AppState {
    scan: ScanState,
    /// Signalled to send every tab to the volume picker.
    picker: watch::Sender<()>,
    /// Signalled whenever the tree changes. Every tab watches it and lays out its own view again; a
    /// tab that falls behind skips straight to the newest tree instead of a queue of stale layouts.
    tree_changes: watch::Sender<()>,
    /// Open WebSocket connections (see `Connection`); the server exits after a grace period once the
    /// last one closes.
    connections: AtomicU64,
    /// Connections ever opened. A shutdown timer only fires if this hasn't moved since it was armed.
    connects: AtomicU64,
    shutdown: watch::Sender<bool>,
}

impl AppState {
    fn new(phase: ScanPhase, root: PathBuf, cross_filesystems: bool) -> Self {
        Self {
            scan: ScanState {
                tree: RwLock::new(DirTree::new()),
                phase: Mutex::new(phase),
                root: Mutex::new(root),
                cross_filesystems,
            },
            picker: watch::channel(()).0,
            tree_changes: watch::channel(()).0,
            connections: AtomicU64::new(0),
            connects: AtomicU64::new(0),
            shutdown: watch::channel(false).0,
        }
    }

    fn tree_changed(&self) {
        self.tree_changes.send_replace(());
    }
}

/// How long shutdown waits for work in flight before exiting anyway: first for requests still being
/// answered (see `serve_until_stopped`), then for blocking tasks (see `main`). Either can be stuck
/// in a syscall on a dead network mount.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

fn main() -> Result<()> {
    let args = Args::parse();
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(run_server(args));
    // Dropping the runtime would wait for every blocking task, and one can be stuck in a syscall on
    // a dead network mount (a scan of it, or `statvfs` for /volumes) for as long as the mount takes
    // to time out, if it ever does. A healthy scan stops within ~200ms of being cancelled, so give
    // the tasks a moment, then exit regardless.
    rt.shutdown_timeout(DRAIN_GRACE);
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

    let state = Arc::new(AppState::new(phase, scan_root, cross_filesystems));

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
        // The API only, not the frontend bundle the fallback serves.
        .route_layer(axum::middleware::from_fn(require_own_page))
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

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal(Arc::clone(&state)));
    serve_until_stopped(server, state.shutdown.subscribe(), DRAIN_GRACE).await?;
    Ok(())
}

/// Run `server` until it finishes, or until `grace` after `shutdown` turns true. A graceful shutdown
/// waits for every request in flight, and one can be stuck for as long as a dead network mount takes
/// to time out (`statvfs` for /volumes), if it ever does; the server must still exit.
async fn serve_until_stopped(
    server: impl IntoFuture<Output = std::io::Result<()>>,
    mut shutdown: watch::Receiver<bool>,
    grace: Duration,
) -> std::io::Result<()> {
    let deadline = async move {
        let _ = shutdown.wait_for(|&stopping| stopping).await;
        tokio::time::sleep(grace).await;
    };
    tokio::select! {
        result = server.into_future() => result,
        () = deadline => {
            eprintln!("Exiting with requests still unanswered");
            Ok(())
        }
    }
}

/// What brings a tab up to date with the tree, encoded: a `ScanStart` if a new scan replaced the
/// tree it last showed (which also takes its view back to the root), then a layout of its view.
/// No layout while there is nothing to show: waiting for a path or for Start (the tree may still
/// hold an earlier scan, and a layout would bring its treemap back over the picker), before the
/// tab's first viewport, or before a scan's root arrives.
fn update_tab(state: &AppState, view: &mut ViewConfig) -> (Option<Bytes>, Option<Bytes>) {
    let (scan_done, scan_time) = match *state.scan.phase.lock() {
        ScanPhase::Running { started, .. } => (false, started.elapsed()),
        ScanPhase::Done(took) => (true, took),
        ScanPhase::AwaitingPath | ScanPhase::AwaitingStart => return (None, None),
    };

    let tree = state.scan.tree.read();
    // Checked under the lock the layout is computed under, so a page always hears of a new scan
    // before it gets a layout of it (`ScanStart` clears the page).
    let scan_start = (view.scan != tree.scans).then(|| {
        view.scan = tree.scans;
        view.view_root = None;
        Bytes::from(
            wire::ServerMessage::ScanStart {
                path: tree.scan_path.clone(),
            }
            .encode(),
        )
    });
    let (vw, vh) = view.viewport;
    let Some(root_id) = tree.root_id.filter(|_| vw > 0.0 && vh > 0.0) else {
        return (scan_start, None);
    };
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
    let layout = layout::compute_layout(&tree, view_root, vw, vh, &config);
    let breadcrumb = tree.breadcrumb(view_root);
    let dir_count = tree.nodes.len() as u32;
    let partial = scan_done && !tree.complete;
    drop(tree);

    let payload = wire::LayoutPayload {
        root_size: layout.size,
        dir_count,
        scan_done,
        partial,
        scan_ms: u32::try_from(scan_time.as_millis()).unwrap_or(u32::MAX),
        breadcrumb,
        rects: layout.rects,
    };
    (scan_start, Some(wire::ServerMessage::Layout(payload).encode().into()))
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
    // What a console sends a process group to stop it; the tests stop the server this way.
    #[cfg(windows)]
    let terminate = async {
        tokio::signal::windows::ctrl_break()
            .expect("failed to install Ctrl+Break handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = terminate => {},
        _ = tokio::signal::ctrl_c() => {},
        _ = rx.changed() => {},
    }
    // Signal all WS handlers to close
    let _ = state.shutdown.send(true);
    // Stop an in-flight scan too. It runs on the blocking pool, which the runtime waits on at
    // exit, so without this a Ctrl+C during a large scan hangs until the whole walk finishes.
    if let ScanPhase::Running { cancel, .. } = &*state.scan.phase.lock() {
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
            started: Instant::now(),
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
    // Only this scan's relay may end its phase. `next` gets how long the scan ran.
    let end_phase = |next: fn(Duration) -> ScanPhase| {
        let mut phase = state.scan.phase.lock();
        let ScanPhase::Running {
            cancel: running,
            started,
        } = &*phase
        else {
            return false;
        };
        if !Arc::ptr_eq(running, cancel) {
            return false;
        }
        *phase = next(started.elapsed());
        true
    };

    // Reused across iterations so the per-batch drain doesn't reallocate each wakeup.
    let mut events = Vec::new();
    let mut started = false;
    while let Ok(event) = rx.recv() {
        events.push(event);
        events.extend(rx.try_iter());

        let mut tree = state.scan.tree.write();
        for event in events.drain(..) {
            started |= matches!(event, ScanEvent::ScanStart { .. });
            let done = matches!(event, ScanEvent::ScanDone { .. });
            tree.apply(event);
            if done {
                // Under the tree lock, so no tab lays out the final tree still marked as running.
                end_phase(ScanPhase::Done);
                drop(tree);
                state.tree_changed();
                return;
            }
        }
        drop(tree);
        state.tree_changed();
    }

    // The scanner stopped without `ScanDone`, so the tree isn't marked complete: the page says so.
    if started {
        state.scan.tree.write().recompute_totals();
        if end_phase(ScanPhase::Done) {
            state.tree_changed();
        }
    } else if end_phase(|_| ScanPhase::AwaitingPath) {
        state.picker.send_replace(());
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
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// Refuse an API request from another site's page. Binding to loopback stops remote hosts, but the
/// server has no auth and exposes the filesystem, so a malicious page in the user's own browser
/// could otherwise drive it: connect to ws://127.0.0.1, or start a scan.
async fn require_own_page(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if !api_request_allowed(request.headers()) {
        return (StatusCode::FORBIDDEN, "request refused: not from this app's own page").into_response();
    }
    next.run(request).await
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

    let mut picker = state.picker.subscribe();
    let mut tree_changes = state.tree_changes.subscribe();
    let mut shutdown_rx = state.shutdown.subscribe();
    let picker_mode = || Message::Binary(wire::ServerMessage::PickerMode.encode().into());

    // A new tab starts on the picker while the server waits for a path. Otherwise its first layout
    // follows the viewport it sends on connecting.
    let awaiting_path = matches!(*state.scan.phase.lock(), ScanPhase::AwaitingPath);
    if awaiting_path && socket.send(picker_mode()).await.is_err() {
        return;
    }

    // A tab that joins during or after a scan is shown it without being told it started.
    let mut view = ViewConfig {
        scan: state.scan.tree.read().scans,
        ..ViewConfig::default()
    };
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
            // In order, so a trip to the picker reaches the page before any later scan's layouts.
            biased;
            _ = shutdown_rx.changed() => break,
            changed = picker.changed() => {
                if changed.is_err() || socket.send(picker_mode()).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Binary(data))) => {
                        stale |= handle_client_message(&state, &mut view, &data);
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
            // A stale tab's next layout covers every change until then, so it stops listening.
            changed = tree_changes.changed(), if !stale => {
                if changed.is_err() {
                    break;
                }
                stale = true;
            }
            () = tokio::time::sleep_until(layout_due), if stale => {
                stale = false;
                last_layout = tokio::time::Instant::now();
                // Before laying out, so a change made meanwhile makes the tab stale again.
                tree_changes.mark_unchanged();
                let (scan_start, layout) = update_tab(&state, &mut view);
                for message in [scan_start, layout].into_iter().flatten() {
                    if socket.send(Message::Binary(message)).await.is_err() {
                        return;
                    }
                }
            }
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

/// Set `slot` to `value`, returning whether that changed it. A page sends its toolbar's values
/// whenever one of them is edited, so most messages leave the view as it was.
fn set<T: PartialEq>(slot: &mut T, value: T) -> bool {
    let changed = *slot != value;
    *slot = value;
    changed
}

/// Act on one message from a tab. View changes apply to that tab's `view` only; returns whether it
/// changed, so the tab is laid out again.
fn handle_client_message(state: &Arc<AppState>, view: &mut ViewConfig, data: &[u8]) -> bool {
    let Some(msg) = ClientMessage::decode(data) else {
        return false;
    };
    match msg {
        ClientMessage::Viewport { width, height } => set(&mut view.viewport, (f64::from(width), f64::from(height))),
        ClientMessage::Navigate { id } => set(&mut view.view_root, Some(id)),
        ClientMessage::SetDepth { depth } => set(&mut view.max_depth, depth),
        ClientMessage::ColorMode { mode } => set(&mut view.color_mode, mode),
        ClientMessage::FilterExt { extensions } => set(&mut view.filter.extensions, extensions),
        ClientMessage::FilterSize { min, max } => {
            set(&mut view.filter.min_size, min) | set(&mut view.filter.max_size, max)
        }
        ClientMessage::FilterName { pattern } => set(&mut view.filter.name_pattern, pattern),
        ClientMessage::ClearFilter => set(&mut view.filter, FilterConfig::default()),
        ClientMessage::RevealDir { id } => {
            reveal(state, id, None);
            false
        }
        ClientMessage::RevealFile { dir_id, name } => {
            // `name` is attacker-controllable over the WS; only join a single, non-traversing
            // component so it can't escape the resolved directory with `..` or an absolute path.
            if is_safe_component(&name) {
                reveal(state, dir_id, Some(&name));
            }
            false
        }
        ClientMessage::Rescan => {
            start_scan(state, None, |phase| matches!(phase, ScanPhase::Done(_)));
            false
        }
        ClientMessage::ScanPath { path } => {
            // Only from the picker, or once the current scan is done (never mid-scan). A path that
            // can't be scanned sends the page back to the picker (see `relay_scan`).
            start_scan(state, Some(path.into()), |phase| {
                matches!(phase, ScanPhase::AwaitingPath | ScanPhase::Done(_))
            });
            false
        }
    }
}

async fn volumes_handler() -> Json<Vec<wire::VolumeInfo>> {
    let volumes = tokio::task::spawn_blocking(scanner::volumes::list_volumes)
        .await
        .unwrap_or_default();
    Json(volumes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsdirstat_core::scan::{ScanOutcome, ScannedFile};

    /// The state after relaying a scan that ran for `ran_for` and ended with `ending`, or without
    /// a `ScanDone` at all, having found one 10-byte file.
    fn finished_scan(ending: Option<ScanOutcome>, ran_for: Duration) -> AppState {
        let cancel = Arc::new(AtomicBool::new(false));
        let running = ScanPhase::Running {
            cancel: Arc::clone(&cancel),
            started: Instant::now() - ran_for,
        };
        let state = AppState::new(running, PathBuf::from("/root"), false);
        let (tx, rx) = std::sync::mpsc::channel();
        let file = ScannedFile {
            name: "a".into(),
            size: 10,
            mtime: 0,
        };
        for event in [
            ScanEvent::ScanStart { path: "root".into() },
            ScanEvent::Dir {
                id: 1,
                parent: 0,
                name: "root".into(),
                mtime: 0,
                files: vec![file],
            },
        ]
        .into_iter()
        .chain(ending.map(|outcome| ScanEvent::ScanDone { outcome }))
        {
            tx.send(event).unwrap();
        }
        drop(tx);
        relay_scan(&state, &rx, &cancel);
        state
    }

    /// The layout a new tab gets.
    fn first_layout(state: &AppState) -> wire::LayoutPayload {
        let mut view = ViewConfig {
            viewport: (800.0, 600.0),
            ..ViewConfig::default()
        };
        let (_, layout) = update_tab(state, &mut view);
        match wire::ServerMessage::decode(&layout.expect("a layout")) {
            Some(wire::ServerMessage::Layout(layout)) => layout,
            other => panic!("expected a layout, got {other:?}"),
        }
    }

    /// A finished scan's layouts tell the page whether it read everything: not when it stalled, nor
    /// when the scanner stopped without saying how it ended.
    #[test]
    fn layouts_say_whether_the_scan_read_everything() {
        for (ending, partial) in [
            (Some(ScanOutcome::Complete), false),
            (Some(ScanOutcome::Stalled), true),
            (None, true),
        ] {
            let layout = first_layout(&finished_scan(ending, Duration::ZERO));
            assert!(layout.scan_done, "{ending:?}");
            assert_eq!(layout.partial, partial, "{ending:?}");
            assert_eq!(layout.root_size, 10, "{ending:?}");
        }
    }

    /// Every tab is told how long the scan took, however long after it finished the tab opened.
    #[test]
    fn layouts_carry_how_long_the_scan_took() {
        let state = finished_scan(Some(ScanOutcome::Complete), Duration::from_secs(5));
        let took = first_layout(&state).scan_ms;
        assert!((5_000..6_000).contains(&took), "{took}ms");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(first_layout(&state).scan_ms, took, "a later tab sees the same time");
    }

    /// Shutdown doesn't wait for ever on a request stuck in a syscall.
    #[tokio::test]
    async fn shutdown_gives_up_on_requests_that_never_finish() {
        let (shutdown, stopping) = watch::channel(false);
        let stuck = std::future::pending::<std::io::Result<()>>();
        let serving = tokio::spawn(serve_until_stopped(stuck, stopping, Duration::from_millis(100)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!serving.is_finished(), "it serves until shutdown starts");

        shutdown.send_replace(true);
        let served = tokio::time::timeout(Duration::from_secs(5), serving).await;
        assert!(matches!(served, Ok(Ok(Ok(())))), "{served:?}");
    }

    /// A page names a file to reveal, and the server joins that name onto a scanned directory, so a
    /// name that could climb out of it (`..`, a path, an absolute path) must be refused.
    #[test]
    fn reveal_names_must_be_one_plain_component() {
        for name in ["a.txt", "My File.pdf", ".hidden", "..rc", "notes..", "a/"] {
            assert!(is_safe_component(name), "{name:?} should be allowed");
        }
        for name in ["", ".", "..", "./a", "a/b", "a/..", "../etc/passwd", "/etc/passwd"] {
            assert!(!is_safe_component(name), "{name:?} should be refused");
        }
        #[cfg(windows)]
        for name in [r"a\b", r"..\x", r"C:\Windows", "C:x", r"\\server\share"] {
            assert!(!is_safe_component(name), "{name:?} should be refused");
        }
    }

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
