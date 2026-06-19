use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreadcrumbEntry {
    pub id: u64,
    pub name: String,
}

/// Layout geometry is kept in f64 (layout-space precision); it is shared by the layout engine,
/// the server, and the client and serialized as-is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutRect {
    pub id: i64,
    pub parent_id: u64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub name: String,
    pub hue: u16,
    pub size: u64,
    pub depth: u8,
    pub is_container: bool,
    pub header_height: f64,
    pub is_files: bool,
    pub is_file: bool,
    pub mtime: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutPayload {
    pub root_size: u64,
    pub dir_count: u32,
    pub scan_done: bool,
    pub breadcrumb: Vec<BreadcrumbEntry>,
    pub rects: Vec<LayoutRect>,
}

/// Scanner → server internal events (not sent over WebSocket).
#[derive(Debug)]
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

/// Server → client WebSocket messages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    ScanStart { path: String },
    Layout(LayoutPayload),
    PickerMode,
}

impl ServerMessage {
    pub fn encode(&self) -> Vec<u8> {
        pack(postcard::to_allocvec(self).expect("ServerMessage must serialize"))
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        postcard::from_bytes(&unpack(data)?).ok()
    }
}

/// Client → server WebSocket messages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    ScanPath { path: String },
}

impl ClientMessage {
    pub fn encode(&self) -> Vec<u8> {
        pack(postcard::to_allocvec(self).expect("ClientMessage must serialize"))
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        let mut msg: ClientMessage = postcard::from_bytes(&unpack(data)?).ok()?;
        msg.normalize();
        Some(msg)
    }

    /// Canonicalize incoming values regardless of what a client sent: clamp the depth and color
    /// mode to their valid ranges and lower-case the name filter for case-insensitive matching.
    fn normalize(&mut self) {
        match self {
            ClientMessage::SetDepth { depth } => *depth = (*depth).clamp(1, 10),
            ClientMessage::ColorMode { mode } if *mode > 1 => *mode = 0,
            ClientMessage::FilterName { pattern } => *pattern = pattern.to_ascii_lowercase(),
            _ => {}
        }
    }
}

const COMPRESSION_LEVEL: u8 = 6;
/// Sanity backstop against a malicious/corrupt frame inflating to an unreasonable size.
const MAX_DECOMPRESSED: usize = 256 * 1024 * 1024;

/// Prefix a payload with a 1-byte flag: 1 = deflated, 0 = raw. Compression is only kept when it
/// actually shrinks the payload, so tiny control messages never expand.
fn pack(bytes: Vec<u8>) -> Vec<u8> {
    let compressed = miniz_oxide::deflate::compress_to_vec(&bytes, COMPRESSION_LEVEL);
    let mut out = Vec::with_capacity(compressed.len().min(bytes.len()) + 1);
    if compressed.len() < bytes.len() {
        out.push(1);
        out.extend_from_slice(&compressed);
    } else {
        out.push(0);
        out.extend_from_slice(&bytes);
    }
    out
}

fn unpack(data: &[u8]) -> Option<Vec<u8>> {
    let (&flag, rest) = data.split_first()?;
    match flag {
        0 => Some(rest.to_vec()),
        1 => miniz_oxide::inflate::decompress_to_vec_with_limit(rest, MAX_DECOMPRESSED).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_roundtrip(msg: ClientMessage) -> ClientMessage {
        ClientMessage::decode(&msg.encode()).expect("client message should round-trip")
    }

    fn server_roundtrip(msg: ServerMessage) -> ServerMessage {
        ServerMessage::decode(&msg.encode()).expect("server message should round-trip")
    }

    fn sample_rects(count: usize) -> Vec<LayoutRect> {
        (0..count)
            .map(|i| LayoutRect {
                id: i as i64,
                parent_id: 0,
                x: i as f64 * 1.5,
                y: 2.0,
                w: 100.0,
                h: 50.0,
                name: format!("dir-{i}"),
                hue: (i % 360) as u16,
                size: 1000 + i as u64,
                depth: (i % 8) as u8,
                is_container: true,
                header_height: 18.0,
                is_files: false,
                is_file: false,
                mtime: 1_700_000_000,
            })
            .collect()
    }

    #[test]
    fn viewport_round_trips() {
        assert_eq!(
            client_roundtrip(ClientMessage::Viewport {
                width: 800.0,
                height: 600.0,
            }),
            ClientMessage::Viewport {
                width: 800.0,
                height: 600.0,
            }
        );
    }

    #[test]
    fn navigate_round_trips() {
        assert_eq!(
            client_roundtrip(ClientMessage::Navigate { id: 42 }),
            ClientMessage::Navigate { id: 42 }
        );
    }

    #[test]
    fn reveal_file_round_trips() {
        let msg = ClientMessage::RevealFile {
            dir_id: 5,
            name: "hello.txt".into(),
        };
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn scan_path_unicode_round_trips() {
        let msg = ClientMessage::ScanPath {
            path: "/tmp/日本語".into(),
        };
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn rescan_and_clear_filter_round_trip() {
        assert_eq!(client_roundtrip(ClientMessage::Rescan), ClientMessage::Rescan);
        assert_eq!(client_roundtrip(ClientMessage::ClearFilter), ClientMessage::ClearFilter);
    }

    #[test]
    fn set_depth_is_clamped() {
        assert_eq!(
            client_roundtrip(ClientMessage::SetDepth { depth: 5 }),
            ClientMessage::SetDepth { depth: 5 }
        );
        assert_eq!(
            client_roundtrip(ClientMessage::SetDepth { depth: 0 }),
            ClientMessage::SetDepth { depth: 1 }
        );
        assert_eq!(
            client_roundtrip(ClientMessage::SetDepth { depth: 255 }),
            ClientMessage::SetDepth { depth: 10 }
        );
    }

    #[test]
    fn color_mode_clamps_unknown_to_type() {
        assert_eq!(
            client_roundtrip(ClientMessage::ColorMode { mode: 1 }),
            ClientMessage::ColorMode { mode: 1 }
        );
        assert_eq!(
            client_roundtrip(ClientMessage::ColorMode { mode: 7 }),
            ClientMessage::ColorMode { mode: 0 }
        );
    }

    #[test]
    fn filter_name_is_lowercased() {
        assert_eq!(
            client_roundtrip(ClientMessage::FilterName {
                pattern: "FooBar.TXT".into(),
            }),
            ClientMessage::FilterName {
                pattern: "foobar.txt".into(),
            }
        );
    }

    #[test]
    fn filter_ext_round_trips() {
        let msg = ClientMessage::FilterExt {
            extensions: vec!["rs".into(), "toml".into(), "json".into()],
        };
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn filter_size_round_trips() {
        let msg = ClientMessage::FilterSize {
            min: 1024,
            max: u64::MAX,
        };
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn scan_start_and_picker_mode_round_trip() {
        let msg = ServerMessage::ScanStart {
            path: "/tmp/test".into(),
        };
        assert_eq!(server_roundtrip(msg.clone()), msg);
        assert_eq!(server_roundtrip(ServerMessage::PickerMode), ServerMessage::PickerMode);
    }

    #[test]
    fn small_layout_round_trips() {
        let msg = ServerMessage::Layout(LayoutPayload {
            root_size: 10_000,
            dir_count: 3,
            scan_done: true,
            breadcrumb: vec![BreadcrumbEntry {
                id: 7,
                name: "root".into(),
            }],
            rects: sample_rects(1),
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn large_layout_round_trips_and_compresses() {
        let payload = LayoutPayload {
            root_size: 5_000_000,
            dir_count: 1000,
            scan_done: false,
            breadcrumb: vec![BreadcrumbEntry {
                id: 0,
                name: String::new(),
            }],
            rects: sample_rects(1000),
        };
        let msg = ServerMessage::Layout(payload);
        let encoded = msg.encode();
        // The repetitive rect stream should compress (flag byte 1).
        assert_eq!(encoded[0], 1, "large layout should be deflated");
        assert!(encoded.len() < postcard::to_allocvec(&msg).unwrap().len());
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn decode_empty_or_garbage_returns_none() {
        assert!(ClientMessage::decode(&[]).is_none());
        assert!(ServerMessage::decode(&[]).is_none());
        // Flag byte present but body is not a valid message.
        assert!(ClientMessage::decode(&[0, 0xff, 0xff, 0xff]).is_none());
        // Unknown compression flag.
        assert!(ClientMessage::decode(&[2, 0, 0]).is_none());
    }
}
