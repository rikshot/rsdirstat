use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast};
use tower_http::services::ServeDir;

use crate::{layout, scan};

const MSG_SCAN_START: u8 = 1;
const MSG_LAYOUT: u8 = 2;

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
    filter: Mutex<layout::FilterConfig>,
}

fn encode_scan_start(path: &str) -> Vec<u8> {
    let pb = path.as_bytes();
    let mut buf = Vec::with_capacity(3 + pb.len());
    buf.push(MSG_SCAN_START);
    buf.extend_from_slice(&(pb.len() as u16).to_le_bytes());
    buf.extend_from_slice(pb);
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
    buf.push(MSG_LAYOUT);
    buf.extend_from_slice(&view_root.to_le_bytes());
    buf.extend_from_slice(&root_size.to_le_bytes());
    buf.extend_from_slice(&dir_count.to_le_bytes());
    buf.push(scan_done as u8);
    buf.extend_from_slice(&(breadcrumb.len() as u16).to_le_bytes());
    for bc in breadcrumb {
        buf.extend_from_slice(&bc.id.to_le_bytes());
        let nb = bc.name.as_bytes();
        buf.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        buf.extend_from_slice(nb);
    }
    buf.extend_from_slice(&(rects.len() as u32).to_le_bytes());
    for r in rects {
        buf.extend_from_slice(&r.id.to_le_bytes());
        buf.extend_from_slice(&r.parent_id.to_le_bytes());
        buf.extend_from_slice(&(r.x as f32).to_le_bytes());
        buf.extend_from_slice(&(r.y as f32).to_le_bytes());
        buf.extend_from_slice(&(r.w as f32).to_le_bytes());
        buf.extend_from_slice(&(r.h as f32).to_le_bytes());
        buf.extend_from_slice(&r.hue.to_le_bytes());
        buf.extend_from_slice(&r.size.to_le_bytes());
        buf.push(r.depth);
        buf.push((r.is_container as u8) | ((r.is_files as u8) << 1) | ((r.is_file as u8) << 2));
        buf.extend_from_slice(&(r.header_h as f32).to_le_bytes());
        buf.extend_from_slice(&r.mtime.to_le_bytes());
        let nb = r.name.as_bytes();
        buf.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        buf.extend_from_slice(nb);
    }
    buf
}

pub async fn run_streaming(
    path: PathBuf,
    cross_filesystems: bool,
    port: u16,
    no_open: bool,
    wait: bool,
) -> anyhow::Result<()> {
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
        filter: Mutex::new(layout::FilterConfig::default()),
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

            let cur_version = layout_state.tree_version.load(Ordering::Relaxed);
            let scan_done = layout_state.scan_done.load(Ordering::Relaxed);

            if cur_version == last_version || layout_state.connections.load(Ordering::Relaxed) == 0 {
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
            let filter = {
                let f = layout_state.filter.lock().unwrap();
                if f.is_active() {
                    f.clone()
                } else {
                    layout::FilterConfig::default()
                }
            };

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

            let msg = encode_layout(view_root, root_size, dir_count, scan_done, &breadcrumb, &rects);

            *layout_state.last_layout.lock().unwrap() = Some(msg.clone());
            let _ = layout_state.layout_tx.send(msg);

            last_version = cur_version;
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
        .fallback_service(ServeDir::new(&static_dir));

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
    let (tx, rx) = std::sync::mpsc::channel::<scan::ScanEvent>();

    let relay_state = Arc::clone(state);
    tokio::task::spawn_blocking(move || {
        while let Ok(event) = rx.recv() {
            let mut events = vec![event];
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }

            let mut tree = relay_state.tree.write().unwrap();
            for event in events {
                match event {
                    scan::ScanEvent::ScanStart { path } => {
                        tree.clear();
                        tree.scan_path = path.clone();
                        *relay_state.view_root.lock().unwrap() = None;
                        relay_state.scan_done.store(false, Ordering::Relaxed);
                        let _ = relay_state.layout_tx.send(encode_scan_start(&path));
                    }
                    scan::ScanEvent::Dir {
                        id,
                        parent,
                        name,
                        size,
                        mtime,
                    } => {
                        tree.insert_dir(id, parent, &name, size, mtime);
                    }
                    scan::ScanEvent::File {
                        parent,
                        name,
                        size,
                        mtime,
                    } => {
                        tree.insert_file(parent, &name, size, mtime);
                    }
                    scan::ScanEvent::ScanDone => {
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
    let cross_fs = state.cross_filesystems;
    tokio::task::spawn_blocking(move || {
        if let Err(e) = scan::scan_tree_streaming(&path, cross_fs, tx) {
            eprintln!("Scan error: {e}");
        }
    });
}

fn find_static_dir() -> PathBuf {
    // Check relative to executable, then current dir
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or(std::path::Path::new(".")).join("static");
        if dir.is_dir() {
            return dir;
        }
    }
    let cwd = PathBuf::from("static");
    if cwd.is_dir() {
        return cwd;
    }
    eprintln!("Warning: 'static/' directory not found, serving from current directory");
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
    if let Some(msg) = last
        && socket.send(Message::Binary(msg.into())).await.is_err()
    {
        return;
    }

    let mut layout_rx = state.layout_tx.subscribe();

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        handle_client_message(&state, &data);
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
            result = layout_rx.recv() => {
                match result {
                    Ok(msg) => {
                        if socket.send(Message::Binary(msg.into())).await.is_err() {
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
        let s = Arc::clone(&state);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if s.connections.load(Ordering::Relaxed) == 0 && s.had_connection.load(Ordering::Relaxed) {
                std::process::exit(0);
            }
        });
    }
}

impl ServerState {
    fn invalidate_layout(&self) {
        self.tree_version.fetch_add(1, Ordering::Relaxed);
        self.layout_notify.notify_one();
    }
}

const MSG_CLIENT_VIEWPORT: u8 = 1;
const MSG_CLIENT_NAVIGATE: u8 = 2;
const MSG_CLIENT_REVEAL_DIR: u8 = 3;
const MSG_CLIENT_REVEAL_FILE: u8 = 4;
const MSG_CLIENT_RESCAN: u8 = 5;
const MSG_CLIENT_SET_DEPTH: u8 = 6;
const MSG_CLIENT_COLOR_MODE: u8 = 7;
const MSG_CLIENT_FILTER_EXT: u8 = 8;
const MSG_CLIENT_FILTER_SIZE: u8 = 9;
const MSG_CLIENT_FILTER_NAME: u8 = 10;
const MSG_CLIENT_CLEAR_FILTER: u8 = 11;

fn reveal_in_finder(path: &std::path::Path) {
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
}

fn handle_client_message(state: &Arc<ServerState>, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    match data[0] {
        MSG_CLIENT_VIEWPORT if data.len() >= 9 => {
            let w = f32::from_le_bytes(data[1..5].try_into().unwrap()) as f64;
            let h = f32::from_le_bytes(data[5..9].try_into().unwrap()) as f64;
            *state.viewport.lock().unwrap() = (w, h);
            state.invalidate_layout();
        }
        MSG_CLIENT_NAVIGATE if data.len() >= 9 => {
            let id = u64::from_le_bytes(data[1..9].try_into().unwrap());
            *state.view_root.lock().unwrap() = Some(id);
            state.invalidate_layout();
        }
        MSG_CLIENT_REVEAL_DIR if data.len() >= 9 => {
            let id = u64::from_le_bytes(data[1..9].try_into().unwrap());
            let tree = state.tree.read().unwrap();
            let path = tree.full_path(id, &state.scan_root);
            drop(tree);
            if let Some(path) = path {
                reveal_in_finder(&path);
            }
        }
        MSG_CLIENT_REVEAL_FILE if data.len() >= 11 => {
            let dir_id = u64::from_le_bytes(data[1..9].try_into().unwrap());
            let name_len = u16::from_le_bytes(data[9..11].try_into().unwrap()) as usize;
            if data.len() >= 11 + name_len {
                let name = std::str::from_utf8(&data[11..11 + name_len]).unwrap_or("");
                let tree = state.tree.read().unwrap();
                let path = tree.full_path(dir_id, &state.scan_root).map(|p| p.join(name));
                drop(tree);
                if let Some(path) = path {
                    reveal_in_finder(&path);
                }
            }
        }
        MSG_CLIENT_RESCAN => {
            if state.scan_done.load(Ordering::Relaxed) {
                start_scan(state);
            }
        }
        MSG_CLIENT_SET_DEPTH if data.len() >= 2 => {
            let depth = data[1].clamp(1, 10);
            state.max_depth.store(depth, Ordering::Relaxed);
            state.invalidate_layout();
        }
        MSG_CLIENT_COLOR_MODE if data.len() >= 2 => {
            state.color_mode.store(data[1], Ordering::Relaxed);
            state.invalidate_layout();
        }
        MSG_CLIENT_FILTER_EXT if data.len() >= 2 => {
            let count = data[1] as usize;
            let mut off = 2;
            let mut exts = Vec::with_capacity(count);
            for _ in 0..count {
                if off >= data.len() {
                    break;
                }
                let len = data[off] as usize;
                off += 1;
                if off + len > data.len() {
                    break;
                }
                if let Ok(s) = std::str::from_utf8(&data[off..off + len]) {
                    exts.push(s.into());
                }
                off += len;
            }
            state.filter.lock().unwrap().extensions = exts;
            state.invalidate_layout();
        }
        MSG_CLIENT_FILTER_SIZE if data.len() >= 17 => {
            let min = u64::from_le_bytes(data[1..9].try_into().unwrap());
            let max = u64::from_le_bytes(data[9..17].try_into().unwrap());
            let mut f = state.filter.lock().unwrap();
            f.min_size = min;
            f.max_size = max;
            state.invalidate_layout();
        }
        MSG_CLIENT_FILTER_NAME if data.len() >= 3 => {
            let len = u16::from_le_bytes(data[1..3].try_into().unwrap()) as usize;
            if data.len() >= 3 + len {
                let pattern = std::str::from_utf8(&data[3..3 + len])
                    .unwrap_or("")
                    .to_ascii_lowercase();
                state.filter.lock().unwrap().name_pattern = pattern;
                state.invalidate_layout();
            }
        }
        MSG_CLIENT_CLEAR_FILTER => {
            *state.filter.lock().unwrap() = layout::FilterConfig::default();
            state.invalidate_layout();
        }
        _ => {}
    }
}
