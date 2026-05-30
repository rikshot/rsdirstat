#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreadcrumbEntry {
    pub id: u64,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayoutRect {
    pub id: i64,
    pub parent_id: u64,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub name: String,
    pub hue: u16,
    pub size: u64,
    pub depth: u8,
    pub is_container: bool,
    pub header_height: f32,
    pub is_files: bool,
    pub is_file: bool,
    pub mtime: i64,
}

#[derive(Clone, Debug, PartialEq)]
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

// Server → client message tags
pub const MSG_SCAN_START: u8 = 1;
pub const MSG_LAYOUT: u8 = 2;
pub const MSG_PICKER_MODE: u8 = 3;

#[derive(Clone, Debug, PartialEq)]
pub enum ServerMessage {
    ScanStart { path: String },
    Layout(LayoutPayload),
    PickerMode,
}

/// Client → server WebSocket messages.
#[derive(Clone, Debug, PartialEq)]
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

/// Minimum encoded size of a breadcrumb entry (u64 id + u16 name length, empty name).
const BREADCRUMB_MIN_BYTES: usize = 8 + 2;
/// Minimum encoded size of a layout rect (all fixed fields + u16 name length, empty name).
const RECT_MIN_BYTES: usize = 8 + 8 + 4 + 4 + 4 + 4 + 2 + 8 + 1 + 1 + 4 + 8 + 2;

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
const MSG_SCAN_PATH: u8 = 12;

fn encode_u16_string(tag: u8, value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut buf = Vec::with_capacity(3 + bytes.len());
    buf.push(tag);
    buf.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(bytes);
    buf
}

fn decode_u16_string(data: &[u8], offset: &mut usize) -> Option<String> {
    if *offset + 2 > data.len() {
        return None;
    }
    let len = u16::from_le_bytes(data[*offset..*offset + 2].try_into().ok()?) as usize;
    *offset += 2;
    if *offset + len > data.len() {
        return None;
    }
    let value = std::str::from_utf8(&data[*offset..*offset + len]).ok()?.to_string();
    *offset += len;
    Some(value)
}

pub fn encode_viewport(width: f32, height: f32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(9);
    buf.push(MSG_VIEWPORT);
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf
}

pub fn encode_navigate(id: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(9);
    buf.push(MSG_NAVIGATE);
    buf.extend_from_slice(&id.to_le_bytes());
    buf
}

pub fn encode_reveal_dir(id: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(9);
    buf.push(MSG_REVEAL_DIR);
    buf.extend_from_slice(&id.to_le_bytes());
    buf
}

pub fn encode_reveal_file(parent_id: u64, name: &str) -> Vec<u8> {
    let name_bytes = name.as_bytes();
    let mut buf = Vec::with_capacity(11 + name_bytes.len());
    buf.push(MSG_REVEAL_FILE);
    buf.extend_from_slice(&parent_id.to_le_bytes());
    buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(name_bytes);
    buf
}

pub fn encode_rescan() -> Vec<u8> {
    vec![MSG_RESCAN]
}

pub fn encode_set_depth(depth: u8) -> Vec<u8> {
    vec![MSG_SET_DEPTH, depth]
}

pub fn encode_color_mode(mode: u8) -> Vec<u8> {
    vec![MSG_COLOR_MODE, mode]
}

pub fn encode_filter_ext<S: AsRef<str>>(extensions: &[S]) -> Vec<u8> {
    // The count and each length are u8-prefixed, so drop extensions that don't fit rather than
    // truncating a length byte and desyncing the rest of the frame.
    let kept: Vec<&[u8]> = extensions
        .iter()
        .map(|ext| ext.as_ref().as_bytes())
        .filter(|bytes| bytes.len() <= u8::MAX as usize)
        .take(u8::MAX as usize)
        .collect();
    let payload_len = kept.iter().map(|bytes| 1 + bytes.len()).sum::<usize>();
    let mut buf = Vec::with_capacity(2 + payload_len);
    buf.push(MSG_FILTER_EXT);
    buf.push(kept.len() as u8);
    for bytes in kept {
        buf.push(bytes.len() as u8);
        buf.extend_from_slice(bytes);
    }
    buf
}

pub fn encode_filter_size(min: u64, max: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(17);
    buf.push(MSG_FILTER_SIZE);
    buf.extend_from_slice(&min.to_le_bytes());
    buf.extend_from_slice(&max.to_le_bytes());
    buf
}

pub fn encode_filter_name(pattern: &str) -> Vec<u8> {
    encode_u16_string(MSG_FILTER_NAME, pattern)
}

pub fn encode_clear_filter() -> Vec<u8> {
    vec![MSG_CLEAR_FILTER]
}

pub fn encode_scan_path(path: &str) -> Vec<u8> {
    encode_u16_string(MSG_SCAN_PATH, path)
}

pub fn encode_picker_mode() -> Vec<u8> {
    vec![MSG_PICKER_MODE]
}

pub fn encode_scan_start(path: &str) -> Vec<u8> {
    encode_u16_string(MSG_SCAN_START, path)
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
        buf.extend_from_slice(&rect.x.to_le_bytes());
        buf.extend_from_slice(&rect.y.to_le_bytes());
        buf.extend_from_slice(&rect.w.to_le_bytes());
        buf.extend_from_slice(&rect.h.to_le_bytes());
        buf.extend_from_slice(&rect.hue.to_le_bytes());
        buf.extend_from_slice(&rect.size.to_le_bytes());
        buf.push(rect.depth);
        buf.push((rect.is_container as u8) | ((rect.is_files as u8) << 1) | ((rect.is_file as u8) << 2));
        buf.extend_from_slice(&rect.header_height.to_le_bytes());
        buf.extend_from_slice(&rect.mtime.to_le_bytes());
        let name_bytes = rect.name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
    }
    buf
}

pub fn decode_server_message(data: &[u8]) -> Option<ServerMessage> {
    if data.is_empty() {
        return None;
    }

    let mut offset = 1;
    match data[0] {
        MSG_SCAN_START => Some(ServerMessage::ScanStart {
            path: decode_u16_string(data, &mut offset)?,
        }),
        MSG_PICKER_MODE => Some(ServerMessage::PickerMode),
        MSG_LAYOUT => {
            if offset + 8 + 4 + 1 + 2 > data.len() {
                return None;
            }

            let root_size = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
            offset += 8;
            let dir_count = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
            offset += 4;
            let scan_done = data[offset] != 0;
            offset += 1;

            let breadcrumb_count = u16::from_le_bytes(data[offset..offset + 2].try_into().ok()?) as usize;
            offset += 2;
            // Cap the reservation against the bytes that remain so a bogus count can't trigger a
            // huge allocation; the per-entry bounds checks below still reject a truncated frame.
            let breadcrumb_cap = breadcrumb_count.min((data.len() - offset) / BREADCRUMB_MIN_BYTES + 1);
            let mut breadcrumb = Vec::with_capacity(breadcrumb_cap);
            for _ in 0..breadcrumb_count {
                if offset + 8 > data.len() {
                    return None;
                }
                let id = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
                offset += 8;
                let name = decode_u16_string(data, &mut offset)?;
                breadcrumb.push(BreadcrumbEntry { id, name });
            }

            if offset + 4 > data.len() {
                return None;
            }
            let rect_count = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?) as usize;
            offset += 4;
            let rect_cap = rect_count.min((data.len() - offset) / RECT_MIN_BYTES + 1);
            let mut rects = Vec::with_capacity(rect_cap);
            for _ in 0..rect_count {
                if offset + 8 + 8 + 4 + 4 + 4 + 4 + 2 + 8 + 1 + 1 + 4 + 8 + 2 > data.len() {
                    return None;
                }

                let id = i64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
                offset += 8;
                let parent_id = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
                offset += 8;
                let x = f32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
                offset += 4;
                let y = f32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
                offset += 4;
                let w = f32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
                offset += 4;
                let h = f32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
                offset += 4;
                let hue = u16::from_le_bytes(data[offset..offset + 2].try_into().ok()?);
                offset += 2;
                let size = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
                offset += 8;
                let depth = data[offset];
                offset += 1;
                let flags = data[offset];
                offset += 1;
                let header_height = f32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
                offset += 4;
                let mtime = i64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
                offset += 8;
                let name = decode_u16_string(data, &mut offset)?;

                rects.push(LayoutRect {
                    id,
                    parent_id,
                    x,
                    y,
                    w,
                    h,
                    name,
                    hue,
                    size,
                    depth,
                    is_container: (flags & 1) != 0,
                    header_height,
                    is_files: (flags & 2) != 0,
                    is_file: (flags & 4) != 0,
                    mtime,
                });
            }

            Some(ServerMessage::Layout(LayoutPayload {
                root_size,
                dir_count,
                scan_done,
                breadcrumb,
                rects,
            }))
        }
        _ => None,
    }
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
            MSG_COLOR_MODE if data.len() >= 2 => Some(ClientMessage::ColorMode {
                // Only Type (0) and Age (1) are defined; treat anything else as Type.
                mode: if data[1] <= 1 { data[1] } else { 0 },
            }),
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
            MSG_SCAN_PATH if data.len() >= 3 => {
                let len = u16::from_le_bytes(data[1..3].try_into().ok()?) as usize;
                if data.len() < 3 + len {
                    return None;
                }
                let path = std::str::from_utf8(&data[3..3 + len]).ok()?.to_string();
                Some(ClientMessage::ScanPath { path })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_viewport_round_trips_through_decoder() {
        let message = encode_viewport(800.0, 600.0);
        assert_eq!(
            ClientMessage::decode(&message),
            Some(ClientMessage::Viewport {
                width: 800.0,
                height: 600.0,
            })
        );
    }

    #[test]
    fn encode_navigate_round_trips_through_decoder() {
        let message = encode_navigate(42);
        assert_eq!(
            ClientMessage::decode(&message),
            Some(ClientMessage::Navigate { id: 42 })
        );
    }

    #[test]
    fn encode_scan_start_basic() {
        let buf = encode_scan_start("/home/user");
        assert_eq!(buf[0], MSG_SCAN_START);
        let len = u16::from_le_bytes([buf[1], buf[2]]);
        assert_eq!(len, 10);
        assert_eq!(&buf[3..], b"/home/user");
    }

    #[test]
    fn encode_scan_start_empty_path() {
        let buf = encode_scan_start("");
        assert_eq!(buf[0], MSG_SCAN_START);
        let len = u16::from_le_bytes([buf[1], buf[2]]);
        assert_eq!(len, 0);
        assert_eq!(buf.len(), 3);
    }

    #[test]
    fn encode_layout_header_fields() {
        let breadcrumb = vec![BreadcrumbEntry {
            id: 42,
            name: "root".into(),
        }];
        let rects = vec![LayoutRect {
            id: -1,
            parent_id: 0,
            x: 1.0,
            y: 2.0,
            w: 100.0,
            h: 50.0,
            name: "dir".into(),
            hue: 120,
            size: 9999,
            depth: 3,
            is_container: true,
            is_files: false,
            is_file: false,
            header_height: 20.0,
            mtime: 1700000000,
        }];
        let buf = encode_layout(5000, 7, true, &breadcrumb, &rects);

        assert_eq!(buf[0], MSG_LAYOUT);
        assert_eq!(u64::from_le_bytes(buf[1..9].try_into().unwrap()), 5000);
        assert_eq!(u32::from_le_bytes(buf[9..13].try_into().unwrap()), 7);
        assert_eq!(buf[13], 1);
        assert_eq!(u16::from_le_bytes(buf[14..16].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(buf[16..24].try_into().unwrap()), 42);
        assert_eq!(u16::from_le_bytes(buf[24..26].try_into().unwrap()), 4);
        assert_eq!(&buf[26..30], b"root");
        assert_eq!(u32::from_le_bytes(buf[30..34].try_into().unwrap()), 1);
    }

    #[test]
    fn decode_scan_start_message() {
        assert_eq!(
            decode_server_message(&encode_scan_start("/tmp/test")),
            Some(ServerMessage::ScanStart {
                path: "/tmp/test".into(),
            })
        );
    }

    #[test]
    fn decode_picker_mode_message() {
        assert_eq!(
            decode_server_message(&encode_picker_mode()),
            Some(ServerMessage::PickerMode)
        );
    }

    #[test]
    fn decode_layout_message_round_trip() {
        let breadcrumb = vec![BreadcrumbEntry {
            id: 7,
            name: "root".into(),
        }];
        let rects = vec![LayoutRect {
            id: 42,
            parent_id: 7,
            x: 10.5,
            y: 20.5,
            w: 100.0,
            h: 200.0,
            name: "src".into(),
            hue: 120,
            size: 5000,
            depth: 2,
            is_container: true,
            header_height: 18.0,
            is_files: false,
            is_file: false,
            mtime: 1_700_000_000,
        }];

        let encoded = encode_layout(10_000, 3, true, &breadcrumb, &rects);
        let Some(ServerMessage::Layout(decoded)) = decode_server_message(&encoded) else {
            panic!("expected layout message");
        };

        assert_eq!(decoded.root_size, 10_000);
        assert_eq!(decoded.dir_count, 3);
        assert!(decoded.scan_done);
        assert_eq!(decoded.breadcrumb, breadcrumb);
        assert_eq!(decoded.rects, rects);
    }

    #[test]
    fn decode_truncated_layout_message_returns_none() {
        let encoded = encode_layout(10_000, 3, true, &[], &[]);
        let truncated = &encoded[..encoded.len() - 1];
        assert!(decode_server_message(truncated).is_none());
    }

    #[test]
    fn encode_scan_path_unicode_round_trips() {
        let message = encode_scan_path("/tmp/日本語");
        assert_eq!(
            ClientMessage::decode(&message),
            Some(ClientMessage::ScanPath {
                path: "/tmp/日本語".into(),
            })
        );
    }

    #[test]
    fn decode_reveal_dir() {
        let mut data = vec![MSG_REVEAL_DIR];
        data.extend_from_slice(&99u64.to_le_bytes());
        let Some(ClientMessage::RevealDir { id }) = ClientMessage::decode(&data) else {
            panic!("expected RevealDir");
        };
        assert_eq!(id, 99);
    }

    #[test]
    fn decode_reveal_file() {
        let mut data = vec![MSG_REVEAL_FILE];
        data.extend_from_slice(&5u64.to_le_bytes());
        let name = b"hello.txt";
        data.extend_from_slice(&(name.len() as u16).to_le_bytes());
        data.extend_from_slice(name);
        let Some(ClientMessage::RevealFile { dir_id, name }) = ClientMessage::decode(&data) else {
            panic!("expected RevealFile");
        };
        assert_eq!(dir_id, 5);
        assert_eq!(name, "hello.txt");
    }

    #[test]
    fn decode_rescan() {
        assert!(matches!(
            ClientMessage::decode(&[MSG_RESCAN]),
            Some(ClientMessage::Rescan)
        ));
    }

    #[test]
    fn decode_set_depth() {
        let data = vec![MSG_SET_DEPTH, 5];
        let Some(ClientMessage::SetDepth { depth }) = ClientMessage::decode(&data) else {
            panic!("expected SetDepth");
        };
        assert_eq!(depth, 5);
    }

    #[test]
    fn decode_color_mode() {
        let data = vec![MSG_COLOR_MODE, 1];
        let Some(ClientMessage::ColorMode { mode }) = ClientMessage::decode(&data) else {
            panic!("expected ColorMode");
        };
        assert_eq!(mode, 1);
    }

    #[test]
    fn decode_color_mode_clamps_unknown_to_type() {
        let data = vec![MSG_COLOR_MODE, 7];
        let Some(ClientMessage::ColorMode { mode }) = ClientMessage::decode(&data) else {
            panic!("expected ColorMode");
        };
        assert_eq!(mode, 0);
    }

    #[test]
    fn decode_filter_ext_single() {
        let mut data = vec![MSG_FILTER_EXT, 1];
        data.push(2);
        data.extend_from_slice(b"rs");
        let Some(ClientMessage::FilterExt { extensions }) = ClientMessage::decode(&data) else {
            panic!("expected FilterExt");
        };
        assert_eq!(extensions.len(), 1);
        assert_eq!(&*extensions[0], "rs");
    }

    #[test]
    fn decode_filter_size() {
        let mut data = vec![MSG_FILTER_SIZE];
        data.extend_from_slice(&1024u64.to_le_bytes());
        data.extend_from_slice(&u64::MAX.to_le_bytes());
        let Some(ClientMessage::FilterSize { min, max }) = ClientMessage::decode(&data) else {
            panic!("expected FilterSize");
        };
        assert_eq!(min, 1024);
        assert_eq!(max, u64::MAX);
    }

    #[test]
    fn decode_filter_name() {
        let pattern = b"Foo";
        let mut data = vec![MSG_FILTER_NAME];
        data.extend_from_slice(&(pattern.len() as u16).to_le_bytes());
        data.extend_from_slice(pattern);
        let Some(ClientMessage::FilterName { pattern }) = ClientMessage::decode(&data) else {
            panic!("expected FilterName");
        };
        assert_eq!(pattern, "foo");
    }

    #[test]
    fn decode_clear_filter() {
        assert!(matches!(
            ClientMessage::decode(&[MSG_CLEAR_FILTER]),
            Some(ClientMessage::ClearFilter)
        ));
    }

    #[test]
    fn decode_empty_data_returns_none() {
        assert!(ClientMessage::decode(&[]).is_none());
    }

    #[test]
    fn decode_unknown_tag_returns_none() {
        assert!(ClientMessage::decode(&[255]).is_none());
    }

    #[test]
    fn decode_truncated_viewport_returns_none() {
        let mut data = vec![MSG_VIEWPORT];
        data.extend_from_slice(&100.0f32.to_le_bytes());
        assert!(ClientMessage::decode(&data).is_none());
    }

    #[test]
    fn decode_truncated_navigate_returns_none() {
        let data = vec![MSG_NAVIGATE, 0, 0, 0, 0];
        assert!(ClientMessage::decode(&data).is_none());
    }

    #[test]
    fn decode_truncated_reveal_file_name_returns_none() {
        let mut data = vec![MSG_REVEAL_FILE];
        data.extend_from_slice(&1u64.to_le_bytes());
        data.extend_from_slice(&100u16.to_le_bytes());
        data.extend_from_slice(b"abc");
        assert!(ClientMessage::decode(&data).is_none());
    }

    #[test]
    fn decode_truncated_filter_name_returns_none() {
        let mut data = vec![MSG_FILTER_NAME];
        data.extend_from_slice(&50u16.to_le_bytes());
        assert!(ClientMessage::decode(&data).is_none());
    }

    #[test]
    fn set_depth_clamped_zero_to_one() {
        let data = vec![MSG_SET_DEPTH, 0];
        let Some(ClientMessage::SetDepth { depth }) = ClientMessage::decode(&data) else {
            panic!("expected SetDepth");
        };
        assert_eq!(depth, 1);
    }

    #[test]
    fn set_depth_clamped_255_to_10() {
        let data = vec![MSG_SET_DEPTH, 255];
        let Some(ClientMessage::SetDepth { depth }) = ClientMessage::decode(&data) else {
            panic!("expected SetDepth");
        };
        assert_eq!(depth, 10);
    }

    #[test]
    fn filter_name_lowercases_input() {
        let pattern = b"FooBar.TXT";
        let mut data = vec![MSG_FILTER_NAME];
        data.extend_from_slice(&(pattern.len() as u16).to_le_bytes());
        data.extend_from_slice(pattern);
        let Some(ClientMessage::FilterName { pattern }) = ClientMessage::decode(&data) else {
            panic!("expected FilterName");
        };
        assert_eq!(pattern, "foobar.txt");
    }

    #[test]
    fn filter_ext_multiple_extensions() {
        let mut data = vec![MSG_FILTER_EXT, 3];
        for ext in &[b"rs" as &[u8], b"toml", b"json"] {
            data.push(ext.len() as u8);
            data.extend_from_slice(ext);
        }
        let Some(ClientMessage::FilterExt { extensions }) = ClientMessage::decode(&data) else {
            panic!("expected FilterExt");
        };
        assert_eq!(extensions.len(), 3);
        assert_eq!(&*extensions[0], "rs");
        assert_eq!(&*extensions[1], "toml");
        assert_eq!(&*extensions[2], "json");
    }

    #[test]
    fn encode_filter_ext_drops_overlong_extension() {
        let long = "a".repeat(300);
        let message = encode_filter_ext(&[long.as_str(), "rs"]);
        let Some(ClientMessage::FilterExt { extensions }) = ClientMessage::decode(&message) else {
            panic!("expected FilterExt");
        };
        assert_eq!(extensions.len(), 1);
        assert_eq!(&*extensions[0], "rs");
    }

    #[test]
    fn decode_scan_path() {
        let path = b"/home/user";
        let mut data = vec![MSG_SCAN_PATH];
        data.extend_from_slice(&(path.len() as u16).to_le_bytes());
        data.extend_from_slice(path);
        let Some(ClientMessage::ScanPath { path }) = ClientMessage::decode(&data) else {
            panic!("expected ScanPath");
        };
        assert_eq!(path, "/home/user");
    }

    #[test]
    fn decode_truncated_scan_path_returns_none() {
        let mut data = vec![MSG_SCAN_PATH];
        data.extend_from_slice(&50u16.to_le_bytes());
        data.extend_from_slice(b"short");
        assert!(ClientMessage::decode(&data).is_none());
    }
}
