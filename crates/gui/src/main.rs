#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::routing::get;
use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast};
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;

use rsdirstat_core::layout::{self, LayoutConfig};
use rsdirstat_core::protocol::{self, ClientMessage, ScanEvent};
use rsdirstat_core::tree::FilterConfig;

#[cfg(target_os = "macos")]
use rsdirstat_macos as scanner;
#[cfg(target_os = "linux")]
use rsdirstat_linux as scanner;
#[cfg(target_os = "windows")]
use rsdirstat_windows as scanner;

#[derive(Parser)]
#[command(name = "rsdirstat-gui", about = "Interactive treemap disk usage visualizer")]
struct Args {
    /// Path to scan
    #[arg(default_value = ".")]
    path: PathBuf,

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

struct ScanState {
    tree: RwLock<layout::DirTree>,
    version: AtomicU64,
    done: AtomicBool,
    root: PathBuf,
    cross_filesystems: bool,
}

struct LayoutBroadcast {
    tx: broadcast::Sender<Vec<u8>>,
    last: Mutex<Option<Vec<u8>>>,
    notify: Notify,
}

struct ConnectionTracker {
    count: AtomicU64,
    had_any: AtomicBool,
}

struct AppState {
    scan: ScanState,
    view: Mutex<ViewConfig>,
    layout: LayoutBroadcast,
    connections: ConnectionTracker,
    start: Notify,
}

impl AppState {
    fn invalidate_layout(&self) {
        self.scan.version.fetch_add(1, Ordering::Relaxed);
        self.layout.notify.notify_one();
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_server(args.path, args.all, args.port, args.no_open, args.wait))
}

async fn run_server(path: PathBuf, cross_filesystems: bool, port: u16, no_open: bool, wait: bool) -> Result<()> {
    let (layout_tx, _) = broadcast::channel::<Vec<u8>>(64);
    let scan_root = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());

    let state = Arc::new(AppState {
        scan: ScanState {
            tree: RwLock::new(layout::DirTree::new()),
            version: AtomicU64::new(0),
            done: AtomicBool::new(false),
            root: scan_root,
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
            had_any: AtomicBool::new(false),
        },
        start: Notify::new(),
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

            let current_version = layout_state.scan.version.load(Ordering::Relaxed);
            let scan_done = layout_state.scan.done.load(Ordering::Relaxed);

            if current_version == last_version || layout_state.connections.count.load(Ordering::Relaxed) == 0 {
                continue;
            }
            if !scan_done && last_compute.elapsed() < Duration::from_millis(45) {
                continue;
            }

            let view = layout_state.view.lock().unwrap().clone();
            let (vw, vh) = view.viewport;
            if vw <= 0.0 || vh <= 0.0 {
                continue;
            }

            let tree = layout_state.scan.tree.read().unwrap();
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

            let message = protocol::encode_layout(root_size, dir_count, scan_done, &breadcrumb, &rects);
            *layout_state.layout.last.lock().unwrap() = Some(message.clone());
            let _ = layout_state.layout.tx.send(message);

            last_version = current_version;
            last_compute = Instant::now();
        }
    });

    if !wait {
        state.start.notify_one();
    }

    let static_dir = find_static_dir();

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/start", get(start_handler))
        .with_state(Arc::clone(&state))
        .fallback_service(ServeDir::new(&static_dir))
        .layer(CompressionLayer::new());

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    let actual_port = listener.local_addr()?.port();
    eprintln!("Listening on http://localhost:{actual_port}");

    if !no_open {
        let url = if wait {
            format!("http://localhost:{actual_port}/?wait")
        } else {
            format!("http://localhost:{actual_port}")
        };
        if let Err(e) = open::that(&url) {
            eprintln!("Failed to open browser: {e}");
        }
    }

    axum::serve(listener, app).await?;
    Ok(())
}

fn start_scan(state: &Arc<AppState>) {
    let (tx, rx) = std::sync::mpsc::channel::<ScanEvent>();

    let relay_state = Arc::clone(state);
    tokio::task::spawn_blocking(move || {
        while let Ok(event) = rx.recv() {
            let mut events = vec![event];
            while let Ok(extra) = rx.try_recv() {
                events.push(extra);
            }

            let mut tree = relay_state.scan.tree.write().unwrap();
            for event in events {
                match event {
                    ScanEvent::ScanStart { path } => {
                        tree.clear();
                        tree.scan_path = path.clone();
                        relay_state.view.lock().unwrap().view_root = None;
                        relay_state.scan.done.store(false, Ordering::Relaxed);
                        let _ = relay_state.layout.tx.send(protocol::encode_scan_start(&path));
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

    let path = state.scan.root.clone();
    let cross_filesystems = state.scan.cross_filesystems;
    tokio::task::spawn_blocking(move || {
        if let Err(e) = scanner::scan::scan(&path, cross_filesystems, tx) {
            eprintln!("Scan error: {e}");
        }
    });
}

fn find_static_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    if manifest.is_dir() {
        return manifest;
    }
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or(std::path::Path::new(".")).join("static");
        if dir.is_dir() {
            return dir;
        }
    }
    eprintln!("Warning: 'static/' directory not found");
    PathBuf::from(".")
}

async fn start_handler(axum::extract::State(state): axum::extract::State<Arc<AppState>>) -> &'static str {
    state.start.notify_one();
    "ok"
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> axum::response::Response {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    state.connections.count.fetch_add(1, Ordering::Relaxed);
    state.connections.had_any.store(true, Ordering::Relaxed);

    let last = state.layout.last.lock().unwrap().clone();
    if let Some(message) = last
        && socket.send(Message::Binary(message.into())).await.is_err()
    {
        return;
    }

    let mut layout_rx = state.layout.tx.subscribe();

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
        }
    }

    if state.connections.count.fetch_sub(1, Ordering::Relaxed) == 1 {
        let server = Arc::clone(&state);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if server.connections.count.load(Ordering::Relaxed) == 0 && server.connections.had_any.load(Ordering::Relaxed) {
                std::process::exit(0);
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
        let _ = std::process::Command::new("explorer")
            .arg("/select,")
            .arg(path)
            .spawn();
    }
}

fn handle_client_message(state: &Arc<AppState>, data: &[u8]) {
    let Some(msg) = ClientMessage::decode(data) else { return };
    match msg {
        ClientMessage::Viewport { width, height } => {
            state.view.lock().unwrap().viewport = (width as f64, height as f64);
            state.invalidate_layout();
        }
        ClientMessage::Navigate { id } => {
            state.view.lock().unwrap().view_root = Some(id);
            state.invalidate_layout();
        }
        ClientMessage::RevealDir { id } => {
            let tree = state.scan.tree.read().unwrap();
            if let Some(path) = tree.full_path(id, &state.scan.root) {
                drop(tree);
                reveal_in_file_manager(&path);
            }
        }
        ClientMessage::RevealFile { dir_id, name } => {
            let tree = state.scan.tree.read().unwrap();
            if let Some(path) = tree.full_path(dir_id, &state.scan.root).map(|p| p.join(name)) {
                drop(tree);
                reveal_in_file_manager(&path);
            }
        }
        ClientMessage::Rescan => {
            if state.scan.done.load(Ordering::Relaxed) {
                start_scan(state);
            }
        }
        ClientMessage::SetDepth { depth } => {
            state.view.lock().unwrap().max_depth = depth;
            state.invalidate_layout();
        }
        ClientMessage::ColorMode { mode } => {
            state.view.lock().unwrap().color_mode = mode;
            state.invalidate_layout();
        }
        ClientMessage::FilterExt { extensions } => {
            state.view.lock().unwrap().filter.extensions = extensions;
            state.invalidate_layout();
        }
        ClientMessage::FilterSize { min, max } => {
            let mut view = state.view.lock().unwrap();
            view.filter.min_size = min;
            view.filter.max_size = max;
            drop(view);
            state.invalidate_layout();
        }
        ClientMessage::FilterName { pattern } => {
            state.view.lock().unwrap().filter.name_pattern = pattern;
            state.invalidate_layout();
        }
        ClientMessage::ClearFilter => {
            state.view.lock().unwrap().filter = FilterConfig::default();
            state.invalidate_layout();
        }
    }
}
