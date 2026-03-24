use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use anyhow::{Context, Result};
use dashmap::DashSet;

const VREG: u32 = 1;
const VDIR: u32 = 2;

const BUF_SIZE: usize = 1024 * 1024;

static SCAN_ATTRS: libc::attrlist = libc::attrlist {
    bitmapcount: libc::ATTR_BIT_MAP_COUNT,
    reserved: 0,
    commonattr: libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_MODTIME
        | libc::ATTR_CMN_FILEID,
    volattr: 0,
    dirattr: 0,
    fileattr: libc::ATTR_FILE_TOTALSIZE,
    forkattr: 0,
};

static NAME_ATTRS: libc::attrlist = libc::attrlist {
    bitmapcount: libc::ATTR_BIT_MAP_COUNT,
    reserved: 0,
    commonattr: libc::ATTR_CMN_NAME,
    volattr: 0,
    dirattr: 0,
    fileattr: 0,
    forkattr: 0,
};

pub struct ScanResult {
    pub dir_sizes: HashMap<PathBuf, u64>,
    pub file_entries: Vec<(PathBuf, u64)>,
}

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

struct WorkItem {
    fd: OwnedFd,
    file_id: u64,
    parent_id: u64,
    name: String,
    path: String,
}

struct WorkQueueInner {
    queue: Vec<WorkItem>,
    pending: usize,
}

struct WorkQueue {
    inner: Mutex<WorkQueueInner>,
    condvar: Condvar,
}

impl WorkQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(WorkQueueInner {
                queue: Vec::new(),
                pending: 0,
            }),
            condvar: Condvar::new(),
        }
    }

    fn push(&self, items: Vec<WorkItem>) {
        if items.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let count = items.len();
        inner.pending += count;
        inner.queue.extend(items);
        if count == 1 {
            self.condvar.notify_one();
        } else {
            self.condvar.notify_all();
        }
    }

    fn take(&self) -> Option<WorkItem> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if let Some(item) = inner.queue.pop() {
                return Some(item);
            }
            if inner.pending == 0 {
                self.condvar.notify_all();
                return None;
            }
            inner = self.condvar.wait(inner).unwrap();
        }
    }

    fn finish_one(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.pending -= 1;
        if inner.pending == 0 {
            self.condvar.notify_all();
        }
    }

    fn pending(&self) -> usize {
        self.inner.lock().unwrap().pending
    }

    fn cancel(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.pending = 0;
        inner.queue.clear();
        self.condvar.notify_all();
    }
}

#[derive(Default)]
struct ThreadResult {
    dir_sizes: HashMap<u64, u64>,
    dir_parents: HashMap<u64, u64>,
    file_entries: Vec<(u64, u64, u64)>,
}

fn raise_fd_limit() {
    unsafe {
        let mut rlim: libc::rlimit = std::mem::zeroed();
        libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim);
        rlim.rlim_cur = rlim.rlim_max;
        libc::setrlimit(libc::RLIMIT_NOFILE, &rlim);
    }
}

struct RootInfo {
    path: PathBuf,
    ino: u64,
    dev: u64,
    name: String,
    fd: OwnedFd,
}

fn open_root(root: &Path) -> Result<RootInfo> {
    raise_fd_limit();
    let path = std::fs::canonicalize(root).context("failed to canonicalize root path")?;
    let meta = std::fs::metadata(&path).context("failed to stat root path")?;
    let dev = meta.dev();
    let ino = meta.ino();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let c_path = CString::new(path.as_os_str().as_bytes()).context("path contains null byte")?;
    let raw_fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if raw_fd < 0 {
        anyhow::bail!("failed to open root directory");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    Ok(RootInfo {
        path,
        ino,
        dev,
        name,
        fd,
    })
}

struct RawScan {
    root_path: PathBuf,
    root_ino: u64,
    root_dev: u64,
    dir_sizes: HashMap<u64, u64>,
    dir_parents: HashMap<u64, u64>,
    file_entries: Vec<(u64, u64, u64)>,
}

fn scan_raw(root: &Path, collect_files: bool, cross_filesystems: bool) -> Result<RawScan> {
    let ri = open_root(root)?;

    let num_threads = thread::available_parallelism().map(|n| n.get()).unwrap_or(4);

    let visited = {
        let set = DashSet::new();
        set.insert(ri.ino);
        Arc::new(set)
    };

    let work = Arc::new(WorkQueue::new());
    work.push(vec![WorkItem {
        fd: ri.fd,
        file_id: ri.ino,
        parent_id: 0,
        name: ri.name,
        path: String::new(),
    }]);

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let work = Arc::clone(&work);
            let visited = Arc::clone(&visited);
            let root_dev_i32 = ri.dev as i32;
            thread::spawn(move || {
                let mut result = ThreadResult::default();
                let mut buf = vec![0u8; BUF_SIZE];

                while let Some(item) = work.take() {
                    scan_directory(
                        item.fd,
                        item.file_id,
                        item.parent_id,
                        &item.name,
                        root_dev_i32,
                        collect_files,
                        cross_filesystems,
                        &mut buf,
                        &work,
                        &mut result,
                        None,
                        &visited,
                        &item.path,
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
        let result = handle.join().map_err(|_| anyhow::anyhow!("thread panicked"))?;
        dir_sizes.extend(result.dir_sizes);
        dir_parents.extend(result.dir_parents);
        file_entries.extend(result.file_entries);
    }

    Ok(RawScan {
        root_path: ri.path,
        root_ino: ri.ino,
        root_dev: ri.dev,
        dir_sizes,
        dir_parents,
        file_entries,
    })
}

pub fn scan(root: &Path, collect_files: bool, cross_filesystems: bool, top: usize) -> Result<ScanResult> {
    let raw = scan_raw(root, collect_files, cross_filesystems)?;
    let mut dir_sizes = raw.dir_sizes;
    let mut dir_parents = raw.dir_parents;
    let file_entries = raw.file_entries;

    let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
    for (&child, &parent) in &dir_parents {
        children.entry(parent).or_default().push(child);
    }

    let mut visit_order = Vec::with_capacity(dir_sizes.len());
    let mut stack = vec![raw.root_ino];
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
            if *id == raw.root_ino {
                scan_result.dir_sizes.insert(raw.root_path.clone(), *size);
            } else if let Some(path) = resolve_path(*id, raw.root_dev, &dir_parents, raw.root_ino, &raw.root_path) {
                scan_result.dir_sizes.insert(path, *size);
            }
        }
    }

    if collect_files {
        let nf = top.min(file_entries.len());
        if nf > 0 {
            let mut file_entries = file_entries;
            file_entries.select_nth_unstable_by(nf - 1, |a, b| b.2.cmp(&a.2));
            file_entries.truncate(nf);
            for &(file_id, parent_id, _) in &file_entries {
                dir_parents.insert(file_id, parent_id);
            }
            for &(id, _, size) in &file_entries {
                if let Some(path) = resolve_path(id, raw.root_dev, &dir_parents, raw.root_ino, &raw.root_path) {
                    scan_result.file_entries.push((path, size));
                }
            }
        }
    }

    Ok(scan_result)
}

pub fn scan_tree_streaming(root: &Path, cross_filesystems: bool, tx: std::sync::mpsc::Sender<ScanEvent>) -> Result<()> {
    let ri = open_root(root)?;

    let _ = tx.send(ScanEvent::ScanStart { path: ri.name.clone() });

    let num_threads = thread::available_parallelism().map(|n| n.get()).unwrap_or(4);

    let visited = {
        let set = DashSet::new();
        set.insert(ri.ino);
        Arc::new(set)
    };

    let work = Arc::new(WorkQueue::new());
    work.push(vec![WorkItem {
        fd: ri.fd,
        file_id: ri.ino,
        parent_id: 0,
        name: ri.name.clone(),
        path: ri.path.display().to_string(),
    }]);

    let active_dirs: Arc<Vec<Mutex<String>>> = Arc::new((0..num_threads).map(|_| Mutex::new(String::new())).collect());

    let _handles: Vec<_> = (0..num_threads)
        .enumerate()
        .map(|(tid, _)| {
            let work = Arc::clone(&work);
            let tx = tx.clone();
            let visited = Arc::clone(&visited);
            let root_dev_i32 = ri.dev as i32;
            let active = Arc::clone(&active_dirs);
            thread::spawn(move || {
                let mut buf = vec![0u8; BUF_SIZE];
                let mut result = ThreadResult::default();

                while let Some(item) = work.take() {
                    *active[tid].lock().unwrap() = item.path.clone();
                    scan_directory(
                        item.fd,
                        item.file_id,
                        item.parent_id,
                        &item.name,
                        root_dev_i32,
                        false,
                        cross_filesystems,
                        &mut buf,
                        &work,
                        &mut result,
                        Some(&tx),
                        &visited,
                        &item.path,
                    );
                    work.finish_one();
                }
            })
        })
        .collect();

    // Wait for completion with stall detection — a hung mount can block
    // a worker thread in getattrlistbulk indefinitely
    let mut stall_count = 0u32;
    let mut last_pending = usize::MAX;
    loop {
        thread::sleep(std::time::Duration::from_millis(200));
        let p = work.pending();
        if p == 0 {
            break;
        }
        if p == last_pending {
            stall_count += 1;
            if stall_count >= 15 {
                eprintln!("Scan stalled with {p} items pending, finishing with partial results");
                eprintln!("Stuck directories:");
                for dir in active_dirs.iter() {
                    let name = dir.lock().unwrap();
                    if !name.is_empty() {
                        eprintln!("  {name}");
                    }
                }
                work.cancel();
                break;
            }
        } else {
            stall_count = 0;
        }
        last_pending = p;
    }

    let _ = tx.send(ScanEvent::ScanDone);

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn scan_directory(
    fd: OwnedFd,
    dir_file_id: u64,
    parent_id: u64,
    dir_name: &str,
    root_dev: i32,
    collect_files: bool,
    cross_filesystems: bool,
    buf: &mut [u8],
    work: &WorkQueue,
    result: &mut ThreadResult,
    tx: Option<&std::sync::mpsc::Sender<ScanEvent>>,
    visited: &DashSet<u64>,
    dir_path: &str,
) {
    let mut new_work = Vec::new();
    let mut dir_total: u64 = 0;
    let mut dir_mtime: i64 = 0;

    loop {
        let count = unsafe {
            libc::getattrlistbulk(
                fd.as_raw_fd(),
                &SCAN_ATTRS as *const libc::attrlist as *mut libc::c_void,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                libc::FSOPT_NOFOLLOW as u64,
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

            let entry_length = u32::from_ne_bytes(buf[offset..offset + 4].try_into().unwrap()) as usize;
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
                        let name_c = parsed.name.as_bytes();
                        let mut name_buf = [0u8; 256];
                        if name_c.len() < 255 {
                            name_buf[..name_c.len()].copy_from_slice(name_c);
                            let raw_fd = unsafe {
                                libc::openat(
                                    fd.as_raw_fd(),
                                    name_buf.as_ptr() as *const i8,
                                    libc::O_RDONLY | libc::O_DIRECTORY,
                                )
                            };
                            if raw_fd >= 0 {
                                let child_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
                                if visited.insert(parsed.file_id) {
                                    result.dir_parents.insert(parsed.file_id, dir_file_id);
                                    new_work.push(WorkItem {
                                        fd: child_fd,
                                        file_id: parsed.file_id,
                                        parent_id: dir_file_id,
                                        name: parsed.name.to_string(),
                                        path: if dir_path.is_empty() {
                                            String::new()
                                        } else {
                                            format!("{}/{}", dir_path.trim_end_matches('/'), parsed.name)
                                        },
                                    });
                                }
                            }
                        } else {
                            eprintln!("Skipping directory with name >= 255 bytes");
                        }
                    }
                    VREG => {
                        dir_total += parsed.file_size;
                        dir_mtime = dir_mtime.max(parsed.mtime);
                        if collect_files {
                            result
                                .file_entries
                                .push((parsed.file_id, dir_file_id, parsed.file_size));
                        }
                        if let Some(tx) = tx
                            && parsed.file_size > 0
                        {
                            let _ = tx.send(ScanEvent::File {
                                parent: dir_file_id,
                                name: parsed.name.to_string(),
                                size: parsed.file_size,
                                mtime: parsed.mtime,
                            });
                        }
                    }
                    _ => {}
                }
            }

            offset += entry_length;
        }
    }

    drop(fd);

    result.dir_sizes.insert(dir_file_id, dir_total);

    if let Some(tx) = tx {
        let _ = tx.send(ScanEvent::Dir {
            id: dir_file_id,
            parent: parent_id,
            name: dir_name.to_string(),
            size: dir_total,
            mtime: dir_mtime,
        });
    }

    work.push(new_work);
}

struct ParsedEntry<'a> {
    name: &'a str,
    dev_id: i32,
    obj_type: u32,
    file_id: u64,
    file_size: u64,
    mtime: i64,
}

fn parse_entry(entry: &[u8]) -> Option<ParsedEntry<'_>> {
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

    if pos + 16 > entry.len() {
        return None;
    }
    let mtime = i64::from_ne_bytes(entry[pos..pos + 8].try_into().ok()?);
    pos += 16; // tv_sec(8) + tv_nsec(8)

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
    let name = CStr::from_bytes_until_nul(&entry[name_start..]).ok()?.to_str().ok()?;

    Some(ParsedEntry {
        name,
        dev_id,
        obj_type,
        file_id,
        file_size,
        mtime,
    })
}

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
        libc::getattrlist(
            c_path.as_ptr(),
            &NAME_ATTRS as *const libc::attrlist as *mut libc::c_void,
            buf.as_mut_ptr() as *mut libc::c_void,
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
