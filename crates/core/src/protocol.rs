use crate::layout::LayoutRect;
use crate::tree::BreadcrumbEntry;

/// Scanner → server internal events (not sent over WebSocket).
pub enum ScanEvent {
    ScanStart {
        path: String,
    },
    Dir {
        id: u64,
        parent: u64,
        name: String,
        size: u64,
        mtime: i64,
    },
    File {
        parent: u64,
        name: String,
        size: u64,
        mtime: i64,
    },
    ScanDone,
}

// Server → client message tags
pub const MSG_SCAN_START: u8 = 1;
pub const MSG_LAYOUT: u8 = 2;

/// Client → server WebSocket messages.
pub enum ClientMessage {
    Viewport { width: f32, height: f32 },
    Navigate { id: u64 },
    RevealDir { id: u64 },
    RevealFile { dir_id: u64, name: String },
    Rescan,
    SetDepth { depth: u8 },
    ColorMode { mode: u8 },
    FilterExt { extensions: Vec<Box<str>> },
    FilterSize { min: u64, max: u64 },
    FilterName { pattern: String },
    ClearFilter,
}

const MSG_VIEWPORT: u8 = 1;
const MSG_NAVIGATE: u8 = 2;
const MSG_REVEAL_DIR: u8 = 3;
const MSG_REVEAL_FILE: u8 = 4;
const MSG_RESCAN: u8 = 5;
const MSG_SET_DEPTH: u8 = 6;
const MSG_COLOR_MODE: u8 = 7;
const MSG_FILTER_EXT: u8 = 8;
const MSG_FILTER_SIZE: u8 = 9;
const MSG_FILTER_NAME: u8 = 10;
const MSG_CLEAR_FILTER: u8 = 11;

pub fn encode_scan_start(path: &str) -> Vec<u8> {
    let path_bytes = path.as_bytes();
    let mut buf = Vec::with_capacity(3 + path_bytes.len());
    buf.push(MSG_SCAN_START);
    buf.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(path_bytes);
    buf
}

pub fn encode_layout(
    root_size: u64,
    dir_count: u32,
    scan_done: bool,
    breadcrumb: &[BreadcrumbEntry],
    rects: &[LayoutRect],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(24 + breadcrumb.len() * 20 + rects.len() * 68);
    buf.push(MSG_LAYOUT);
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

impl ClientMessage {
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.is_empty() {
            return None;
        }
        match data[0] {
            MSG_VIEWPORT if data.len() >= 9 => {
                let width = f32::from_le_bytes(data[1..5].try_into().ok()?);
                let height = f32::from_le_bytes(data[5..9].try_into().ok()?);
                Some(ClientMessage::Viewport { width, height })
            }
            MSG_NAVIGATE if data.len() >= 9 => {
                let id = u64::from_le_bytes(data[1..9].try_into().ok()?);
                Some(ClientMessage::Navigate { id })
            }
            MSG_REVEAL_DIR if data.len() >= 9 => {
                let id = u64::from_le_bytes(data[1..9].try_into().ok()?);
                Some(ClientMessage::RevealDir { id })
            }
            MSG_REVEAL_FILE if data.len() >= 11 => {
                let dir_id = u64::from_le_bytes(data[1..9].try_into().ok()?);
                let name_len = u16::from_le_bytes(data[9..11].try_into().ok()?) as usize;
                if data.len() < 11 + name_len {
                    return None;
                }
                let name = std::str::from_utf8(&data[11..11 + name_len]).ok()?.to_string();
                Some(ClientMessage::RevealFile { dir_id, name })
            }
            MSG_RESCAN => Some(ClientMessage::Rescan),
            MSG_SET_DEPTH if data.len() >= 2 => Some(ClientMessage::SetDepth {
                depth: data[1].clamp(1, 10),
            }),
            MSG_COLOR_MODE if data.len() >= 2 => Some(ClientMessage::ColorMode { mode: data[1] }),
            MSG_FILTER_EXT if data.len() >= 2 => {
                let count = data[1] as usize;
                let mut offset = 2;
                let mut extensions = Vec::with_capacity(count);
                for _ in 0..count {
                    if offset >= data.len() {
                        break;
                    }
                    let len = data[offset] as usize;
                    offset += 1;
                    if offset + len > data.len() {
                        break;
                    }
                    if let Ok(s) = std::str::from_utf8(&data[offset..offset + len]) {
                        extensions.push(s.into());
                    }
                    offset += len;
                }
                Some(ClientMessage::FilterExt { extensions })
            }
            MSG_FILTER_SIZE if data.len() >= 17 => {
                let min = u64::from_le_bytes(data[1..9].try_into().ok()?);
                let max = u64::from_le_bytes(data[9..17].try_into().ok()?);
                Some(ClientMessage::FilterSize { min, max })
            }
            MSG_FILTER_NAME if data.len() >= 3 => {
                let len = u16::from_le_bytes(data[1..3].try_into().ok()?) as usize;
                if data.len() < 3 + len {
                    return None;
                }
                let pattern = std::str::from_utf8(&data[3..3 + len]).ok()?.to_ascii_lowercase();
                Some(ClientMessage::FilterName { pattern })
            }
            MSG_CLEAR_FILTER => Some(ClientMessage::ClearFilter),
            _ => None,
        }
    }
}
