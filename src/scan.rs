use std::collections::HashMap;
use std::ffi::{CStr, CString, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use anyhow::{Context, Result};

const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x80000000;
const ATTR_CMN_NAME: u32 = 0x00000001;
const ATTR_CMN_OBJTYPE: u32 = 0x00000008;
const ATTR_CMN_DEVID: u32 = 0x00000002;
const ATTR_FILE_TOTALSIZE: u32 = 0x00000002;

const VREG: u32 = 1;
const VDIR: u32 = 2;

const FSOPT_NOFOLLOW: u32 = 0x00000001;
const O_RDONLY: c_int = 0x0000;
const O_DIRECTORY: c_int = 0x00100000;

const BUF_SIZE: usize = 256 * 1024;

#[repr(C)]
#[derive(Default)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

static SCAN_ATTRS: AttrList = AttrList {
    bitmapcount: ATTR_BIT_MAP_COUNT,
    reserved: 0,
    commonattr: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE | ATTR_CMN_DEVID,
    volattr: 0,
    dirattr: 0,
    fileattr: ATTR_FILE_TOTALSIZE,
    forkattr: 0,
};

unsafe extern "C" {
    fn getattrlistbulk(
        dirfd: c_int,
        alist: *const AttrList,
        attribute_buffer: *mut c_void,
        buffer_size: usize,
        options: u64,
    ) -> c_int;

    fn open(path: *const i8, oflag: c_int, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
}

pub struct ScanResult {
    pub dir_sizes: HashMap<PathBuf, u64>,
    pub file_entries: Vec<(PathBuf, u64)>,
}

struct WorkQueue {
    queue: Mutex<Vec<PathBuf>>,
    active: AtomicUsize,
    condvar: Condvar,
    done: AtomicBool,
}

impl WorkQueue {
    fn new() -> Self {
        Self {
            queue: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            condvar: Condvar::new(),
            done: AtomicBool::new(false),
        }
    }

    fn push_many(&self, items: Vec<PathBuf>) {
        if items.is_empty() {
            return;
        }
        let count = items.len();
        let mut q = self.queue.lock().unwrap();
        q.extend(items);
        if count == 1 {
            self.condvar.notify_one();
        } else {
            self.condvar.notify_all();
        }
    }

    fn take(&self) -> Option<PathBuf> {
        let mut q = self.queue.lock().unwrap();
        loop {
            if let Some(item) = q.pop() {
                self.active.fetch_add(1, Ordering::AcqRel);
                return Some(item);
            }
            if self.done.load(Ordering::Acquire) {
                return None;
            }
            q = self.condvar.wait(q).unwrap();
        }
    }

    fn finish_one(&self) {
        let prev = self.active.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            let q = self.queue.lock().unwrap();
            if q.is_empty() {
                self.done.store(true, Ordering::Release);
                self.condvar.notify_all();
            }
        }
    }
}

pub fn scan(root: &Path, collect_files: bool, cross_filesystems: bool) -> Result<ScanResult> {
    let root = std::fs::canonicalize(root).context("failed to canonicalize root path")?;
    let root_dev = std::fs::metadata(&root)
        .context("failed to stat root path")?
        .dev() as i32;

    let num_threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let work = Arc::new(WorkQueue::new());
    work.push_many(vec![root.clone()]);

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let work = Arc::clone(&work);
            thread::spawn(move || {
                let mut result = ScanResult {
                    dir_sizes: HashMap::new(),
                    file_entries: Vec::new(),
                };
                let mut buf = vec![0u8; BUF_SIZE];

                while let Some(dir_path) = work.take() {
                    scan_directory(
                        &dir_path,
                        root_dev,
                        collect_files,
                        cross_filesystems,
                        &mut buf,
                        &work,
                        &mut result,
                    );
                    work.finish_one();
                }

                result
            })
        })
        .collect();

    let mut combined = ScanResult {
        dir_sizes: HashMap::new(),
        file_entries: Vec::new(),
    };

    for handle in handles {
        let result = handle
            .join()
            .map_err(|_| anyhow::anyhow!("thread panicked"))?;
        combined.dir_sizes.extend(result.dir_sizes);
        combined.file_entries.extend(result.file_entries);
    }

    // Propagate sizes bottom-up: sort by depth (deepest first) so children add to parents
    let mut all_dirs: Vec<(usize, PathBuf)> = combined
        .dir_sizes
        .keys()
        .map(|p| (p.components().count(), p.clone()))
        .collect();
    all_dirs.sort_unstable_by(|a, b| b.0.cmp(&a.0));

    for (_, dir) in &all_dirs {
        if let Some(parent) = dir.parent() {
            let size = combined.dir_sizes[dir];
            if let Some(parent_size) = combined.dir_sizes.get_mut(parent) {
                *parent_size += size;
            }
        }
    }

    Ok(combined)
}

fn scan_directory(
    dir_path: &Path,
    root_dev: i32,
    collect_files: bool,
    cross_filesystems: bool,
    buf: &mut [u8],
    work: &WorkQueue,
    result: &mut ScanResult,
) {
    let c_path = match path_to_cstring(dir_path) {
        Ok(p) => p,
        Err(_) => return,
    };

    let fd = unsafe { open(c_path.as_ptr(), O_RDONLY | O_DIRECTORY) };
    if fd < 0 {
        return;
    }

    let mut subdirs = Vec::new();
    let mut dir_total: u64 = 0;

    loop {
        let count = unsafe {
            getattrlistbulk(
                fd,
                &SCAN_ATTRS,
                buf.as_mut_ptr() as *mut c_void,
                buf.len(),
                FSOPT_NOFOLLOW as u64,
            )
        };

        if count <= 0 {
            break;
        }

        let mut offset = 0usize;
        for _ in 0..count {
            if offset + 4 > buf.len() {
                break;
            }

            let entry_length =
                u32::from_ne_bytes(buf[offset..offset + 4].try_into().unwrap()) as usize;
            if entry_length == 0 || offset + entry_length > buf.len() {
                break;
            }

            let entry = &buf[offset..offset + entry_length];
            if let Some((name, obj_type, dev_id, file_size)) = parse_entry(entry)
                && name != "."
                && name != ".."
                && (cross_filesystems || dev_id == root_dev)
            {
                match obj_type {
                    VDIR => subdirs.push(dir_path.join(name)),
                    VREG => {
                        dir_total += file_size;
                        if collect_files {
                            result.file_entries.push((dir_path.join(name), file_size));
                        }
                    }
                    _ => {}
                }
            }

            offset += entry_length;
        }
    }

    unsafe { close(fd) };

    // Record this directory's direct file size
    result.dir_sizes.insert(dir_path.to_path_buf(), dir_total);

    // Push subdirectories as new work
    work.push_many(subdirs);
}

fn parse_entry(entry: &[u8]) -> Option<(&str, u32, i32, u64)> {
    let mut pos = 4; // skip entry_length

    // Skip returned attribute_set_t (5 * u32 = 20 bytes)
    if pos + 20 > entry.len() {
        return None;
    }
    pos += 20;

    // ATTR_CMN_NAME: AttrReference (offset: i32, length: u32)
    if pos + 8 > entry.len() {
        return None;
    }
    let name_offset = i32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?) as usize;
    let _name_length = u32::from_ne_bytes(entry[pos + 4..pos + 8].try_into().ok()?);
    let name_base = pos;
    pos += 8;

    // ATTR_CMN_DEVID: dev_t (i32) — bit 1, comes before OBJTYPE bit 3
    if pos + 4 > entry.len() {
        return None;
    }
    let dev_id = i32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    pos += 4;

    // ATTR_CMN_OBJTYPE: fsobj_type_t (u32)
    if pos + 4 > entry.len() {
        return None;
    }
    let obj_type = u32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    pos += 4;

    // ATTR_FILE_TOTALSIZE: off_t (u64) — only present for files
    let file_size = if obj_type == VREG && pos + 8 <= entry.len() {
        u64::from_ne_bytes(entry[pos..pos + 8].try_into().ok()?)
    } else {
        0
    };

    // Resolve name from AttrReference
    let name_start = name_base + name_offset;
    if name_start >= entry.len() {
        return None;
    }
    let name_slice = &entry[name_start..];
    let name = CStr::from_bytes_until_nul(name_slice).ok()?.to_str().ok()?;

    Some((name, obj_type, dev_id, file_size))
}

fn path_to_cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("path contains null byte")
}
