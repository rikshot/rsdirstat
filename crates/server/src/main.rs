#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::routing::get;
use clap::Parser;
use humansize::{BINARY, format_size};
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast};
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;

use rsdirstat_core::layout::{self, FilterConfig};
use rsdirstat_core::protocol::{self, ClientMessage, ScanEvent};

#[derive(Parser)]
#[command(name = "rsdirstat", about = "Blazing fast disk usage scanner for macOS")]
struct Args {
    /// Path to scan
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Show individual files instead of directories
    #[arg(long)]
    files: bool,

    /// Number of results to display
    #[arg(long, default_value_t = 10)]
    top: usize,

    /// Cross filesystem boundaries
    #[arg(long)]
    all: bool,

    /// Launch web GUI with interactive treemap
    #[arg(long)]
    gui: bool,

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

struct ServerState {
    tree: RwLock<layout::DirTree>,
    tree_version: AtomicU64,
    viewport: Mutex<(f64, f64)>,
    view_root: Mutex<Option<u64>>,
    scan_done: AtomicBool,
    scan_root: PathBuf,
    cross_filesystems: bool,
    layout_tx: broadcast::Sender<Vec<u8>>,
    last_layout: Mutex<Option<Vec<u8>>>,
    layout_notify: Notify,
    start: Notify,
    connections: AtomicU64,
    had_connection: AtomicBool,
    max_depth: AtomicU8,
    color_mode: AtomicU8,
    filter: Mutex<FilterConfig>,
}

impl ServerState {
    fn invalidate_layout(&self) {
        self.tree_version.fetch_add(1, Ordering::Relaxed);
        self.layout_notify.notify_one();
    }
}

fn encode_scan_start(path: &str) -> Vec<u8> {
    let path_bytes = path.as_bytes();
    let mut buf = Vec::with_capacity(3 + path_bytes.len());
    buf.push(protocol::MSG_SCAN_START);
    buf.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(path_bytes);
    buf
}

fn encode_layout(
    view_root: u64,
    root_size: u64,
    dir_count: u32,
    scan_done: bool,
    breadcrumb: &[layout::BreadcrumbEntry],
    rects: &[layout::LayoutRect],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32 + breadcrumb.len() * 20 + rects.len() * 68);
    buf.push(protocol::MSG_LAYOUT);
    buf.extend_from_slice(&view_root.to_le_bytes());
    buf.extend_from_slice(&root_size.to_le_bytes());
    buf.extend_from_slice(&dir_count.to_le_bytes());
    buf.push(scan_done as u8);
    buf.extend_from_slice(&(breadcrumb.len() as u16).to_le_bytes());
    for entry in breadcrumb {
        buf.extend_from_slice(&entry.id.to_le_bytes());
        let name_bytes = entry.name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
    }
    buf.extend_from_slice(&(rects.len() as u32).to_le_bytes());
    for rect in rects {
        buf.extend_from_slice(&rect.id.to_le_bytes());
        buf.extend_from_slice(&rect.parent_id.to_le_bytes());
        buf.extend_from_slice(&(rect.x as f32).to_le_bytes());
        buf.extend_from_slice(&(rect.y as f32).to_le_bytes());
        buf.extend_from_slice(&(rect.w as f32).to_le_bytes());
        buf.extend_from_slice(&(rect.h as f32).to_le_bytes());
        buf.extend_from_slice(&rect.hue.to_le_bytes());
        buf.extend_from_slice(&rect.size.to_le_bytes());
        buf.push(rect.depth);
        buf.push((rect.is_container as u8) | ((rect.is_files as u8) << 1) | ((rect.is_file as u8) << 2));
        buf.extend_from_slice(&(rect.header_height as f32).to_le_bytes());
        buf.extend_from_slice(&rect.mtime.to_le_bytes());
        let name_bytes = rect.name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
    }
    buf
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.gui {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(run_server(args.path, args.all, args.port, args.no_open, args.wait))?;
        return Ok(());
    }

    let result = rsdirstat_macos::scan::scan(&args.path, args.files, args.all, args.top)?;

    let mut entries: Vec<(PathBuf, u64)> = if args.files {
        result.file_entries
    } else {
        result.dir_sizes.into_iter().collect()
    };

    entries.sort_unstable_by(|a, b| b.1.cmp(&a.1));

    let formatted: Vec<(String, &PathBuf)> = entries
        .iter()
        .map(|(path, size)| (format_size(*size, BINARY), path))
        .collect();

    let max_width = formatted.iter().map(|(s, _)| s.len()).max().unwrap_or(0);

    for (size_str, path) in &formatted {
        println!("{size_str:>width$}  {}", path.display(), width = max_width);
    }

    Ok(())
}

async fn run_server(path: PathBuf, cross_filesystems: bool, port: u16, no_open: bool, wait: bool) -> Result<()> {
    let (layout_tx, _) = broadcast::channel::<Vec<u8>>(64);
    let scan_root = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());

    let state = Arc::new(ServerState {
        tree: RwLock::new(layout::DirTree::new()),
        tree_version: AtomicU64::new(0),
        viewport: Mutex::new((0.0, 0.0)),
        view_root: Mutex::new(None),
        scan_done: AtomicBool::new(false),
        scan_root,
        cross_filesystems,
        layout_tx,
        last_layout: Mutex::new(None),
        layout_notify: Notify::new(),
        start: Notify::new(),
        connections: AtomicU64::new(0),
        had_connection: AtomicBool::new(false),
        max_depth: AtomicU8::new(5),
        color_mode: AtomicU8::new(0),
        filter: Mutex::new(FilterConfig::default()),
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
                _ = layout_state.layout_notify.notified() => {},
                _ = tokio::time::sleep(Duration::from_millis(50)) => {},
            }

            let current_version = layout_state.tree_version.load(Ordering::Relaxed);
            let scan_done = layout_state.scan_done.load(Ordering::Relaxed);

            if current_version == last_version || layout_state.connections.load(Ordering::Relaxed) == 0 {
                continue;
            }
            if !scan_done && last_compute.elapsed() < Duration::from_millis(45) {
                continue;
            }

            let (vw, vh) = *layout_state.viewport.lock().unwrap();
            if vw <= 0.0 || vh <= 0.0 {
                continue;
            }

            let max_depth = layout_state.max_depth.load(Ordering::Relaxed);
            let color_mode = layout_state.color_mode.load(Ordering::Relaxed);
            let filter = layout_state.filter.lock().unwrap().clone();

            let tree = layout_state.tree.read().unwrap();
            let root_id = match tree.root_id {
                Some(id) => id,
                None => continue,
            };
            let mut view_root = layout_state.view_root.lock().unwrap().unwrap_or(root_id);
            if !tree.nodes.contains_key(&view_root) {
                view_root = root_id;
            }

            let config = layout::LayoutConfig {
                max_depth,
                color_mode,
                filter,
                mtime_range: tree.mtime_range,
            };
            let rects = layout::compute_layout(&tree, view_root, vw, vh, &config);
            let breadcrumb = tree.breadcrumb(view_root);
            let root_size = tree.recursive_sizes.get(&view_root).copied().unwrap_or(0);
            let dir_count = tree.nodes.len() as u32;
            drop(tree);

            let message = encode_layout(view_root, root_size, dir_count, scan_done, &breadcrumb, &rects);
            *layout_state.last_layout.lock().unwrap() = Some(message.clone());
            let _ = layout_state.layout_tx.send(message);

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

fn start_scan(state: &Arc<ServerState>) {
    let (tx, rx) = std::sync::mpsc::channel::<ScanEvent>();

    let relay_state = Arc::clone(state);
    tokio::task::spawn_blocking(move || {
        while let Ok(event) = rx.recv() {
            let mut events = vec![event];
            while let Ok(extra) = rx.try_recv() {
                events.push(extra);
            }

            let mut tree = relay_state.tree.write().unwrap();
            for event in events {
                match event {
                    ScanEvent::ScanStart { path } => {
                        tree.clear();
                        tree.scan_path = path.clone();
                        *relay_state.view_root.lock().unwrap() = None;
                        relay_state.scan_done.store(false, Ordering::Relaxed);
                        let _ = relay_state.layout_tx.send(encode_scan_start(&path));
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
                        relay_state.scan_done.store(true, Ordering::Relaxed);
                    }
                }
            }
            drop(tree);
            relay_state.invalidate_layout();
        }
    });

    let path = state.scan_root.clone();
    let cross_filesystems = state.cross_filesystems;
    tokio::task::spawn_blocking(move || {
        if let Err(e) = rsdirstat_macos::scan::scan_tree_streaming(&path, cross_filesystems, tx) {
            eprintln!("Scan error: {e}");
        }
    });
}

fn find_static_dir() -> PathBuf {
    // Compile-time path: crates/server/static
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    if manifest.is_dir() {
        return manifest;
    }
    // Next to the executable (for release installs)
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or(std::path::Path::new(".")).join("static");
        if dir.is_dir() {
            return dir;
        }
    }
    eprintln!("Warning: 'static/' directory not found");
    PathBuf::from(".")
}

async fn start_handler(axum::extract::State(state): axum::extract::State<Arc<ServerState>>) -> &'static str {
    state.start.notify_one();
    "ok"
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
) -> axum::response::Response {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<ServerState>) {
    state.connections.fetch_add(1, Ordering::Relaxed);
    state.had_connection.store(true, Ordering::Relaxed);

    let last = state.last_layout.lock().unwrap().clone();
    if let Some(message) = last
        && socket.send(Message::Binary(message.into())).await.is_err()
    {
        return;
    }

    let mut layout_rx = state.layout_tx.subscribe();

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

    if state.connections.fetch_sub(1, Ordering::Relaxed) == 1 {
        let server = Arc::clone(&state);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if server.connections.load(Ordering::Relaxed) == 0 && server.had_connection.load(Ordering::Relaxed) {
                std::process::exit(0);
            }
        });
    }
}

fn reveal_in_finder(path: &std::path::Path) {
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
}

fn handle_client_message(state: &Arc<ServerState>, data: &[u8]) {
    let Some(msg) = ClientMessage::decode(data) else { return };
    match msg {
        ClientMessage::Viewport { width, height } => {
            *state.viewport.lock().unwrap() = (width as f64, height as f64);
            state.invalidate_layout();
        }
        ClientMessage::Navigate { id } => {
            *state.view_root.lock().unwrap() = Some(id);
            state.invalidate_layout();
        }
        ClientMessage::RevealDir { id } => {
            let tree = state.tree.read().unwrap();
            if let Some(path) = tree.full_path(id, &state.scan_root) {
                drop(tree);
                reveal_in_finder(&path);
            }
        }
        ClientMessage::RevealFile { dir_id, name } => {
            let tree = state.tree.read().unwrap();
            if let Some(path) = tree.full_path(dir_id, &state.scan_root).map(|p| p.join(name)) {
                drop(tree);
                reveal_in_finder(&path);
            }
        }
        ClientMessage::Rescan => {
            if state.scan_done.load(Ordering::Relaxed) {
                start_scan(state);
            }
        }
        ClientMessage::SetDepth { depth } => {
            state.max_depth.store(depth, Ordering::Relaxed);
            state.invalidate_layout();
        }
        ClientMessage::ColorMode { mode } => {
            state.color_mode.store(mode, Ordering::Relaxed);
            state.invalidate_layout();
        }
        ClientMessage::FilterExt { extensions } => {
            state.filter.lock().unwrap().extensions = extensions;
            state.invalidate_layout();
        }
        ClientMessage::FilterSize { min, max } => {
            let mut f = state.filter.lock().unwrap();
            f.min_size = min;
            f.max_size = max;
            state.invalidate_layout();
        }
        ClientMessage::FilterName { pattern } => {
            state.filter.lock().unwrap().name_pattern = pattern;
            state.invalidate_layout();
        }
        ClientMessage::ClearFilter => {
            *state.filter.lock().unwrap() = FilterConfig::default();
            state.invalidate_layout();
        }
    }
}
