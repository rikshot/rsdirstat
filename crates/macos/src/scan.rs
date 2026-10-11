use std::ffi::{CStr, CString};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use rsdirstat_core::scan::{
    DirJob, DirSink, ScanEvent, ScanOutcome, node_id, open_dir, open_dir_at, raise_fd_limit, root_name, run_scan,
};

const VREG: u32 = 1;
const VDIR: u32 = 2;

// st_flags bit (from <sys/stat.h>, not exposed by libc) marking a "dataless" object: a cloud
// placeholder (Dropbox/Drive/iCloud) whose data isn't materialized locally. Its logical length is
// the full "online" size while 0 bytes are allocated on disk, so we count it as 0. Read-only flag,
// surfaced via ATTR_CMN_FLAGS; reading attributes never triggers a download.
const SF_DATALESS: u32 = 0x4000_0000;

static SCAN_ATTRS: libc::attrlist = libc::attrlist {
    bitmapcount: libc::ATTR_BIT_MAP_COUNT,
    reserved: 0,
    commonattr: libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_MODTIME
        | libc::ATTR_CMN_FLAGS
        | libc::ATTR_CMN_FILEID,
    volattr: 0,
    dirattr: 0,
    // DATALENGTH (logical data-fork length), not TOTALSIZE: the latter adds resource-fork bytes,
    // which Linux (stx_size) and Windows (EndOfFile) don't count, so the same tree would total
    // differently per platform. DATALENGTH matches their apparent-size semantics. LINKCOUNT lets us
    // skip hardlinks already counted under another name.
    fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_DATALENGTH,
    forkattr: 0,
};

pub fn scan(root: &Path, cross_filesystems: bool, tx: std::sync::mpsc::Sender<ScanEvent>) -> Result<ScanOutcome> {
    scan_cancellable(root, cross_filesystems, tx, Arc::default())
}

/// Scan, aborting promptly if `cancel` is set. The server trips this when a scan is superseded by a
/// rescan or a new `ScanPath` so the old worker threads stop walking the filesystem.
pub fn scan_cancellable(
    root: &Path,
    cross_filesystems: bool,
    tx: std::sync::mpsc::Sender<ScanEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<ScanOutcome> {
    raise_fd_limit();
    let path = std::fs::canonicalize(root).context("failed to canonicalize root path")?;
    let meta = std::fs::metadata(&path).context("failed to stat root path")?;
    let fd = open_dir(&path).context("failed to open root directory")?;
    let root = DirJob {
        handle: fd,
        id: node_id(meta.dev(), meta.ino()),
        parent_id: 0,
        name: root_name(&path),
        path: path.display().to_string(),
    };
    let root_dev = meta.dev() as i32;
    Ok(run_scan(root, tx, cancel, move |dir, buffer, sink| {
        read_dir(dir, buffer, sink, root_dev, cross_filesystems)
    }))
}

/// Read one directory with `getattrlistbulk`, which returns entries and their attributes in
/// batches, so files need no further syscalls.
fn read_dir(
    dir: &DirJob<OwnedFd>,
    buffer: &mut [u8],
    sink: &mut DirSink<'_, OwnedFd>,
    root_dev: i32,
    cross_filesystems: bool,
) {
    loop {
        let count = unsafe {
            libc::getattrlistbulk(
                dir.handle.as_raw_fd(),
                &SCAN_ATTRS as *const libc::attrlist as *mut libc::c_void,
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
                libc::FSOPT_NOFOLLOW as u64,
            )
        };

        if count < 0 {
            eprintln!(
                "getattrlistbulk failed for {}: {}",
                dir.name,
                std::io::Error::last_os_error()
            );
            break;
        }
        if count == 0 {
            break;
        }

        let mut offset = 0usize;
        for _ in 0..count {
            if offset + 4 > buffer.len() {
                break;
            }

            let entry_length = u32::from_ne_bytes(buffer[offset..offset + 4].try_into().unwrap()) as usize;
            if entry_length == 0 || offset + entry_length > buffer.len() {
                break;
            }

            let entry = &buffer[offset..offset + entry_length];
            if let Some(parsed) = parse_entry(entry)
                && parsed.name != "."
                && parsed.name != ".."
                && (cross_filesystems || parsed.dev_id == root_dev)
            {
                let id = || node_id(parsed.dev_id as u32 as u64, parsed.file_id);
                match parsed.obj_type {
                    VDIR => {
                        if let Some(child) = CString::new(parsed.name)
                            .ok()
                            .and_then(|name| open_dir_at(&dir.handle, &name))
                        {
                            sink.dir(id(), parsed.name, sink.child_path(parsed.name), child);
                        }
                    }
                    VREG => {
                        // Unmaterialized cloud placeholders occupy 0 bytes on disk; count them as
                        // such rather than their "online" logical length.
                        let size = if parsed.dataless { 0 } else { parsed.file_size };
                        sink.file(parsed.name, size, parsed.mtime, (parsed.nlink > 1).then(id));
                    }
                    _ => {}
                }
            }

            offset += entry_length;
        }

        if !sink.checkpoint() {
            break;
        }
    }
}

struct ParsedEntry<'a> {
    name: &'a str,
    dev_id: i32,
    obj_type: u32,
    file_id: u64,
    file_size: u64,
    nlink: u32,
    mtime: i64,
    dataless: bool,
}

/// Parse one `getattrlistbulk` entry. The layout is dynamic: a leading `attribute_set_t`
/// (ATTR_CMN_RETURNED_ATTRS) reports which requested attributes were actually returned, and each
/// attribute's bytes are present only if its bit is set, in ascending bit order per group. We must
/// honor that bitmap rather than assume every field is present — a filesystem that omits one would
/// otherwise shift every later field and corrupt the parse.
// The final `field!` advances `pos` past the last attribute even though nothing reads it after —
// the uniform macro is clearer than special-casing the tail.
#[allow(unused_assignments)]
fn parse_entry(entry: &[u8]) -> Option<ParsedEntry<'_>> {
    // entry[0..4] is the u32 entry length (used by the caller for framing); skip it.
    let mut pos = 4;

    // ATTR_CMN_RETURNED_ATTRS: attribute_set_t = 5 × u32 (common, vol, dir, file, fork bitmaps).
    if pos + 20 > entry.len() {
        return None;
    }
    let returned_common = u32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    let returned_file = u32::from_ne_bytes(entry[pos + 12..pos + 16].try_into().ok()?);
    pos += 20;

    let mut name = "";
    let mut dev_id = 0i32;
    let mut obj_type = 0u32;
    let mut file_id = 0u64;
    let mut mtime = 0i64;
    let mut file_size = 0u64;
    let mut nlink = 0u32;
    let mut flags = 0u32;

    // Read a fixed-width field only if its bit is set, advancing `pos` past the bytes it occupies.
    macro_rules! field {
        ($present:expr, $width:expr, $read:expr) => {
            if $present {
                if pos + $width > entry.len() {
                    return None;
                }
                $read;
                pos += $width;
            }
        };
    }

    // ATTR_CMN_NAME: attrreference_t (i32 data offset relative to its own start + u32 length).
    field!(returned_common & libc::ATTR_CMN_NAME != 0, 8, {
        let name_offset = i32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?) as usize;
        let name_start = pos + name_offset;
        if name_start >= entry.len() {
            return None;
        }
        name = CStr::from_bytes_until_nul(&entry[name_start..]).ok()?.to_str().ok()?;
    });
    field!(returned_common & libc::ATTR_CMN_DEVID != 0, 4, {
        dev_id = i32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    });
    field!(returned_common & libc::ATTR_CMN_OBJTYPE != 0, 4, {
        obj_type = u32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    });
    // ATTR_CMN_MODTIME: timespec (16 bytes); we only need tv_sec from the first 8.
    field!(returned_common & libc::ATTR_CMN_MODTIME != 0, 16, {
        mtime = i64::from_ne_bytes(entry[pos..pos + 8].try_into().ok()?);
    });
    // ATTR_CMN_FLAGS (bit 0x40000) precedes FILEID (bit 0x2000000) in ascending-bit order. Returns
    // st_flags as a u32; we only inspect SF_DATALESS to spot unmaterialized cloud placeholders.
    field!(returned_common & libc::ATTR_CMN_FLAGS != 0, 4, {
        flags = u32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    });
    field!(returned_common & libc::ATTR_CMN_FILEID != 0, 8, {
        file_id = u64::from_ne_bytes(entry[pos..pos + 8].try_into().ok()?);
    });
    // File attributes only appear for files, so these bits are naturally clear for directories.
    // LINKCOUNT (bit 0x400) precedes DATALENGTH (bit 0x2000) in ascending-bit order.
    field!(returned_file & libc::ATTR_FILE_LINKCOUNT != 0, 4, {
        nlink = u32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    });
    field!(returned_file & libc::ATTR_FILE_DATALENGTH != 0, 8, {
        file_size = u64::from_ne_bytes(entry[pos..pos + 8].try_into().ok()?);
    });

    Some(ParsedEntry {
        name,
        dev_id,
        obj_type,
        file_id,
        file_size,
        nlink,
        mtime,
        dataless: flags & SF_DATALESS != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one synthetic `getattrlistbulk` entry mirroring `parse_entry`'s expected layout: a
    /// leading u32 length, the 5-word returned-attrs bitmap, then each requested attribute in
    /// ascending-bit order, with the name bytes appended after the fixed fields.
    fn build_file_entry(name: &str, flags: u32, file_size: u64) -> Vec<u8> {
        let mut e = Vec::new();
        e.extend_from_slice(&0u32.to_ne_bytes()); // entry length, backfilled below

        let returned_common: u32 = libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_FLAGS
            | libc::ATTR_CMN_FILEID;
        let returned_file: u32 = libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_DATALENGTH;
        e.extend_from_slice(&returned_common.to_ne_bytes());
        e.extend_from_slice(&0u32.to_ne_bytes()); // vol
        e.extend_from_slice(&0u32.to_ne_bytes()); // dir
        e.extend_from_slice(&returned_file.to_ne_bytes());
        e.extend_from_slice(&0u32.to_ne_bytes()); // fork

        // ATTR_CMN_NAME attrreference_t: i32 offset (relative to its own start) + u32 length.
        let name_ref_pos = e.len();
        e.extend_from_slice(&0i32.to_ne_bytes()); // offset, backfilled
        e.extend_from_slice(&((name.len() + 1) as u32).to_ne_bytes());
        e.extend_from_slice(&7i32.to_ne_bytes()); // DEVID
        e.extend_from_slice(&VREG.to_ne_bytes()); // OBJTYPE
        e.extend_from_slice(&111i64.to_ne_bytes()); // MODTIME tv_sec
        e.extend_from_slice(&0i64.to_ne_bytes()); // MODTIME tv_nsec
        e.extend_from_slice(&flags.to_ne_bytes()); // FLAGS
        e.extend_from_slice(&42u64.to_ne_bytes()); // FILEID
        e.extend_from_slice(&1u32.to_ne_bytes()); // LINKCOUNT
        e.extend_from_slice(&file_size.to_ne_bytes()); // DATALENGTH

        let name_pos = e.len();
        e.extend_from_slice(name.as_bytes());
        e.push(0);

        let offset = (name_pos - name_ref_pos) as i32;
        e[name_ref_pos..name_ref_pos + 4].copy_from_slice(&offset.to_ne_bytes());
        let len = e.len() as u32;
        e[0..4].copy_from_slice(&len.to_ne_bytes());
        e
    }

    #[test]
    fn dataless_cloud_placeholder_is_flagged() {
        // A Dropbox/Drive/iCloud file that isn't materialized: full logical size, 0 bytes on disk.
        let entry = build_file_entry("Demo.gdoc", SF_DATALESS, 173);
        let parsed = parse_entry(&entry).expect("entry should parse");
        assert_eq!(parsed.name, "Demo.gdoc");
        assert_eq!(parsed.file_size, 173, "logical size still parsed");
        assert!(parsed.dataless, "SF_DATALESS must mark the entry dataless");
    }

    #[test]
    fn materialized_file_is_not_dataless() {
        let entry = build_file_entry("main.rs", 0, 12);
        let parsed = parse_entry(&entry).expect("entry should parse");
        assert_eq!(parsed.file_size, 12);
        assert!(!parsed.dataless);
    }
}
