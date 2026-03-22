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
const ATTR_CMN_DEVID: u32 = 0x00000002;
const ATTR_CMN_OBJTYPE: u32 = 0x00000008;
const ATTR_CMN_FILEID: u32 = 0x02000000;
const ATTR_FILE_TOTALSIZE: u32 = 0x00000002;

const VREG: u32 = 1;
const VDIR: u32 = 2;

const FSOPT_NOFOLLOW: u32 = 0x00000001;
const O_RDONLY: c_int = 0x0000;
const O_DIRECTORY: c_int = 0x00100000;

const BUF_SIZE: usize = 1024 * 1024;

#[repr(C)]
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
    commonattr: ATTR_CMN_RETURNED_ATTRS
        | ATTR_CMN_NAME
        | ATTR_CMN_DEVID
        | ATTR_CMN_OBJTYPE
        | ATTR_CMN_FILEID,
    volattr: 0,
    dirattr: 0,
    fileattr: ATTR_FILE_TOTALSIZE,
    forkattr: 0,
};

static NAME_ATTRS: AttrList = AttrList {
    bitmapcount: ATTR_BIT_MAP_COUNT,
    reserved: 0,
    commonattr: ATTR_CMN_NAME,
    volattr: 0,
    dirattr: 0,
    fileattr: 0,
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
    fn openat(dirfd: c_int, path: *const i8, oflag: c_int, ...) -> c_int;
    fn close(fd: c_int) -> c_int;

    fn getattrlist(
        path: *const i8,
        attr_list: *const AttrList,
        attr_buf: *mut c_void,
        attr_buf_size: usize,
        options: u32,
    ) -> c_int;
}

pub struct ScanResult {
    pub dir_sizes: HashMap<PathBuf, u64>,
    pub file_entries: Vec<(PathBuf, u64)>,
}

struct WorkItem {
    fd: c_int,
    file_id: u64,
}

struct WorkQueue {
    queue: Mutex<Vec<WorkItem>>,
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

    fn push(&self, items: Vec<WorkItem>) {
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

    fn take(&self) -> Option<WorkItem> {
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

struct ThreadResult {
    dir_sizes: HashMap<u64, u64>,
    dir_parents: HashMap<u64, u64>,
    file_entries: Vec<(u64, u64, u64)>,
}

pub fn scan(
    root: &Path,
    collect_files: bool,
    cross_filesystems: bool,
    top: usize,
) -> Result<ScanResult> {
    let root = std::fs::canonicalize(root).context("failed to canonicalize root path")?;
    let meta = std::fs::metadata(&root).context("failed to stat root path")?;
    let root_dev = meta.dev() as i32;
    let root_ino = meta.ino();

    let c_root = CString::new(root.as_os_str().as_bytes()).context("path contains null byte")?;
    let root_fd = unsafe { open(c_root.as_ptr(), O_RDONLY | O_DIRECTORY) };
    if root_fd < 0 {
        anyhow::bail!("failed to open root directory");
    }

    let num_threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let work = Arc::new(WorkQueue::new());
    work.push(vec![WorkItem {
        fd: root_fd,
        file_id: root_ino,
    }]);

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let work = Arc::clone(&work);
            thread::spawn(move || {
                let mut result = ThreadResult {
                    dir_sizes: HashMap::new(),
                    dir_parents: HashMap::new(),
                    file_entries: Vec::new(),
                };
                let mut buf = vec![0u8; BUF_SIZE];

                while let Some(item) = work.take() {
                    scan_directory(
                        item.fd,
                        item.file_id,
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

    let mut dir_sizes: HashMap<u64, u64> = HashMap::new();
    let mut dir_parents: HashMap<u64, u64> = HashMap::new();
    let mut file_entries: Vec<(u64, u64, u64)> = Vec::new();

    for handle in handles {
        let result = handle
            .join()
            .map_err(|_| anyhow::anyhow!("thread panicked"))?;
        dir_sizes.extend(result.dir_sizes);
        dir_parents.extend(result.dir_parents);
        file_entries.extend(result.file_entries);
    }

    // Propagate sizes bottom-up via DFS visit order
    let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
    for (&child, &parent) in &dir_parents {
        children.entry(parent).or_default().push(child);
    }

    let mut visit_order = Vec::with_capacity(dir_sizes.len());
    let mut stack = vec![root_ino];
    while let Some(id) = stack.pop() {
        visit_order.push(id);
        if let Some(kids) = children.get(&id) {
            for &kid in kids {
                stack.push(kid);
            }
        }
    }

    for &id in visit_order.iter().rev() {
        if let Some(&parent) = dir_parents.get(&id) {
            let size = dir_sizes.get(&id).copied().unwrap_or(0);
            if let Some(parent_size) = dir_sizes.get_mut(&parent) {
                *parent_size += size;
            }
        }
    }

    // Resolve paths only for top-N results
    let dev_id = meta.dev();
    let mut scan_result = ScanResult {
        dir_sizes: HashMap::new(),
        file_entries: Vec::new(),
    };

    let mut dir_list: Vec<(u64, u64)> = dir_sizes.into_iter().collect();
    let nd = top.min(dir_list.len());
    if nd > 0 {
        dir_list.select_nth_unstable_by(nd - 1, |a, b| b.1.cmp(&a.1));
        dir_list.truncate(nd);
        for (id, size) in &dir_list {
            if *id == root_ino {
                scan_result.dir_sizes.insert(root.clone(), *size);
            } else if let Some(path) =
                resolve_path(*id, dev_id, &dir_parents, root_ino, &root)
            {
                scan_result.dir_sizes.insert(path, *size);
            }
        }
    }

    if collect_files {
        let nf = top.min(file_entries.len());
        if nf > 0 {
            file_entries.select_nth_unstable_by(nf - 1, |a, b| b.2.cmp(&a.2));
            file_entries.truncate(nf);
            for &(file_id, parent_id, _) in &file_entries {
                dir_parents.insert(file_id, parent_id);
            }
            for &(id, _, size) in &file_entries {
                if let Some(path) =
                    resolve_path(id, dev_id, &dir_parents, root_ino, &root)
                {
                    scan_result.file_entries.push((path, size));
                }
            }
        }
    }

    Ok(scan_result)
}

#[allow(clippy::too_many_arguments)]
fn scan_directory(
    fd: c_int,
    dir_file_id: u64,
    root_dev: i32,
    collect_files: bool,
    cross_filesystems: bool,
    buf: &mut [u8],
    work: &WorkQueue,
    result: &mut ThreadResult,
) {
    let mut new_work = Vec::new();
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
            if let Some(parsed) = parse_entry(entry)
                && parsed.name != "."
                && parsed.name != ".."
                && (cross_filesystems || parsed.dev_id == root_dev)
            {
                match parsed.obj_type {
                    VDIR => {
                        // openat avoids full path resolution in the kernel
                        let name_c = parsed.name.as_bytes();
                        let mut name_buf = [0u8; 256];
                        if name_c.len() < 255 {
                            name_buf[..name_c.len()].copy_from_slice(name_c);
                            let child_fd = unsafe {
                                openat(
                                    fd,
                                    name_buf.as_ptr() as *const i8,
                                    O_RDONLY | O_DIRECTORY,
                                )
                            };
                            if child_fd >= 0 {
                                result.dir_parents.insert(parsed.file_id, dir_file_id);
                                new_work.push(WorkItem {
                                    fd: child_fd,
                                    file_id: parsed.file_id,
                                });
                            }
                        }
                    }
                    VREG => {
                        dir_total += parsed.file_size;
                        if collect_files {
                            result.file_entries.push((
                                parsed.file_id,
                                dir_file_id,
                                parsed.file_size,
                            ));
                        }
                    }
                    _ => {}
                }
            }

            offset += entry_length;
        }
    }

    unsafe { close(fd) };

    result.dir_sizes.insert(dir_file_id, dir_total);
    work.push(new_work);
}

struct ParsedEntry<'a> {
    name: &'a str,
    dev_id: i32,
    obj_type: u32,
    file_id: u64,
    file_size: u64,
}

fn parse_entry(entry: &[u8]) -> Option<ParsedEntry<'_>> {
    // entry_length(4) + attribute_set_t(20) + NAME(AttrRef:8) + DEVID(4) + OBJTYPE(4) + FILEID(8) + TOTALSIZE(8)
    let mut pos = 4;

    if pos + 20 > entry.len() {
        return None;
    }
    pos += 20;

    if pos + 8 > entry.len() {
        return None;
    }
    let name_offset = i32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?) as usize;
    let name_base = pos;
    pos += 8;

    if pos + 4 > entry.len() {
        return None;
    }
    let dev_id = i32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    pos += 4;

    if pos + 4 > entry.len() {
        return None;
    }
    let obj_type = u32::from_ne_bytes(entry[pos..pos + 4].try_into().ok()?);
    pos += 4;

    if pos + 8 > entry.len() {
        return None;
    }
    let file_id = u64::from_ne_bytes(entry[pos..pos + 8].try_into().ok()?);
    pos += 8;

    let file_size = if obj_type == VREG && pos + 8 <= entry.len() {
        u64::from_ne_bytes(entry[pos..pos + 8].try_into().ok()?)
    } else {
        0
    };

    let name_start = name_base + name_offset;
    if name_start >= entry.len() {
        return None;
    }
    let name = CStr::from_bytes_until_nul(&entry[name_start..])
        .ok()?
        .to_str()
        .ok()?;

    Some(ParsedEntry {
        name,
        dev_id,
        obj_type,
        file_id,
        file_size,
    })
}

/// Resolve a file ID to a full path by walking up the parent chain.
/// Only called for the top-N results so cost is negligible.
fn resolve_path(
    file_id: u64,
    dev_id: u64,
    parents: &HashMap<u64, u64>,
    root_id: u64,
    root_path: &Path,
) -> Option<PathBuf> {
    if file_id == root_id {
        return Some(root_path.to_path_buf());
    }

    let mut chain = Vec::new();
    let mut current = file_id;
    while current != root_id {
        chain.push(current);
        current = *parents.get(&current)?;
        if chain.len() > 1000 {
            return None;
        }
    }

    let mut path = root_path.to_path_buf();
    for &id in chain.iter().rev() {
        let name = get_name_by_id(dev_id, id)?;
        path.push(name);
    }
    Some(path)
}

fn get_name_by_id(dev_id: u64, file_id: u64) -> Option<String> {
    let vol_path = format!("/.vol/{}/{}", dev_id, file_id);
    let c_path = CString::new(vol_path.as_bytes()).ok()?;

    let mut buf = [0u8; 1024];
    if unsafe {
        getattrlist(
            c_path.as_ptr(),
            &NAME_ATTRS,
            buf.as_mut_ptr() as *mut c_void,
            buf.len(),
            0,
        )
    } != 0
    {
        return None;
    }

    let name_offset = i32::from_ne_bytes(buf[4..8].try_into().ok()?) as usize;
    let name_start = 4 + name_offset;
    if name_start >= buf.len() {
        return None;
    }
    CStr::from_bytes_until_nul(&buf[name_start..])
        .ok()?
        .to_str()
        .ok()
        .map(|s| s.to_string())
}
