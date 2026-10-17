#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::future::IntoFuture;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use clap::Parser;
use parking_lot::{Mutex, RwLock};
use rust_embed::RustEmbed;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::compression::CompressionLayer;

use rsdirstat_core::layout::{self, FilteredSizes, LayoutConfig};
use rsdirstat_core::scan::{ScanEvent, ScanOptions};
use rsdirstat_core::tree::{DirTree, FilterConfig};
use rsdirstat_protocol::{self as wire, ClientMessage};

mod scanning;
use scanning::{ScanArgs, scanner};

#[derive(Parser)]
#[command(name = "rsdirstat", version, about = "Interactive treemap disk usage server")]
struct Args {
    /// Path to scan (omit to show volume picker)
    path: Option<PathBuf>,

    #[command(flatten)]
    scan: ScanArgs,

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
    color_mode: wire::ColorMode,
    filter: FilterConfig,
    /// The sizes `filter` left at the last layout, kept for the next while they still hold.
    filtered: FilteredSizes,
    /// What the last layout sent showed.
    shown: Option<Shown>,
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            scan: 0,
            viewport: (0.0, 0.0),
            view_root: None,
            max_depth: wire::DEFAULT_DEPTH,
            color_mode: wire::ColorMode::default(),
            filter: FilterConfig::default(),
            filtered: FilteredSizes::default(),
            shown: None,
        }
    }
}

/// Everything a tab's layout shows, so one that would show nothing new isn't sent: during a scan,
/// a view of a folder already read stays as it is while the rest of the tree grows.
#[derive(PartialEq)]
struct Shown {
    scan: u64,
    view_root: u64,
    /// When anything under the view root last changed (`DirNode::changed`).
    changed: u64,
    viewport: (f64, f64),
    max_depth: u8,
    color_mode: wire::ColorMode,
    filter: FilterConfig,
    scan_done: bool,
    /// In Age mode, the tree's range of mtimes, which a folder's colour is relative to.
    age_range: Option<(i64, i64)>,
}

/// How often a tab gets a new layout while a scan streams in, at most. Once it's done, a tab gets one
/// as soon as its view changes.
const SCAN_LAYOUT_INTERVAL: Duration = Duration::from_millis(45);

/// While a scan streams in, a tab waits at least this many times as long as its last layout took
/// before the next. Laying out holds the tree's read lock, and a big filtered view can take longer
/// than `SCAN_LAYOUT_INTERVAL`: back to back, a few tabs would keep the scan's events from the tree.
const SCAN_LAYOUT_COST_FACTOR: u32 = 4;

/// Grace period after the last client disconnects before the server exits. Long enough to
/// survive a page reload (which briefly drops the WebSocket) without shutting down underneath
/// the user.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Where the server is in a scan's life. Scans never overlap: one starts only from a phase other
/// than `Running`.
enum ScanPhase {
    /// No path to scan: the volume picker is up. At startup without a path, and after a scan
    /// couldn't start or ended without reading its root.
    AwaitingPath,
    /// The scan of `root` waits for the page's Start button (`--wait`).
    AwaitingStart { root: PathBuf },
    /// Scanning `root` since `started`. `cancel` stops it, and also identifies this scan to its
    /// relay task.
    Running {
        root: PathBuf,
        cancel: Arc<AtomicBool>,
        started: Instant,
    },
    /// The last scan, of `root`, has ended (complete, or cut short by a cancel or stall) with its
    /// root read, after running for `took`. One that ended without has nothing to show: the phase
    /// goes back to `AwaitingPath` instead.
    Done { root: PathBuf, took: Duration },
}

impl ScanPhase {
    /// The path about to be, being or last scanned.
    fn root(&self) -> Option<&PathBuf> {
        match self {
            Self::AwaitingPath => None,
            Self::AwaitingStart { root } | Self::Running { root, .. } | Self::Done { root, .. } => Some(root),
        }
    }
}

struct ScanState {
    tree: RwLock<DirTree>,
    phase: Mutex<ScanPhase>,
    options: ScanOptions,
}

struct AppState {
    scan: ScanState,
    /// Signalled to send every tab to the volume picker.
    picker: watch::Sender<()>,
    /// Signalled whenever the tree changes. Every tab watches it and lays out its own view again; a
    /// tab that falls behind skips straight to the newest tree instead of a queue of stale layouts.
    tree_changes: watch::Sender<()>,
    /// The WebSocket connections (see `Connection`); the server exits after a grace period once the
    /// last one closes.
    connections: Mutex<Connections>,
    shutdown: watch::Sender<bool>,
    session: Session,
}

impl AppState {
    fn new(phase: ScanPhase, options: ScanOptions, session: Session) -> Self {
        Self {
            scan: ScanState {
                tree: RwLock::new(DirTree::new()),
                phase: Mutex::new(phase),
                options,
            },
            picker: watch::channel(()).0,
            tree_changes: watch::channel(()).0,
            connections: Mutex::default(),
            shutdown: watch::channel(false).0,
            session,
        }
    }

    fn tree_changed(&self) {
        self.tree_changes.send_replace(());
    }
}

/// What the page holds and nothing else does: a random token, in the address rsdirstat opens, which
/// the page trades for a cookie. The API asks for that cookie. Listening on loopback keeps other
/// machines out, and the checks of `Host`, `Origin` and `Sec-Fetch-Site` other websites, but neither
/// stops another user of this machine, or another program, from connecting to the port.
struct Session {
    port: u16,
    token: String,
    /// The cookie's name, with the port in it: browsers send a host's cookies to all its ports, and
    /// two servers running at once mustn't overwrite each other's.
    cookie: String,
}

impl Session {
    /// A new session for the server on `port`, with 128 random bits for its token.
    fn new(port: u16) -> Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("can't make a session token: {e}"))?;
        Ok(Self {
            port,
            token: bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
            cookie: format!("rsdirstat-{port}"),
        })
    }

    /// The address that starts the session in a browser.
    fn url(&self) -> String {
        format!("http://localhost:{}/?token={}", self.port, self.token)
    }

    /// Whether `token` is this session's, compared in constant time.
    fn is_token(&self, token: &str) -> bool {
        token.len() == self.token.len()
            && token
                .bytes()
                .zip(self.token.bytes())
                .fold(0, |differ, (a, b)| differ | (a ^ b))
                == 0
    }

    /// Whether a request with `headers` carries the session's cookie.
    fn admits(&self, headers: &HeaderMap) -> bool {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|cookie| cookie.trim().split_once('='))
            .any(|(name, value)| name == self.cookie && self.is_token(value))
    }

    /// Whether `uri` carries the session's token, as the address that starts the session does.
    fn in_query(&self, uri: &Uri) -> bool {
        uri.query()
            .into_iter()
            .flat_map(|query| query.split('&'))
            .filter_map(|pair| pair.strip_prefix("token="))
            .any(|token| self.is_token(token))
    }

    /// The cookie that admits the page's requests: only to this host, never sent by another site's
    /// requests (`SameSite=Strict`), and not readable by scripts.
    fn set_cookie(&self) -> HeaderValue {
        HeaderValue::try_from(format!(
            "{}={}; Path=/; HttpOnly; SameSite=Strict",
            self.cookie, self.token
        ))
        .expect("a cookie of a hex token is a valid header")
    }
}

/// How long shutdown waits for work in flight before exiting anyway: first for requests still being
/// answered (see `serve_until_stopped`), then for blocking tasks (see `main`). Either can be stuck
/// in a syscall on a dead network mount.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// How long a scan may take to open its root (canonicalize, stat, open) before the page goes back
/// to the picker. A dead network mount can block those calls for good, before the scan's own stall
/// detection starts. As long as that detection's timeout, for a disk that is only waking up.
const OPEN_TIMEOUT: Duration = rsdirstat_core::scan::STALL_TIMEOUT;

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
        scan,
        port,
        no_open,
        wait,
    } = args;
    // Canonicalize once, here at the boundary, and fail fast: a path that can't be scanned is an
    // error on the command line, not a browser tab that opens on the volume picker. A file, or a
    // folder that can't be listed, canonicalizes fine, so the root must open as a folder too.
    let phase = match path {
        None => ScanPhase::AwaitingPath,
        Some(path) => ScanPhase::AwaitingStart {
            root: std::fs::canonicalize(&path)
                .and_then(|root| std::fs::read_dir(&root).map(|_| root))
                .with_context(|| format!("can't scan {}", path.display()))?,
        },
    };

    // Bind loopback only: the UI is always pointed at localhost, and the server exposes full
    // directory listings plus arbitrary-path scans, so there's no reason to accept connections from
    // other hosts on the network. Connections from this one need the session's token.
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let actual_port = listener.local_addr()?.port();
    let state = Arc::new(AppState::new(phase, scan.into(), Session::new(actual_port)?));

    if Assets::get("index.html").is_none() {
        eprintln!("Warning: frontend bundle not embedded — run `trunk build` before building; the GUI will not load.");
    }

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/start", get(start_handler))
        .route("/volumes", get(volumes_handler))
        // The API only, not the frontend bundle the fallback serves.
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            require_own_page,
        ))
        .fallback(static_handler)
        .with_state(Arc::clone(&state))
        .layer(CompressionLayer::new())
        .layer(axum::middleware::map_response(deny_framing))
        .layer(axum::middleware::from_fn(require_loopback_host));

    // Once there is a server to show it: a port that can't be had fails the start before any scan.
    if !wait {
        start_scan(&state, None, |phase| matches!(phase, ScanPhase::AwaitingStart { .. }));
    }
    let url = state.session.url();
    eprintln!("Listening on {url}");

    // Kept until the server exits, then deleted.
    let _opener = if no_open {
        None
    } else {
        open_browser(&url)
            .inspect_err(|e| eprintln!("Failed to open browser: {e}"))
            .ok()
    };

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal(Arc::clone(&state)));
    serve_until_stopped(server, state.shutdown.subscribe(), DRAIN_GRACE).await?;
    Ok(())
}

/// Open `url` in the browser through a file that redirects to it, as Jupyter does: the token stays out
/// of the command lines of the programs that open it (`xdg-open`, the browser), which other users of
/// the machine can read. Only this user can read the file (`tempfile` makes it 0600), and it is
/// deleted when the returned path is dropped. Without waiting for the browser: a launcher can take a
/// while to return, and nothing is served until this does.
fn open_browser(url: &str) -> std::io::Result<tempfile::TempPath> {
    use std::io::Write;
    let mut file = tempfile::Builder::new()
        .prefix("rsdirstat-")
        .suffix(".html")
        .tempfile()?;
    write!(
        file,
        "<!doctype html><meta charset=utf-8><meta http-equiv=refresh content=\"0;url={url}\">\
         <title>rsdirstat</title><a href=\"{url}\">Open rsdirstat</a>"
    )?;
    // Closed, so the browser can read it on Windows too.
    let path = file.into_temp_path();
    open::that_detached(&path)?;
    Ok(path)
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
/// tab's first viewport, or before a scan's root arrives. Nor when it would show what the tab's last
/// layout did.
fn update_tab(state: &AppState, view: &mut ViewConfig) -> (Option<Bytes>, Option<Bytes>) {
    let tree = state.scan.tree.read();
    // The phase too under the tree's lock, under which the relay ends a scan: so a layout of the
    // final tree is never one marked as still running.
    let (scan_done, scan_time) = match *state.scan.phase.lock() {
        ScanPhase::Running { started, .. } => (false, started.elapsed()),
        ScanPhase::Done { took, .. } => (true, took),
        ScanPhase::AwaitingPath | ScanPhase::AwaitingStart { .. } => return (None, None),
    };

    // Checked under the lock the layout is computed under, so a page always hears of a new scan
    // before it gets a layout of it (`ScanStart` clears the page).
    let scan_start = (view.scan != tree.scans).then(|| {
        view.scan = tree.scans;
        view.view_root = None;
        Bytes::from(wire::ServerMessage::ScanStart.encode())
    });
    let (vw, vh) = view.viewport;
    let Some(root_id) = tree.root_id.filter(|_| vw > 0.0 && vh > 0.0) else {
        return (scan_start, None);
    };
    let view_root = view
        .view_root
        .filter(|id| tree.nodes.contains_key(id))
        .unwrap_or(root_id);
    let shown = Shown {
        scan: tree.scans,
        view_root,
        changed: tree.nodes[&view_root].changed,
        viewport: view.viewport,
        max_depth: view.max_depth,
        color_mode: view.color_mode,
        filter: view.filter.clone(),
        scan_done,
        age_range: (view.color_mode == wire::ColorMode::Age).then_some(tree.mtime_range),
    };
    if view.shown.as_ref() == Some(&shown) {
        return (scan_start, None);
    }
    view.shown = Some(shown);
    let config = LayoutConfig {
        max_depth: view.max_depth,
        color_mode: view.color_mode,
        filter: &view.filter,
    };
    let layout = layout::compute_layout(&tree, view_root, vw, vh, &config, &mut view.filtered);
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
        separator: std::path::MAIN_SEPARATOR,
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
        let Some(root) = root.or_else(|| phase.root().cloned()) else {
            return;
        };
        *phase = ScanPhase::Running {
            root: root.clone(),
            cancel: Arc::clone(&cancel),
            started: Instant::now(),
        };
        root
    };

    let (tx, rx) = std::sync::mpsc::channel::<ScanEvent>();
    let relay_state = Arc::clone(state);
    let relay_cancel = Arc::clone(&cancel);
    tokio::task::spawn_blocking(move || relay_scan(&relay_state, &rx, &relay_cancel, OPEN_TIMEOUT));

    let options = state.scan.options;
    tokio::task::spawn_blocking(move || {
        // On failure the scanner drops `tx` without sending anything; the relay takes it from there.
        if let Err(e) = scanner::scan::scan(&path, options, tx, cancel) {
            eprintln!("Scan error: {e:#}");
        }
    });
}

/// Apply one scan's events to the shared tree until it ends, then end its phase: `Done` at
/// `ScanDone`, or, if the scanner stops without one, `Done` with what it found (it panicked). A scan
/// that ends without its root read has nothing to show, and goes back to the picker: one that
/// couldn't open the root, took longer than `open_timeout` to, or stalled reading it.
fn relay_scan(
    state: &AppState,
    rx: &std::sync::mpsc::Receiver<ScanEvent>,
    cancel: &Arc<AtomicBool>,
    open_timeout: Duration,
) {
    // Only this scan's relay may end its phase. `next` gets its root and how long it ran.
    let end_phase = |next: fn(PathBuf, Duration) -> ScanPhase| {
        let mut phase = state.scan.phase.lock();
        let ScanPhase::Running {
            root,
            cancel: running,
            started,
        } = &*phase
        else {
            return false;
        };
        if !Arc::ptr_eq(running, cancel) {
            return false;
        }
        *phase = next(root.clone(), started.elapsed());
        true
    };
    let ended = |root, took| ScanPhase::Done { root, took };
    let to_picker = || {
        if end_phase(|_, _| ScanPhase::AwaitingPath) {
            state.picker.send_replace(());
        }
    };

    // The scanner's first event, or its failure, once it has opened the root. A root it is still
    // opening after `open_timeout` is given up on: whatever the scanner sends if it ever gets going
    // goes nowhere, and the cancel stops it.
    let first = match rx.recv_timeout(open_timeout) {
        Ok(event) => event,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            eprintln!(
                "Gave up on a scan still opening its root after {}s",
                open_timeout.as_secs()
            );
            cancel.store(true, Ordering::Relaxed);
            return to_picker();
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return to_picker(),
    };

    // The first is the scan's `ScanStart` (`Started::new`), so the tree is this scan's from here on.
    let mut incoming = std::iter::once(first).chain(rx.iter());
    // Reused across iterations so the per-batch drain doesn't reallocate each wakeup.
    let mut events = Vec::new();
    // Until `ScanDone`, or until the scanner stops without one.
    let (mut tree, done) = loop {
        let Some(event) = incoming.next() else {
            break (state.scan.tree.write(), false);
        };
        events.push(event);
        events.extend(rx.try_iter());

        let mut tree = state.scan.tree.write();
        // A new scan's `ScanStart` replaces the tree: the old one's nodes are freed once the lock is
        // released and the tabs told, as millions of names take tens of milliseconds to free.
        let mut replaced = None;
        let mut done = false;
        for event in events.drain(..) {
            if matches!(event, ScanEvent::ScanStart { .. }) {
                replaced = Some(std::mem::take(&mut tree.nodes));
            }
            done = matches!(event, ScanEvent::ScanDone { .. });
            tree.apply(event);
            if done {
                break;
            }
        }
        if done {
            break (tree, true);
        }
        drop(tree);
        state.tree_changed();
        drop(replaced);
    };

    // Ended under the tree lock, so no tab lays out the final tree still marked as running. A scan
    // that never read its root (it stalled on it, or the scanner stopped) has nothing to show: back
    // to the picker, as for one that couldn't open it.
    if tree.root_id.is_none() {
        drop(tree);
        return to_picker();
    }
    // Without `ScanDone` the tree isn't marked complete, and the page says so.
    if !done {
        tree.recompute_totals();
    }
    end_phase(ended);
    drop(tree);
    state.tree_changed();
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

/// Serve the embedded frontend. Unknown paths 404. The page itself only with the session's token, in
/// the address (which gives it the session's cookie) or as that cookie: without it, the page's
/// requests would all be refused. The rest of the bundle is the same in every copy of rsdirstat.
async fn static_handler(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let path = match uri.path().trim_start_matches('/') {
        "" => "index.html",
        p => p,
    };
    let session = &state.session;
    let set_cookie = match path {
        "index.html" if session.in_query(&uri) => Some(session.set_cookie()),
        "index.html" if !session.admits(&headers) => {
            return (
                StatusCode::FORBIDDEN,
                "Open the address rsdirstat printed when it started (with its ?token=), or start it again.",
            )
                .into_response();
        }
        _ => None,
    };
    match Assets::get(path) {
        Some(content) => {
            // Content-type is baked in at compile time by rust-embed's `mime-guess` feature.
            let mime = content.metadata.mimetype().to_owned();
            let cookie = set_cookie.map(|cookie| [(header::SET_COOKIE, cookie)]);
            (cookie, [(header::CONTENT_TYPE, mime)], content.data).into_response()
        }
        None => (StatusCode::NOT_FOUND, "Not Found").into_response(),
    }
}

async fn start_handler(axum::extract::State(state): axum::extract::State<Arc<AppState>>) -> &'static str {
    start_scan(&state, None, |phase| matches!(phase, ScanPhase::AwaitingStart { .. }));
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

/// Never shown in another site's frame: there the user's clicks on what looks like that site could
/// be on this page instead (clickjacking), starting a scan or revealing a path.
async fn deny_framing(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    response
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// Refuse an API request without the session's cookie (see `Session`), or from another site's page,
/// which could otherwise drive the server from the user's own browser: connect to ws://127.0.0.1,
/// or start a scan. Such a page's requests don't carry the cookie (`SameSite=Strict`) either; the
/// check of where they come from holds for a browser that would send it.
async fn require_own_page(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !api_request_allowed(request.headers()) || !state.session.admits(request.headers()) {
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

/// The WebSocket connections, counted together under one lock: a shutdown timer armed as the last
/// one closes must see every connection opened since, even one opening as it closes.
#[derive(Default)]
struct Connections {
    open: u64,
    /// Connections ever opened. A shutdown timer only fires if this hasn't moved since it was armed.
    opened: u64,
}

/// Holds one count in `AppState::connections` for the life of a WebSocket handler, whichever way it
/// exits. Dropping the last one arms the auto-shutdown timer.
struct Connection(Arc<AppState>);

impl Connection {
    fn open(state: &Arc<AppState>) -> Self {
        let mut connections = state.connections.lock();
        connections.open += 1;
        connections.opened += 1;
        Self(Arc::clone(state))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let armed_at = {
            let mut connections = self.0.connections.lock();
            connections.open -= 1;
            if connections.open > 0 {
                return;
            }
            connections.opened
        };
        // A handler dropped while the runtime itself shuts down has no timer to arm.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let state = Arc::clone(&self.0);
        runtime.spawn(async move {
            tokio::time::sleep(SHUTDOWN_GRACE).await;
            // Shut down only if nobody connected during the grace period. Checking just the count
            // would let an earlier disconnect's timer fire right after a later reconnect-and-leave
            // (two quick reloads), well inside that second disconnect's grace period.
            if state.connections.lock().opened == armed_at {
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

    // A new tab starts on the picker while the server waits for a path, and on the Start button
    // while it waits for that. One that joins a scan whose root hasn't arrived yet (a Windows
    // drive's MFT takes seconds to read) is told it started, so it shows it running instead of a
    // blank page. Otherwise its first layout follows the viewport it sends on connecting, and shows
    // the scan without being told it started.
    let (waiting, scan) = {
        let tree = state.scan.tree.read();
        let message = |message: wire::ServerMessage| Some(Message::Binary(message.encode().into()));
        let waiting = match *state.scan.phase.lock() {
            ScanPhase::AwaitingPath => Some(picker_mode()),
            ScanPhase::AwaitingStart { .. } => message(wire::ServerMessage::AwaitingStart),
            ScanPhase::Running { .. } if tree.root_id.is_none() => message(wire::ServerMessage::ScanStart),
            ScanPhase::Running { .. } | ScanPhase::Done { .. } => None,
        };
        (waiting, tree.scans)
    };
    if let Some(message) = waiting
        && socket.send(message).await.is_err()
    {
        return;
    }

    let mut view = ViewConfig {
        scan,
        ..ViewConfig::default()
    };
    // Whether the tree or the view changed since the last layout sent.
    let mut stale = false;
    let mut last_layout = tokio::time::Instant::now();
    let mut layout_took = Duration::ZERO;
    loop {
        // While a scan streams in, layouts are rate-limited; otherwise one goes out at once.
        let scanning = matches!(*state.scan.phase.lock(), ScanPhase::Running { .. });
        let layout_due = if scanning {
            last_layout + SCAN_LAYOUT_INTERVAL.max(layout_took * SCAN_LAYOUT_COST_FACTOR)
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
                // Off the async workers: laying out a big filtered view under the tree's lock can
                // take tens of milliseconds, which would hold up the other tabs and requests.
                let (scan_start, layout) = tokio::task::block_in_place(|| update_tab(&state, &mut view));
                layout_took = last_layout.elapsed();
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
    let Some(mut path) = state.scan.tree.read().full_path(dir_id) else {
        return;
    };
    if let Some(name) = name {
        path.push(name);
    }
    // Waited for off the async workers (xdg-open can last as long as the file manager it opens),
    // so the finished child is reaped instead of left a zombie for the life of the server.
    if let Ok(mut child) = scanner::reveal::command(&path).spawn() {
        std::thread::spawn(move || child.wait());
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
        ClientMessage::Filter {
            extensions,
            min_size,
            name_pattern,
        } => set(
            &mut view.filter,
            FilterConfig {
                extensions,
                min_size,
                name_pattern,
            },
        ),
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
            start_scan(state, None, |phase| matches!(phase, ScanPhase::Done { .. }));
            false
        }
        ClientMessage::ScanPath { path } => {
            // Only from the picker, or once the current scan is done (never mid-scan). A path that
            // can't be scanned sends the page back to the picker (see `relay_scan`).
            start_scan(state, Some(path.into()), |phase| {
                matches!(phase, ScanPhase::AwaitingPath | ScanPhase::Done { .. })
            });
            false
        }
    }
}

async fn volumes_handler() -> impl IntoResponse {
    let volumes = tokio::task::spawn_blocking(scanner::volumes::list_volumes)
        .await
        .unwrap_or_default();
    (
        [(header::CONTENT_TYPE, "application/octet-stream")],
        wire::VolumeInfo::encode_list(&volumes),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsdirstat_core::scan::{ScanOutcome, ScannedFile};

    fn session() -> Session {
        Session::new(4242).unwrap()
    }

    /// A server scanning `root` since `started`, and the scan's cancel flag.
    fn running(root: &str, started: Instant) -> (AppState, Arc<AtomicBool>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let phase = ScanPhase::Running {
            root: root.into(),
            cancel: Arc::clone(&cancel),
            started,
        };
        (AppState::new(phase, ScanOptions::default(), session()), cancel)
    }

    /// Only the session's cookie admits a request, and only the session's token starts it.
    #[test]
    fn a_session_admits_only_its_own_token() {
        let session = session();
        assert_eq!(session.token.len(), 32);
        assert_ne!(session.token, Session::new(4242).unwrap().token, "random");
        let cookies = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::COOKIE, HeaderValue::try_from(value).unwrap());
            headers
        };
        let token = &session.token;
        assert!(session.admits(&cookies(&format!("rsdirstat-4242={token}"))));
        assert!(session.admits(&cookies(&format!("other=1; rsdirstat-4242={token}; more=2"))));
        assert!(!session.admits(&HeaderMap::new()));
        assert!(
            !session.admits(&cookies(&format!("rsdirstat-4243={token}"))),
            "another port's"
        );
        assert!(!session.admits(&cookies("rsdirstat-4242=00000000000000000000000000000000")));
        assert!(!session.admits(&cookies(&format!("rsdirstat-4242={}", &token[1..]))));

        let uri = |query: &str| format!("/?{query}").parse::<Uri>().unwrap();
        assert!(session.in_query(&uri(&format!("token={token}"))));
        assert!(session.in_query(&uri(&format!("wait&token={token}"))));
        assert!(!session.in_query(&uri("token=nope")));
        assert!(!session.in_query(&"/".parse().unwrap()));
        assert!(session.url().ends_with(&format!(":4242/?token={token}")));
        let cookie = session.set_cookie();
        let cookie = cookie.to_str().unwrap();
        assert!(cookie.starts_with(&format!("rsdirstat-4242={token};")), "{cookie}");
        assert!(
            cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"),
            "{cookie}"
        );
    }

    /// The state after relaying a scan that ran for `ran_for` and ended with `ending`, or without
    /// a `ScanDone` at all, having found one 10-byte file.
    fn finished_scan(ending: Option<ScanOutcome>, ran_for: Duration) -> AppState {
        let (state, cancel) = running("/root", Instant::now() - ran_for);
        let (tx, rx) = std::sync::mpsc::channel();
        let file = ScannedFile {
            name: "a".into(),
            size: 10,
            mtime: 0,
        };
        for event in [
            ScanEvent::ScanStart { root: "/root".into() },
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
        relay_scan(&state, &rx, &cancel, OPEN_TIMEOUT);
        state
    }

    /// The layout `view` gets now.
    fn layout(state: &AppState, view: &mut ViewConfig) -> wire::LayoutPayload {
        let (_, layout) = update_tab(state, view);
        match wire::ServerMessage::decode(&layout.expect("a layout")) {
            Some(wire::ServerMessage::Layout(layout)) => layout,
            other => panic!("expected a layout, got {other:?}"),
        }
    }

    /// The layout a new tab gets.
    fn first_layout(state: &AppState) -> wire::LayoutPayload {
        let mut view = ViewConfig {
            viewport: (800.0, 600.0),
            ..ViewConfig::default()
        };
        layout(state, &mut view)
    }

    /// A tab gets a layout when the tree under its view or its filter changes, and not otherwise.
    #[test]
    fn filtered_layouts_follow_the_tree_and_the_filter() {
        let state = finished_scan(Some(ScanOutcome::Complete), Duration::ZERO);
        let mut view = ViewConfig {
            viewport: (800.0, 600.0),
            filter: FilterConfig {
                name_pattern: "a".into(),
                ..FilterConfig::default()
            },
            ..ViewConfig::default()
        };
        assert_eq!(layout(&state, &mut view).root_size, 10);
        assert_eq!(update_tab(&state, &mut view).1, None, "nothing new to show");

        let file = ScannedFile {
            name: "ab".into(),
            size: 5,
            mtime: 0,
        };
        state.scan.tree.write().apply(ScanEvent::Dir {
            id: 2,
            parent: 1,
            name: "sub".into(),
            mtime: 0,
            files: vec![file],
        });
        assert_eq!(layout(&state, &mut view).root_size, 15, "the tree changed");

        view.filter.name_pattern = "ab".into();
        assert_eq!(layout(&state, &mut view).root_size, 5, "the filter changed");
    }

    /// While a scan runs, a tab viewing a folder already read gets no layouts as the rest of the tree
    /// grows, and one as soon as something under its folder changes.
    #[test]
    fn a_view_is_laid_out_again_only_when_its_folder_changes() {
        let (state, _) = running("/root", Instant::now());
        let dir = |id, parent, size| ScanEvent::Dir {
            id,
            parent,
            name: format!("d{id}"),
            mtime: 0,
            files: vec![ScannedFile {
                name: "f".into(),
                size,
                mtime: 0,
            }],
        };
        let apply = |event| state.scan.tree.write().apply(event);
        apply(ScanEvent::ScanStart { root: "/root".into() });
        apply(dir(1, 0, 10));
        apply(dir(2, 1, 10));
        let mut view = ViewConfig {
            scan: 1,
            viewport: (800.0, 600.0),
            view_root: Some(2),
            ..ViewConfig::default()
        };
        assert_eq!(layout(&state, &mut view).root_size, 10);

        apply(dir(3, 1, 10));
        assert_eq!(update_tab(&state, &mut view).1, None, "a change elsewhere");

        apply(dir(4, 2, 5));
        assert_eq!(layout(&state, &mut view).root_size, 15, "a change under the view");
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

    /// A scan that ends before reading its root (it stalled on it, or the scanner stopped) has
    /// nothing to show, so every tab goes back to the picker, where a new tab starts too.
    #[test]
    fn a_scan_that_ends_without_its_root_goes_back_to_the_picker() {
        for ending in [Some(ScanOutcome::Stalled), None] {
            let (state, cancel) = running("/root", Instant::now());
            let picker = state.picker.subscribe();
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(ScanEvent::ScanStart { root: "/root".into() }).unwrap();
            if let Some(outcome) = ending {
                tx.send(ScanEvent::ScanDone { outcome }).unwrap();
            }
            drop(tx);
            relay_scan(&state, &rx, &cancel, OPEN_TIMEOUT);
            assert!(
                matches!(*state.scan.phase.lock(), ScanPhase::AwaitingPath),
                "{ending:?}"
            );
            assert!(picker.has_changed().unwrap(), "{ending:?}");
        }
    }

    /// A scanner stuck opening its root (a dead network mount) is given up on, so another path can
    /// be scanned.
    #[test]
    fn a_scan_stuck_opening_its_root_goes_back_to_the_picker() {
        let (state, cancel) = running("/dead", Instant::now());
        let picker = state.picker.subscribe();
        let (_stuck, rx) = std::sync::mpsc::channel();
        relay_scan(&state, &rx, &cancel, Duration::from_millis(100));
        assert!(matches!(*state.scan.phase.lock(), ScanPhase::AwaitingPath));
        assert!(picker.has_changed().unwrap());
        assert!(
            cancel.load(Ordering::Relaxed),
            "the stuck scanner stops if it ever gets going"
        );
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
