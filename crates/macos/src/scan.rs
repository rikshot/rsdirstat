use std::borrow::Cow;
use std::ffi::CStr;
use std::num::NonZeroUsize;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use rsdirstat_core::scan::{
    DirJob, DirSink, ScanEvent, ScanOptions, Started, UnixDir, cpu_count, node_id, open_root, run_scan,
};

/// The most worker threads a scan uses by default. More only queue up in the kernel, which serializes much of
/// `openat` and `getattrlistbulk` on a volume: on a 10-core Mac with warm caches, /System/Library
/// scans in 1.7 s with 3–4 threads and in 5.6 s with 10, which also burn 47 s of kernel CPU.
const MAX_THREADS: usize = 4;

const VREG: u32 = 1;
const VDIR: u32 = 2;

// st_flags bit (from <sys/stat.h>, not exposed by libc) marking a "dataless" object: a cloud
// placeholder (Dropbox/Drive/iCloud) whose data isn't materialized locally. Its logical length is
// the full "online" size while 0 bytes are allocated on disk, so we count it as 0. Read-only flag,
// surfaced via ATTR_CMN_FLAGS; reading attributes never triggers a download.
const SF_DATALESS: u32 = 0x4000_0000;

/// st_flags bit (from <sys/stat.h>, not exposed by libc) marking a firmlink: a directory of the
/// read-only system volume that leads to one of the data volume, `/Users` to
/// `/System/Volumes/Data/Users`. Only its attributes carry it; `stat` goes through to the target.
const SF_FIRMLINK: u32 = 0x0080_0000;

/// `ATTR_DIR_MOUNTSTATUS` bit (from <sys/attr.h>, not exposed by libc) marking a directory that
/// mounts something when looked into: an autofs trigger.
const DIR_MNTSTATUS_TRIGGER: u32 = 0x2;

// <sys/resource.h>; libc binds neither these nor setiopolicy_np (libSystem, macOS 10.15+ for this
// policy).
const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: libc::c_int = 3;
const IOPOL_SCOPE_PROCESS: libc::c_int = 0;
const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: libc::c_int = 1;
unsafe extern "C" {
    fn setiopolicy_np(iotype: libc::c_int, scope: libc::c_int, policy: libc::c_int) -> libc::c_int;
}

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

/// Scan `root`, stopping promptly once `cancel` is set (the server sets it when it shuts down
/// mid-scan).
pub fn scan(
    root: &Path,
    options: ScanOptions,
    tx: std::sync::mpsc::Sender<ScanEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    // Listing a dataless folder (a cloud folder never fetched, or evicted) would have its File
    // Provider fetch it from the network, then each folder in it in turn: slow, and a change to what
    // the user keeps synced. With that off, the listing fails with EDEADLK instead, and the folder
    // reads as what it holds on this disk: nothing.
    if unsafe {
        setiopolicy_np(
            IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
            IOPOL_SCOPE_PROCESS,
            IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
        )
    } != 0
    {
        eprintln!("setiopolicy_np failed: {}", std::io::Error::last_os_error());
    }
    let path = std::fs::canonicalize(root).with_context(|| format!("can't scan {}", root.display()))?;
    let meta = std::fs::metadata(&path).with_context(|| format!("can't stat {}", path.display()))?;
    let fd = open_root(&path).with_context(|| format!("can't open {}", path.display()))?;
    let root_dev = meta.dev() as i32;
    let id = mac_node_id(root_dev, meta.ino());
    let root = UnixDir::Root(Arc::new(fd));
    let threads = options
        .threads
        .map_or_else(|| cpu_count().min(MAX_THREADS), NonZeroUsize::get);
    run_scan(
        Started::new(path, tx),
        root,
        id,
        cancel,
        threads,
        move |dir, buffer, sink| read_dir(dir, buffer, sink, root_dev, options.cross_filesystems),
    );
    Ok(())
}

/// Read one directory with `getattrlistbulk`, which returns entries and their attributes in
/// batches, so files need no further syscalls. False if it can't be opened, or searched.
fn read_dir(
    dir: &DirJob<UnixDir>,
    buffer: &mut [u8],
    sink: &mut DirSink<'_, UnixDir>,
    root_dev: i32,
    cross_filesystems: bool,
) -> bool {
    let Some(fd) = dir.open(sink) else { return false };
    let mut first = true;
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
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                // A cloud folder not fetched (see `scan`): it holds nothing on this disk.
                Some(libc::EDEADLK) => {}
                // A folder that opens but can't be searched (`r--`), as these attributes need: left
                // out, like one that can't be opened. Not the root, which the scan shows.
                Some(libc::EACCES | libc::EPERM) if first && !dir.is_root() => return false,
                _ => sink.read_failed(format_args!("getattrlistbulk: {error}")),
            }
            break;
        }
        if count == 0 {
            break;
        }
        first = false;

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
                let id = || mac_node_id(parsed.dev_id, parsed.file_id);
                match parsed.obj_type {
                    VDIR => {
                        if let Some(id) = dir_id(&fd, &parsed, root_dev, cross_filesystems) {
                            let child = UnixDir::Child(Arc::clone(&fd), parsed.c_name.to_owned());
                            sink.dir(id, &parsed.name, child);
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

/// The node id of the subdirectory `entry` in the directory `dir`, or `None` if the scan leaves it
/// out.
fn dir_id(dir: &OwnedFd, entry: &ParsedEntry, root_dev: i32, cross_filesystems: bool) -> Option<u64> {
    if entry.mount_status != 0 {
        mount_point_id(dir, entry, root_dev, cross_filesystems)
    } else {
        // A firmlink's FILEID is its own, not that of the directory it leads to, which the scan can
        // also reach under `/System/Volumes/Data`: `fstatat` reports that one, so both names get
        // one id and the directory is read once.
        let target = entry.firmlink.then(|| stat_at(dir, entry)).flatten();
        let (dev, ino) = target.map_or((entry.dev_id, entry.file_id), |stat| (stat.st_dev, stat.st_ino));
        Some(mac_node_id(dev, ino))
    }
}

/// The node id of the file `ino` on device `dev`, an i32 `dev_t` taken as unsigned, as listings give
/// it: `MetadataExt::dev` sign-extends it, which would give the root another id than its listing's.
fn mac_node_id(dev: i32, ino: u64) -> u64 {
    node_id(dev as u32 as u64, ino)
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
    let stat = stat_at(dir, entry)?;
    (cross_filesystems || stat.st_dev == root_dev).then(|| mac_node_id(stat.st_dev, stat.st_ino))
}

/// `fstatat` of `entry` in `dir`, which unlike its attributes goes through to what is mounted on it,
/// or what it leads to if it's a firmlink.
fn stat_at(dir: &OwnedFd, entry: &ParsedEntry) -> Option<libc::stat> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    let ok = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            entry.c_name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    (ok == 0).then_some(stat)
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
    /// A firmlink (`SF_FIRMLINK`), whose FILEID isn't that of the directory it leads to.
    firmlink: bool,
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
    // st_flags as a u32; we only inspect SF_DATALESS to spot unmaterialized cloud placeholders, and
    // SF_FIRMLINK.
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
    // LINKCOUNT (bit 0x1) precedes DATALENGTH (bit 0x200) in ascending-bit order.
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
        firmlink: flags & SF_FIRMLINK != 0,
        mount_status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a synthetic entry holds besides its common attributes.
    enum Kind {
        File { flags: u32, size: u64 },
        Dir { flags: u32, mount_status: u32 },
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
            Kind::Dir { flags, .. } => (VDIR, flags, libc::ATTR_DIR_MOUNTSTATUS, 0),
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
            Kind::Dir { mount_status, .. } => e.extend_from_slice(&mount_status.to_ne_bytes()),
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
            let entry = build_entry("Volumes", Kind::Dir { flags: 0, mount_status });
            let parsed = parse_entry(&entry).expect("entry should parse");
            assert_eq!((parsed.obj_type, &*parsed.name), (VDIR, "Volumes"));
            assert_eq!(parsed.mount_status, mount_status);
        }
    }

    #[test]
    fn firmlink_is_flagged() {
        // What `/` lists for `Users`: SF_NOUNLINK | SF_FIRMLINK.
        let entry = build_entry(
            "Users",
            Kind::Dir {
                flags: 0x0090_0000,
                mount_status: 0,
            },
        );
        assert!(parse_entry(&entry).expect("entry should parse").firmlink);
        let entry = build_entry(
            "Users",
            Kind::Dir {
                flags: 0x0010_0000,
                mount_status: 0,
            },
        );
        assert!(!parse_entry(&entry).expect("entry should parse").firmlink);
    }

    /// The firmlink flag and the id `read_dir` gives each subdirectory of `path`, by name.
    fn subdir_ids(path: &str) -> std::collections::HashMap<String, (bool, Option<u64>)> {
        let fd = open_root(Path::new(path)).expect("open");
        let dev = std::fs::metadata(path).expect("stat").dev() as i32;
        let mut buffer = vec![0u8; 64 * 1024];
        let mut ids = std::collections::HashMap::new();
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
            assert!(
                count >= 0,
                "getattrlistbulk {path}: {}",
                std::io::Error::last_os_error()
            );
            if count == 0 {
                return ids;
            }
            let mut offset = 0;
            for _ in 0..count {
                let length = u32::from_ne_bytes(buffer[offset..offset + 4].try_into().unwrap()) as usize;
                let parsed = parse_entry(&buffer[offset..offset + length]).expect("entry should parse");
                if parsed.obj_type == VDIR {
                    let id = dir_id(&fd, &parsed, dev, false);
                    ids.insert(parsed.name.to_string(), (parsed.firmlink, id));
                }
                offset += length;
            }
        }
    }

    #[test]
    fn a_firmlink_and_the_directory_it_leads_to_get_one_id() {
        // Every macOS since 10.15 firmlinks /System/Library/Caches to the data volume's, which a
        // scan of / or /System also reaches under /System/Volumes/Data.
        let (firmlink, id) = subdir_ids("/System/Library")["Caches"];
        assert!(firmlink, "/System/Library/Caches should be a firmlink");
        let (_, data_id) = subdir_ids("/System/Volumes/Data/System/Library")["Caches"];
        assert!(id.is_some());
        assert_eq!(id, data_id);
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
