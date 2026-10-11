use std::borrow::Cow;
use std::ffi::CStr;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use rsdirstat_core::scan::{DirJob, DirSink, ScanEvent, UnixDir, node_id, open_dir, run_scan};

const VREG: u32 = 1;
const VDIR: u32 = 2;

// st_flags bit (from <sys/stat.h>, not exposed by libc) marking a "dataless" object: a cloud
// placeholder (Dropbox/Drive/iCloud) whose data isn't materialized locally. Its logical length is
// the full "online" size while 0 bytes are allocated on disk, so we count it as 0. Read-only flag,
// surfaced via ATTR_CMN_FLAGS; reading attributes never triggers a download.
const SF_DATALESS: u32 = 0x4000_0000;

/// `ATTR_DIR_MOUNTSTATUS` bit (from <sys/attr.h>, not exposed by libc) marking a directory that
/// mounts something when looked into: an autofs trigger.
const DIR_MNTSTATUS_TRIGGER: u32 = 0x2;

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
    // MOUNTSTATUS: a mount point's DEVID is the device of the directory it covers, so this is how
    // another filesystem mounted inside the scan shows.
    dirattr: libc::ATTR_DIR_MOUNTSTATUS,
    // DATALENGTH (logical data-fork length), not TOTALSIZE: the latter adds resource-fork bytes,
    // which Linux (stx_size) and Windows (EndOfFile) don't count, so the same tree would total
    // differently per platform. DATALENGTH matches their apparent-size semantics. LINKCOUNT lets us
    // skip hardlinks already counted under another name.
    fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_DATALENGTH,
    forkattr: 0,
};

pub fn scan(root: &Path, cross_filesystems: bool, tx: std::sync::mpsc::Sender<ScanEvent>) -> Result<()> {
    scan_cancellable(root, cross_filesystems, tx, Arc::default())
}

/// Scan, aborting promptly if `cancel` is set (the server sets it when it shuts down mid-scan).
pub fn scan_cancellable(
    root: &Path,
    cross_filesystems: bool,
    tx: std::sync::mpsc::Sender<ScanEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let path = std::fs::canonicalize(root).context("failed to canonicalize root path")?;
    let meta = std::fs::metadata(&path).context("failed to stat root path")?;
    let fd = open_dir(&path).context("failed to open root directory")?;
    let id = node_id(meta.dev(), meta.ino());
    let root_dev = meta.dev() as i32;
    let root = UnixDir::Root(Arc::new(fd));
    run_scan(&path, root, id, tx, cancel, move |dir, buffer, sink| {
        read_dir(dir, buffer, sink, root_dev, cross_filesystems)
    });
    Ok(())
}

/// Read one directory with `getattrlistbulk`, which returns entries and their attributes in
/// batches, so files need no further syscalls. False if it can't be opened.
fn read_dir(
    dir: &DirJob<UnixDir>,
    buffer: &mut [u8],
    sink: &mut DirSink<'_, UnixDir>,
    root_dev: i32,
    cross_filesystems: bool,
) -> bool {
    let Some(fd) = dir.open() else { return false };
    loop {
        let count = unsafe {
            libc::getattrlistbulk(
                fd.as_raw_fd(),
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
                && !matches!(parsed.c_name.to_bytes(), b"." | b"..")
                && (cross_filesystems || parsed.dev_id == root_dev)
            {
                let id = || node_id(parsed.dev_id as u32 as u64, parsed.file_id);
                match parsed.obj_type {
                    VDIR => {
                        let id = if parsed.mount_status == 0 {
                            Some(id())
                        } else {
                            mount_point_id(&fd, &parsed, root_dev, cross_filesystems)
                        };
                        if let Some(id) = id {
                            let child = UnixDir::Child(Arc::clone(&fd), parsed.c_name.to_owned());
                            sink.dir(id, &parsed.name, sink.child_path(&parsed.name), child);
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
    true
}

/// The node id of the mount point `entry` in the directory `dir`, or `None` if the scan leaves it
/// out: when not crossing filesystems, one of another filesystem than the root's (a volume, `/dev`),
/// and an automount trigger, which looking at would mount. The root's own filesystem can be mounted
/// inside it too: `/System/Volumes/Data`, the data volume `/` reports the device of.
fn mount_point_id(dir: &OwnedFd, entry: &ParsedEntry, root_dev: i32, cross_filesystems: bool) -> Option<u64> {
    if !cross_filesystems && entry.mount_status & DIR_MNTSTATUS_TRIGGER != 0 {
        return None;
    }
    // The entry's own attributes are the covered directory's; `fstatat` reports the mount's root.
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    let ok = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            entry.c_name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    (ok == 0 && (cross_filesystems || stat.st_dev == root_dev)).then(|| node_id(stat.st_dev as u32 as u64, stat.st_ino))
}

struct ParsedEntry<'a> {
    /// The name for display: lossy if it isn't UTF-8, which a network filesystem may pass through.
    name: Cow<'a, str>,
    /// The name as the NUL-terminated string in the buffer, for opening the entry.
    c_name: &'a CStr,
    dev_id: i32,
    obj_type: u32,
    file_id: u64,
    file_size: u64,
    nlink: u32,
    mtime: i64,
    dataless: bool,
    /// A directory's `ATTR_DIR_MOUNTSTATUS`: 0 unless something is (or would be) mounted on it.
    mount_status: u32,
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
    let returned_dir = u32::from_ne_bytes(entry[pos + 8..pos + 12].try_into().ok()?);
    let returned_file = u32::from_ne_bytes(entry[pos + 12..pos + 16].try_into().ok()?);
    pos += 20;

    let mut c_name = c"";
    let mut dev_id = 0i32;
    let mut obj_type = 0u32;
    let mut file_id = 0u64;
    let mut mtime = 0i64;
    let mut file_size = 0u64;
    let mut nlink = 0u32;
    let mut flags = 0u32;
    let mut mount_status = 0u32;

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
        c_name = CStr::from_bytes_until_nul(&entry[name_start..]).ok()?;
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
    // Directory attributes come before file attributes, and only for directories.
    field!(returned_dir & libc::ATTR_DIR_MOUNTSTATUS != 0, 4, {
        mount_status = u32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
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
        name: c_name.to_string_lossy(),
        c_name,
        dev_id,
        obj_type,
        file_id,
        file_size,
        nlink,
        mtime,
        dataless: flags & SF_DATALESS != 0,
        mount_status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a synthetic entry holds besides its common attributes.
    enum Kind {
        File { flags: u32, size: u64 },
        Dir { mount_status: u32 },
    }

    /// Build one synthetic `getattrlistbulk` entry mirroring `parse_entry`'s expected layout: a
    /// leading u32 length, the 5-word returned-attrs bitmap, then each requested attribute in
    /// ascending-bit order and group order (common, directory, file), with the name bytes appended
    /// after the fixed fields.
    fn build_entry(name: &str, kind: Kind) -> Vec<u8> {
        let mut e = Vec::new();
        e.extend_from_slice(&0u32.to_ne_bytes()); // entry length, backfilled below

        let returned_common: u32 = libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_FLAGS
            | libc::ATTR_CMN_FILEID;
        let (obj_type, flags, returned_dir, returned_file) = match kind {
            Kind::File { flags, .. } => (VREG, flags, 0, libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_DATALENGTH),
            Kind::Dir { .. } => (VDIR, 0, libc::ATTR_DIR_MOUNTSTATUS, 0),
        };
        e.extend_from_slice(&returned_common.to_ne_bytes());
        e.extend_from_slice(&0u32.to_ne_bytes()); // vol
        e.extend_from_slice(&returned_dir.to_ne_bytes());
        e.extend_from_slice(&returned_file.to_ne_bytes());
        e.extend_from_slice(&0u32.to_ne_bytes()); // fork

        // ATTR_CMN_NAME attrreference_t: i32 offset (relative to its own start) + u32 length.
        let name_ref_pos = e.len();
        e.extend_from_slice(&0i32.to_ne_bytes()); // offset, backfilled
        e.extend_from_slice(&((name.len() + 1) as u32).to_ne_bytes());
        e.extend_from_slice(&7i32.to_ne_bytes()); // DEVID
        e.extend_from_slice(&obj_type.to_ne_bytes()); // OBJTYPE
        e.extend_from_slice(&111i64.to_ne_bytes()); // MODTIME tv_sec
        e.extend_from_slice(&0i64.to_ne_bytes()); // MODTIME tv_nsec
        e.extend_from_slice(&flags.to_ne_bytes()); // FLAGS
        e.extend_from_slice(&42u64.to_ne_bytes()); // FILEID
        match kind {
            Kind::File { size, .. } => {
                e.extend_from_slice(&1u32.to_ne_bytes()); // LINKCOUNT
                e.extend_from_slice(&size.to_ne_bytes()); // DATALENGTH
            }
            Kind::Dir { mount_status } => e.extend_from_slice(&mount_status.to_ne_bytes()),
        }

        let name_pos = e.len();
        e.extend_from_slice(name.as_bytes());
        e.push(0);

        let offset = (name_pos - name_ref_pos) as i32;
        e[name_ref_pos..name_ref_pos + 4].copy_from_slice(&offset.to_ne_bytes());
        let len = e.len() as u32;
        e[0..4].copy_from_slice(&len.to_ne_bytes());
        e
    }

    fn build_file_entry(name: &str, flags: u32, size: u64) -> Vec<u8> {
        build_entry(name, Kind::File { flags, size })
    }

    #[test]
    fn mount_status_is_read_for_directories() {
        for mount_status in [0, libc::DIR_MNTSTATUS_MNTPOINT, DIR_MNTSTATUS_TRIGGER] {
            let entry = build_entry("Volumes", Kind::Dir { mount_status });
            let parsed = parse_entry(&entry).expect("entry should parse");
            assert_eq!((parsed.obj_type, &*parsed.name), (VDIR, "Volumes"));
            assert_eq!(parsed.mount_status, mount_status);
        }
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
