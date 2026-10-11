use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreadcrumbEntry {
    pub id: u64,
    pub name: String,
}

/// What a laid-out rect stands for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RectKind {
    /// A folder drawn with its contents inside, below a header strip of
    /// [`LayoutRect::header_height`].
    Container,
    /// A folder drawn as one block: at the depth limit, or too small to open.
    #[default]
    Dir,
    File,
    /// One folder's files that are too small to draw on their own, summed into one block.
    OtherFiles,
}

impl RectKind {
    /// A folder, drawn open or as one block.
    pub fn is_dir(self) -> bool {
        matches!(self, Self::Container | Self::Dir)
    }

    pub fn is_container(self) -> bool {
        self == Self::Container
    }
}

/// One rect of a treemap layout, in canvas pixels. Geometry is f32: a canvas is at most a few
/// thousand pixels across, so it is exact to well under a pixel, and half the size of f64 on the
/// wire.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LayoutRect {
    /// A folder's node id. 0 for a file or "(other files)" block: those are identified by
    /// `parent_id` and `name`, which are sent anyway, and a random id per file would cost ~9
    /// poorly-compressible bytes each.
    pub id: u64,
    pub parent_id: u64,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub name: String,
    pub hue: u16,
    pub size: u64,
    pub depth: u8,
    pub kind: RectKind,
    pub mtime: i64,
}

impl LayoutRect {
    /// Height of the header strip above a container's contents (0 for any other rect). The layout
    /// engine and the client both compute it, so it isn't sent.
    pub fn header_height(&self) -> f32 {
        if self.kind.is_container() {
            header_height(self.w, self.h)
        } else {
            0.0
        }
    }
}

/// Height of the header strip above a container of `width` × `height`: 15% of the height, at most
/// 18px, and none when the result would be under 12px or the container is small.
fn header_height(width: f32, height: f32) -> f32 {
    const MAX_HEADER: f32 = 18.0;
    if width > 60.0 && height > 30.0 {
        let header = (height * 0.15).floor().min(MAX_HEADER);
        if header >= 12.0 {
            return header;
        }
    }
    0.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutPayload {
    pub root_size: u64,
    pub dir_count: u32,
    pub scan_done: bool,
    /// The scan stopped before reading everything (it stalled on a stuck directory), so the tree is
    /// missing whatever it hadn't reached. Only once `scan_done`.
    pub partial: bool,
    /// How long the scan has been running, or took once done, in milliseconds. From the server, so a
    /// tab that joins mid-scan, or after it, shows the scan's own time.
    pub scan_ms: u32,
    /// The server's path separator, for showing the paths of what's in the view: the page can't
    /// tell it from a root like `projects`.
    pub separator: char,
    pub breadcrumb: Vec<BreadcrumbEntry>,
    pub rects: Vec<LayoutRect>,
}

/// Default number of treemap levels shown (the frontend's `#depth` select starts on it too).
pub const DEFAULT_DEPTH: u8 = 6;
/// Highest selectable depth; `SetDepth` is clamped to `1..=MAX_DEPTH`.
pub const MAX_DEPTH: u8 = 10;
/// Largest viewport side the server lays out, in CSS pixels: past any real display.
pub const MAX_VIEWPORT: f32 = 16_384.0;
/// What the treemap colours rects by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorMode {
    /// By file type; a folder drawn as one block by its name.
    #[default]
    Type,
    /// By modification time, from red (oldest) to green (newest).
    Age,
}

/// A mounted volume, as the `/volumes` endpoint lists them (`encode_list`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub name: String,
    pub mount_point: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub fs_type: String,
}

impl VolumeInfo {
    /// `used_bytes` is `total - free`, saturating in case a filesystem reports more free than total.
    pub fn new(name: String, mount_point: String, total_bytes: u64, free_bytes: u64, fs_type: String) -> Self {
        Self {
            name,
            mount_point,
            total_bytes,
            used_bytes: total_bytes.saturating_sub(free_bytes),
            fs_type,
        }
    }

    /// The `/volumes` response: `volumes` in the same encoding as the WebSocket messages.
    pub fn encode_list(volumes: &[VolumeInfo]) -> Vec<u8> {
        pack(postcard::to_allocvec(volumes).expect("volumes must serialize"))
    }

    pub fn decode_list(data: &[u8]) -> Option<Vec<VolumeInfo>> {
        postcard::from_bytes(&unpack(data)?).ok()
    }
}

/// Server → client WebSocket messages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// A new scan replaced the tree: the page clears its treemap. Comes before the scan's first
    /// layout.
    ScanStart,
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
    ColorMode { mode: ColorMode },
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

    /// Canonicalize incoming values regardless of what a client sent: clamp the depth to its valid
    /// range, lower-case the name filter for case-insensitive matching, and
    /// take extensions without the dot (or `*.`) people often type before them.
    fn normalize(&mut self) {
        match self {
            ClientMessage::Viewport { width, height } => {
                // A treemap the size of a NaN, or of a million pixels (where every file would be a
                // rect), is never what a page meant; 0 lays out nothing.
                for side in [width, height] {
                    *side = if side.is_finite() {
                        side.clamp(0.0, MAX_VIEWPORT)
                    } else {
                        0.0
                    };
                }
            }
            ClientMessage::SetDepth { depth } => *depth = (*depth).clamp(1, MAX_DEPTH),
            ClientMessage::FilterName { pattern } => *pattern = pattern.to_ascii_lowercase(),
            ClientMessage::FilterExt { extensions } => {
                for extension in extensions.iter_mut() {
                    if extension.starts_with(['*', '.']) {
                        *extension = extension.trim_start_matches(['*', '.']).into();
                    }
                }
                extensions.retain(|extension| !extension.is_empty());
            }
            _ => {}
        }
    }
}

/// DEFLATE level for `pack`. Kept low on purpose: each tab gets a layout up to ~20×/sec during a
/// scan, so compression must stay cheap. Level 1's fast/greedy matching skips the expensive
/// LZ77 search that dominates higher levels (it was ~37% of server CPU at level 6 on a large scan),
/// while still shrinking the repetitive rect stream well. `unpack` inflates identically regardless
/// of the level the sender used.
const COMPRESSION_LEVEL: u8 = 1;
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
        let kinds = [RectKind::Container, RectKind::Dir, RectKind::File, RectKind::OtherFiles];
        (0..count)
            .map(|i| LayoutRect {
                id: i as u64,
                parent_id: 0,
                x: i as f32 * 1.5,
                y: 2.0,
                w: 100.0,
                h: 50.0,
                name: format!("dir-{i}"),
                hue: (i % 360) as u16,
                size: 1000 + i as u64,
                depth: (i % 8) as u8,
                kind: kinds[i % kinds.len()],
                mtime: 1_700_000_000,
            })
            .collect()
    }

    #[test]
    fn header_height_zero_for_small_rect() {
        assert_eq!(header_height(60.0, 100.0), 0.0);
        assert_eq!(header_height(100.0, 30.0), 0.0);
        assert_eq!(header_height(50.0, 20.0), 0.0);
    }

    #[test]
    fn header_height_capped_at_18() {
        assert_eq!(header_height(200.0, 200.0), 18.0);
    }

    #[test]
    fn header_height_proportional_in_mid_range() {
        assert_eq!(header_height(200.0, 100.0), 15.0);
    }

    #[test]
    fn header_height_zero_when_below_min() {
        assert_eq!(header_height(200.0, 40.0), 0.0);
    }

    #[test]
    fn only_containers_have_a_header() {
        let rect = |kind| LayoutRect {
            w: 200.0,
            h: 200.0,
            kind,
            ..LayoutRect::default()
        };
        assert_eq!(rect(RectKind::Container).header_height(), 18.0);
        for kind in [RectKind::Dir, RectKind::File, RectKind::OtherFiles] {
            assert_eq!(rect(kind).header_height(), 0.0);
        }
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
    fn viewport_is_clamped_to_a_drawable_size() {
        let viewport = |width, height| match client_roundtrip(ClientMessage::Viewport { width, height }) {
            ClientMessage::Viewport { width, height } => (width, height),
            other => panic!("{other:?}"),
        };
        assert_eq!(viewport(f32::NAN, 600.0), (0.0, 600.0));
        assert_eq!(viewport(f32::INFINITY, -5.0), (0.0, 0.0));
        assert_eq!(viewport(1e9, 720.0), (MAX_VIEWPORT, 720.0));
        assert_eq!(viewport(1280.0, 720.0), (1280.0, 720.0));
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
    fn color_modes_round_trip_and_unknown_ones_are_refused() {
        for mode in [ColorMode::Type, ColorMode::Age] {
            let msg = ClientMessage::ColorMode { mode };
            assert_eq!(client_roundtrip(msg.clone()), msg);
        }
        let mut unknown = postcard::to_allocvec(&ClientMessage::ColorMode { mode: ColorMode::Age }).unwrap();
        *unknown.last_mut().unwrap() = 7;
        assert_eq!(ClientMessage::decode(&pack(unknown)), None);
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
    fn extensions_arrive_without_their_dot() {
        let msg = ClientMessage::FilterExt {
            extensions: vec![".mp4".into(), "*.MOV".into(), "rs".into(), ".".into(), "tar.gz".into()],
        };
        let expected = ClientMessage::FilterExt {
            extensions: vec!["mp4".into(), "MOV".into(), "rs".into(), "tar.gz".into()],
        };
        assert_eq!(client_roundtrip(msg), expected);
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
    fn volume_lists_round_trip() {
        let volumes = vec![
            VolumeInfo::new("Macintosh HD".into(), "/".into(), 1_000, 400, "apfs".into()),
            VolumeInfo::new("データ".into(), "/Volumes/データ".into(), 10, 20, "exfat".into()),
        ];
        assert_eq!(
            VolumeInfo::decode_list(&VolumeInfo::encode_list(&volumes)),
            Some(volumes)
        );
        assert_eq!(VolumeInfo::decode_list(&[]), None);
    }

    #[test]
    fn scan_start_and_picker_mode_round_trip() {
        assert_eq!(server_roundtrip(ServerMessage::ScanStart), ServerMessage::ScanStart);
        assert_eq!(server_roundtrip(ServerMessage::PickerMode), ServerMessage::PickerMode);
    }

    #[test]
    fn small_layout_round_trips() {
        let msg = ServerMessage::Layout(LayoutPayload {
            root_size: 10_000,
            dir_count: 3,
            scan_done: true,
            partial: true,
            scan_ms: 4_200,
            separator: '\\',
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
            partial: false,
            scan_ms: 0,
            separator: '/',
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
