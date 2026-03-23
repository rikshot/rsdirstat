use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Html;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast};

use crate::{layout, scan};

const INDEX_HTML: &str = include_str!("static/index.html");

const MSG_SCAN_START: u8 = 1;
const MSG_LAYOUT: u8 = 2;

struct ServerState {
    tree: RwLock<layout::DirTree>,
    tree_version: AtomicU64,
    viewport: Mutex<(f64, f64)>,
    view_root: Mutex<Option<u64>>,
    scan_done: AtomicBool,
    layout_tx: broadcast::Sender<Vec<u8>>,
    last_layout: Mutex<Option<Vec<u8>>>,
    layout_notify: Notify,
    start: Notify,
    shutdown: Notify,
    connections: AtomicU64,
    had_connection: AtomicBool,
    wait: bool,
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
    let mut buf = Vec::with_capacity(32 + breadcrumb.len() * 20 + rects.len() * 52);
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
        buf.extend_from_slice(&(r.x as f32).to_le_bytes());
        buf.extend_from_slice(&(r.y as f32).to_le_bytes());
        buf.extend_from_slice(&(r.w as f32).to_le_bytes());
        buf.extend_from_slice(&(r.h as f32).to_le_bytes());
        buf.extend_from_slice(&r.hue.to_le_bytes());
        buf.extend_from_slice(&r.size.to_le_bytes());
        buf.push(r.depth);
        buf.push((r.is_container as u8) | ((r.is_files as u8) << 1));
        buf.extend_from_slice(&(r.header_h as f32).to_le_bytes());
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

    let state = Arc::new(ServerState {
        tree: RwLock::new(layout::DirTree::new()),
        tree_version: AtomicU64::new(0),
        viewport: Mutex::new((0.0, 0.0)),
        view_root: Mutex::new(None),
        scan_done: AtomicBool::new(false),
        layout_tx,
        last_layout: Mutex::new(None),
        layout_notify: Notify::new(),
        start: Notify::new(),
        shutdown: Notify::new(),
        connections: AtomicU64::new(0),
        had_connection: AtomicBool::new(false),
        wait,
    });

    let scan_state = Arc::clone(&state);
    tokio::task::spawn(async move {
        scan_state.start.notified().await;

        let (tx, rx) = std::sync::mpsc::channel::<scan::ScanEvent>();

        let relay_state = Arc::clone(&scan_state);
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
                            relay_state.scan_done.store(false, Ordering::Relaxed);
                            let _ = relay_state.layout_tx.send(encode_scan_start(&path));
                        }
                        scan::ScanEvent::Dir {
                            id,
                            parent,
                            name,
                            size,
                        } => {
                            tree.insert_dir(id, parent, &name, size);
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

        tokio::task::spawn_blocking(move || {
            if let Err(e) = scan::scan_tree_streaming(&path, cross_filesystems, tx) {
                eprintln!("Scan error: {e}");
            }
        });
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

            if cur_version == last_version {
                continue;
            }
            if !scan_done && last_compute.elapsed() < Duration::from_millis(45) {
                continue;
            }

            let (vw, vh) = *layout_state.viewport.lock().unwrap();
            if vw <= 0.0 || vh <= 0.0 {
                continue;
            }

            let tree = layout_state.tree.read().unwrap();
            let root_id = match tree.root_id {
                Some(id) => id,
                None => continue,
            };
            let mut view_root = layout_state.view_root.lock().unwrap().unwrap_or(root_id);
            if tree.nodes.get(&view_root).is_none() {
                view_root = root_id;
            }
            let rects = layout::compute_layout(&tree, view_root, vw, vh);
            let breadcrumb = tree.breadcrumb(view_root);
            let root_size = tree.recursive_sizes.get(&view_root).copied().unwrap_or(0);
            let dir_count = tree.nodes.len() as u32;
            drop(tree);

            let msg = encode_layout(
                view_root,
                root_size,
                dir_count,
                scan_done,
                &breadcrumb,
                &rects,
            );

            *layout_state.last_layout.lock().unwrap() = Some(msg.clone());
            let _ = layout_state.layout_tx.send(msg);

            last_version = cur_version;
            last_compute = Instant::now();
        }
    });

    if !wait {
        state.start.notify_one();
    }

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/ws", get(ws_handler))
        .route("/start", get(start_handler))
        .with_state(Arc::clone(&state));

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    let actual_port = listener.local_addr()?.port();
    eprintln!("Listening on http://localhost:{actual_port}");

    if !no_open {
        let url = format!("http://localhost:{actual_port}");
        if let Err(e) = open::that(&url) {
            eprintln!("Failed to open browser: {e}");
        }
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            state.shutdown.notified().await;
        })
        .await?;
    Ok(())
}

async fn index_handler(
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
) -> Html<String> {
    let html = INDEX_HTML.replace("__WAIT_MODE__", if state.wait { "true" } else { "false" });
    Html(html)
}

async fn start_handler(
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
) -> &'static str {
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
    if let Some(msg) = last {
        if socket.send(Message::Binary(msg.into())).await.is_err() {
            return;
        }
    }

    let mut layout_rx = state.layout_tx.subscribe();

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_client_message(&state, text.as_ref());
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
            if s.connections.load(Ordering::Relaxed) == 0
                && s.had_connection.load(Ordering::Relaxed)
            {
                s.shutdown.notify_one();
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

fn handle_client_message(state: &ServerState, text: &str) {
    let parsed: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };
    match parsed["type"].as_str() {
        Some("viewport") => {
            let w = parsed["w"].as_f64().unwrap_or(0.0);
            let h = parsed["h"].as_f64().unwrap_or(0.0);
            *state.viewport.lock().unwrap() = (w, h);
            state.invalidate_layout();
        }
        Some("navigate") => {
            if let Some(id) = parsed["id"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
            {
                *state.view_root.lock().unwrap() = Some(id);
                state.invalidate_layout();
            }
        }
        _ => {}
    }
}
