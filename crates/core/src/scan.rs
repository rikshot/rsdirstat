//! The platform-independent half of a scan: the worker threads, the work queue, and each
//! directory's bookkeeping. A platform crate supplies only how to open the root and how to read
//! one directory ([`run_scan`]'s `read_dir`), reporting each entry to a [`DirSink`].

use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use dashmap::DashSet;

/// What a scan reports, in order: `ScanStart`, then the entries, then `ScanDone`. `ScanStart` may
/// come again before the first entry, when a scanner falls back to another way of scanning; each
/// starts the scan over. In-process only; nothing here goes over the wire.
#[derive(Clone, Debug)]
pub enum ScanEvent {
    /// The first event, with the scan root as people see it: canonical, and on Windows without the
    /// `\\?\` prefix (see `plain_path`). The tree joins its directories' names onto it.
    ScanStart { root: PathBuf },
    /// A directory with its files, sent once it has been read: after its parent, before its
    /// subdirectories. Its own size is the total of `files`. `mtime` is the newest of all its
    /// files', including the empty ones, which `files` leaves out.
    Dir {
        id: u64,
        parent: u64,
        name: String,
        mtime: i64,
        files: Vec<ScannedFile>,
    },
    /// The last event: nothing follows it, even from a worker still stuck inside a directory after
    /// a cancelled or stalled scan.
    ScanDone { outcome: ScanOutcome },
}

/// A file of non-zero size in a `ScanEvent::Dir`.
#[derive(Clone, Debug)]
pub struct ScannedFile {
    pub name: String,
    pub size: u64,
    pub mtime: i64,
}

impl ScannedFile {
    /// The total size of `files`: their directory's own size.
    pub fn total(files: &[ScannedFile]) -> u64 {
        files.iter().fold(0u64, |total, file| total.saturating_add(file.size))
    }
}

/// Combine a `(device, inode)` pair into a stable node id. Inodes are only unique within a single
/// filesystem, so a cross-filesystem scan (`--all`) can see the same inode number on two devices;
/// mixing the device in keeps node identity distinct across mounts (and stops one directory from
/// being wrongly skipped or overwriting another). Never 0, the parent-of-root sentinel.
pub fn node_id(dev: u64, ino: u64) -> u64 {
    // splitmix64 finalizer over the inode combined with a golden-ratio-scaled device.
    let mut x = ino.wrapping_add(dev.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    x.max(1)
}

#[cfg(unix)]
fn raise_fd_limit() {
    // The scanner keeps a descriptor open for every directory with subdirectories still queued (see
    // `UnixDir`), which can outgrow the default soft limit (256 on macOS) on a deep or wide tree.
    // Raise it to the hard limit.
    unsafe {
        let mut rlim = libc::rlimit::default();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            eprintln!("getrlimit(RLIMIT_NOFILE) failed: {}", std::io::Error::last_os_error());
            return;
        }
        if rlim.rlim_cur >= rlim.rlim_max && rlim.rlim_max != libc::RLIM_INFINITY {
            return;
        }
        rlim.rlim_cur = rlim.rlim_max;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0 {
            return;
        }
        // macOS rejects rlim_cur == RLIM_INFINITY and caps it at the per-process ceiling, so the
        // raise above silently fails there and leaves the 256 default in place. Retry with the
        // kernel's actual limit (kern.maxfilesperproc) instead of giving up.
        #[cfg(target_os = "macos")]
        {
            let mut open_max: libc::c_int = 0;
            let mut size = std::mem::size_of::<libc::c_int>();
            if libc::sysctlbyname(
                c"kern.maxfilesperproc".as_ptr(),
                &mut open_max as *mut _ as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            ) == 0
                && open_max > 0
            {
                rlim.rlim_cur = open_max as libc::rlim_t;
                if libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0 {
                    return;
                }
            }
        }
        eprintln!("setrlimit(RLIMIT_NOFILE) failed: {}", std::io::Error::last_os_error());
    }
}

/// Open a directory for reading its entries.
#[cfg(unix)]
pub fn open_dir(path: &Path) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path)?;
    Ok(dir.into())
}

/// A directory in a Unix scan's queue. A subdirectory is opened only once a worker takes it, by
/// name in its parent, which stays open until then. Opening each one as soon as it's found would
/// hold a descriptor for every queued directory, which a wide tree can push past the process's
/// limit; every directory that then failed to open would be left out without a word.
#[cfg(unix)]
pub enum UnixDir {
    /// The scan root, opened up front so that a root that can't be opened fails the scan.
    Root(Arc<std::os::fd::OwnedFd>),
    /// A subdirectory: its open parent, and its name there.
    Child(Arc<std::os::fd::OwnedFd>, std::ffi::CString),
}

#[cfg(unix)]
impl DirJob<UnixDir> {
    /// Open the directory, or `None` if it can't be opened. Failures other than a lack of
    /// permission, or the directory having gone, are reported.
    pub fn open(&self) -> Option<Arc<std::os::fd::OwnedFd>> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let (parent, name) = match &self.handle {
            UnixDir::Root(fd) => return Some(Arc::clone(fd)),
            UnixDir::Child(parent, name) => (parent, name),
        };
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd >= 0 {
            return Some(Arc::new(unsafe { OwnedFd::from_raw_fd(fd) }));
        }
        let error = std::io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EACCES | libc::EPERM | libc::ENOENT | libc::ENOTDIR)
        ) {
            eprintln!("Can't open {}: {error}", self.path);
        }
        None
    }
}

/// `path` as people see it, and as Explorer takes it: without the `\\?\` prefix of the verbatim
/// paths `canonicalize` gives on Windows, which scans keep for opening paths over `MAX_PATH`.
/// `\\?\C:\x` is `C:\x`, and `\\?\UNC\server\share\x` is `\\server\share\x`; a path with no plainer
/// form (`\\?\Volume{GUID}\`) is left as it is.
pub fn plain_path(path: &Path) -> Cow<'_, Path> {
    let Some(rest) = path.to_str().and_then(|path| path.strip_prefix(r"\\?\")) else {
        return path.into();
    };
    if let Some(unc) = rest.strip_prefix(r"UNC\") {
        return PathBuf::from(format!(r"\\{unc}")).into();
    }
    match rest.as_bytes() {
        [drive, b':', ..] if drive.is_ascii_alphabetic() => PathBuf::from(rest).into(),
        _ => path.into(),
    }
}

/// The scan root's name as shown in the breadcrumb: its last component, or the whole path for a
/// filesystem root (`/`, `C:\`).
pub fn root_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || plain_path(path).display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// How many CPUs this machine has, the most worker threads a scan can keep busy.
pub fn cpu_count() -> usize {
    thread::available_parallelism().map_or(4, |n| n.get())
}

/// What the user chose about a scan.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScanOptions {
    /// Descend into other filesystems mounted inside the scanned tree.
    pub cross_filesystems: bool,
    /// How many worker threads to scan with, instead of the platform's default.
    pub threads: Option<NonZeroUsize>,
    /// On Windows, walk the folders even where NTFS's file index (the MFT) could be read instead,
    /// which needs administrator rights.
    pub no_mft: bool,
}

/// Size of each worker's buffer for directory reads. Measured from 16 KiB to 1 MiB, it makes no
/// difference on Linux (`getdents64`) or macOS (`getattrlistbulk`). On Windows the file system
/// filters pay for a large buffer on every listing call: C:\Windows scans about 10% faster with
/// 64 KiB than with 1 MiB on one thread, 6% on 16.
const READ_BUFFER_SIZE: usize = 64 * 1024;

/// How long the scan may go without any progress before it is treated as stuck. Every read from the
/// OS and every few entries count as progress, so only a syscall blocked this long on every busy
/// worker (a dead network mount, typically) trips it, not a merely slow or huge directory.
const STALL_TIMEOUT: Duration = Duration::from_secs(3);

/// How many entries a worker reports between recordings of progress. A huge directory on slow
/// storage takes a syscall per entry (a `statx`, or opening a subdirectory), so a read from the OS
/// can return far more entries than the stall timeout has time for; counting entries keeps it
/// showing progress. Batched so the workers don't contend on the shared counter for every one.
const ENTRIES_PER_PROGRESS: u32 = 64;

/// How often the waiting caller samples progress and the cancel flag.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// One directory waiting to be read.
pub struct DirJob<H> {
    /// The directory, open or ready to open (see `UnixDir`); dropped once it has been read.
    pub handle: H,
    pub name: String,
    id: u64,
    parent_id: u64,
    /// Full path, for opening children by path (Windows) and for reporting a stuck directory.
    path: String,
}

/// How a scan ended, as `ScanDone` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanOutcome {
    /// Every reachable directory was read.
    Complete,
    /// The caller's cancel flag was set; the results are partial.
    Cancelled,
    /// No progress for [`STALL_TIMEOUT`]; the stuck directories were reported and the results are
    /// partial.
    Stalled,
}

/// Walk the tree under `root`, already opened as `handle` and identified as `id`, on `threads`
/// worker threads, sending `ScanStart`, then a `Dir` per directory carrying its files, then
/// `ScanDone` with how the scan ended.
///
/// `read_dir` reads one directory: it reports every entry to the [`DirSink`] and calls
/// [`DirSink::checkpoint`] after each read from the OS, stopping when it returns false. It returns
/// false if the directory can't be opened, which leaves it out, like any unreadable directory.
///
/// Returns once the walk finishes, the `cancel` flag is set (within [`POLL_INTERVAL`]), or the scan
/// stalls. `ScanDone` is sent last, and `tx` is dropped on return, so the receiver's channel closes
/// then. Workers are detached rather than joined so that one blocked in a syscall can't hold the
/// scan open; whatever it finds afterwards is dropped.
pub fn run_scan<H, F>(
    root: &Path,
    handle: H,
    id: u64,
    tx: Sender<ScanEvent>,
    cancel: Arc<AtomicBool>,
    threads: usize,
    read_dir: F,
) where
    H: Send + 'static,
    F: Fn(&DirJob<H>, &mut [u8], &mut DirSink<'_, H>) -> bool + Send + Sync + 'static,
{
    #[cfg(unix)]
    raise_fd_limit();

    let _ = tx.send(ScanEvent::ScanStart {
        root: plain_path(root).into_owned(),
    });
    let root = DirJob {
        handle,
        name: root_name(root),
        id,
        parent_id: 0,
        // On Windows children are opened by path, built from this one. It keeps its
        // extended-length (\\?\) prefix: without it CreateFileW fails for paths over MAX_PATH
        // (260), silently dropping deep subtrees.
        path: root.display().to_string(),
    };
    let shared = Arc::new(Shared {
        queue: WorkQueue::new(cancel),
        tx: RwLock::new(Some(tx)),
        visited_dirs: DashSet::from_iter([root.id]),
        visited_files: DashSet::new(),
    });
    shared.queue.push(vec![root]);

    let threads = threads.max(1);
    let active: Arc<[Mutex<String>]> = (0..threads).map(|_| Mutex::default()).collect();
    let read_dir = Arc::new(read_dir);
    for worker_index in 0..threads {
        let shared = Arc::clone(&shared);
        let read_dir = Arc::clone(&read_dir);
        let active = Arc::clone(&active);
        thread::spawn(move || run_worker(&shared, &*read_dir, &active[worker_index]));
    }

    let outcome = shared.queue.wait(STALL_TIMEOUT, || {
        eprintln!("Stuck directories:");
        for path in active.iter() {
            let path = path.lock().unwrap();
            if !path.is_empty() {
                eprintln!("  {}", plain_path(Path::new(&*path)).display());
            }
        }
    });
    // Taken under the write lock, so no worker is still sending once `ScanDone` goes out, and none
    // can send after it.
    if let Some(tx) = shared.tx.write().unwrap().take() {
        let _ = tx.send(ScanEvent::ScanDone { outcome });
    }
    // A worker still stuck in a syscall after a stall keeps `shared` alive until it returns; let it
    // hold on to no more than it needs.
    shared.visited_dirs.clear();
    shared.visited_files.clear();
}

struct Shared<H> {
    queue: WorkQueue<DirJob<H>>,
    /// The scan's events go here until `run_scan` takes it to send `ScanDone`.
    tx: RwLock<Option<Sender<ScanEvent>>>,
    /// Directories already queued, so one reachable under two names (a bind mount, a firmlink) is
    /// read once.
    visited_dirs: DashSet<u64>,
    /// Files with several names (hardlinks), counted under the first name seen.
    visited_files: DashSet<u64>,
}

fn run_worker<H, F>(shared: &Shared<H>, read_dir: &F, active: &Mutex<String>)
where
    F: Fn(&DirJob<H>, &mut [u8], &mut DirSink<'_, H>) -> bool,
{
    let mut buffer = vec![0u8; READ_BUFFER_SIZE];
    while let Some(job) = shared.queue.take() {
        active.lock().unwrap().clone_from(&job.path);
        let mut sink = DirSink {
            shared,
            dir_id: job.id,
            dir_path: &job.path,
            mtime: 0,
            files: Vec::new(),
            children: Vec::new(),
            entries: 0,
        };
        let opened = read_dir(&job, &mut buffer, &mut sink);
        let DirSink {
            mtime, files, children, ..
        } = sink;

        let DirJob {
            handle,
            id,
            parent_id,
            name,
            ..
        } = job;
        drop(handle);
        // One event per directory, carrying its files: a channel send and a tree lookup per file
        // would cost more than the files themselves.
        if opened && let Some(tx) = &*shared.tx.read().unwrap() {
            let _ = tx.send(ScanEvent::Dir {
                id,
                parent: parent_id,
                name,
                mtime,
                files,
            });
        }
        // Children are queued before this directory is finished, so the queue never looks empty
        // to the other workers while there is still work to discover.
        shared.queue.push(children);
        shared.queue.finish_one();
        active.lock().unwrap().clear();
    }
}

/// Where a platform's `read_dir` reports the entries of the directory being read.
pub struct DirSink<'a, H> {
    shared: &'a Shared<H>,
    dir_id: u64,
    dir_path: &'a str,
    mtime: i64,
    files: Vec<ScannedFile>,
    children: Vec<DirJob<H>>,
    /// Entries reported so far, for recording progress every `ENTRIES_PER_PROGRESS`.
    entries: u32,
}

impl<H> DirSink<'_, H> {
    /// The full path of the entry `name` in this directory.
    pub fn child_path(&self, name: &str) -> String {
        format!(
            "{}{MAIN_SEPARATOR}{name}",
            self.dir_path.trim_end_matches(MAIN_SEPARATOR)
        )
    }

    /// A subdirectory to read, through `handle`. One already reached under another name is skipped
    /// (and `handle` dropped).
    pub fn dir(&mut self, id: u64, name: &str, path: String, handle: H) {
        self.count_entry();
        if self.shared.visited_dirs.insert(id) {
            self.children.push(DirJob {
                handle,
                id,
                parent_id: self.dir_id,
                name: name.to_owned(),
                path,
            });
        }
    }

    /// A file of `size` bytes, modified at `mtime` (Unix seconds). Pass its identity as `hardlink`
    /// when it has other names (a link count above 1): it is then counted under the first name
    /// seen, as `du` does.
    pub fn file(&mut self, name: impl Into<String>, size: u64, mtime: i64, hardlink: Option<u64>) {
        self.count_entry();
        if hardlink.is_some_and(|id| !self.shared.visited_files.insert(id)) {
            return;
        }
        self.mtime = self.mtime.max(mtime);
        if size > 0 {
            self.files.push(ScannedFile {
                name: name.into(),
                size,
                mtime,
            });
        }
    }

    /// Call after each read from the OS. Records progress for stall detection, and returns false
    /// once the scan is cancelled, so reading a huge directory stops early.
    pub fn checkpoint(&self) -> bool {
        self.shared.queue.record_progress();
        !self.shared.queue.is_cancelled()
    }

    fn count_entry(&mut self) {
        self.entries += 1;
        if self.entries.is_multiple_of(ENTRIES_PER_PROGRESS) {
            self.shared.queue.record_progress();
        }
    }
}

struct QueueState<T> {
    items: Vec<T>,
    /// Items queued or being processed.
    pending: usize,
}

/// A LIFO queue of directories shared by the workers. It finishes when `pending` reaches zero, or
/// is cancelled for good: by the caller's flag, or by the waiter when the scan stalls. Once
/// cancelled, `take` returns `None` and `push` drops its items, so the walk stops instead of
/// carrying on behind a scan already reported as done.
struct WorkQueue<T> {
    state: Mutex<QueueState<T>>,
    /// Wakes workers blocked in `take` when items arrive or the work runs out.
    work: Condvar,
    /// Wakes the waiter when the work runs out or the queue is cancelled. Kept apart from `work` so
    /// a `push`'s `notify_one` can never be absorbed by the waiter instead of an idle worker.
    idle: Condvar,
    /// The caller's cancel flag.
    cancel: Arc<AtomicBool>,
    /// Set (under the lock) once the queue is cancelled.
    cancelled: AtomicBool,
    /// Bumped on every read from the OS, every few entries and every finished item; the waiter
    /// watches it for stalls.
    progress: AtomicU64,
}

impl<T> WorkQueue<T> {
    fn new(cancel: Arc<AtomicBool>) -> Self {
        Self {
            state: Mutex::new(QueueState {
                items: Vec::new(),
                pending: 0,
            }),
            work: Condvar::new(),
            idle: Condvar::new(),
            cancel,
            cancelled: AtomicBool::new(false),
            progress: AtomicU64::new(0),
        }
    }

    fn push(&self, items: Vec<T>) {
        if items.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if self.cancelled.load(Ordering::Relaxed) {
            return;
        }
        let count = items.len();
        state.pending += count;
        state.items.extend(items);
        if count == 1 {
            self.work.notify_one();
        } else {
            self.work.notify_all();
        }
    }

    /// The next item, blocking until one is available. `None` once the work is done or the queue is
    /// cancelled. Every item taken must be followed by `finish_one`, after its children are pushed.
    fn take(&self) -> Option<T> {
        let mut state = self.state.lock().unwrap();
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                self.cancel_locked(&mut state);
            }
            if self.cancelled.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(item) = state.items.pop() {
                return Some(item);
            }
            if state.pending == 0 {
                return None;
            }
            state = self.work.wait(state).unwrap();
        }
    }

    fn finish_one(&self) {
        let mut state = self.state.lock().unwrap();
        // Exact: cancelling removes only the queued items from `pending`, and every taken item
        // reaches here exactly once.
        state.pending -= 1;
        self.record_progress();
        if state.pending == 0 {
            self.work.notify_all();
            self.idle.notify_all();
        }
    }

    fn record_progress(&self) {
        self.progress.fetch_add(1, Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed) || self.cancel.load(Ordering::Relaxed)
    }

    fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        self.cancel_locked(&mut state);
    }

    fn cancel_locked(&self, state: &mut QueueState<T>) {
        if self.cancelled.swap(true, Ordering::Relaxed) {
            return;
        }
        state.pending -= state.items.len();
        state.items.clear();
        self.work.notify_all();
        self.idle.notify_all();
    }

    /// Block until the work is done, the caller's flag cancels it, or no progress has been made for
    /// `stall_timeout`; in that last case call `on_stall`, then cancel the queue.
    fn wait(&self, stall_timeout: Duration, on_stall: impl FnOnce()) -> ScanOutcome {
        let mut last_progress = self.progress.load(Ordering::Relaxed);
        let mut quiet = Duration::ZERO;
        let mut state = self.state.lock().unwrap();
        loop {
            state = self
                .idle
                .wait_timeout_while(state, POLL_INTERVAL, |state| {
                    state.pending != 0 && !self.cancelled.load(Ordering::Relaxed)
                })
                .unwrap()
                .0;
            if self.cancelled.load(Ordering::Relaxed) {
                return ScanOutcome::Cancelled;
            }
            if state.pending == 0 {
                return ScanOutcome::Complete;
            }
            if self.cancel.load(Ordering::Relaxed) {
                self.cancel_locked(&mut state);
                return ScanOutcome::Cancelled;
            }
            let progress = self.progress.load(Ordering::Relaxed);
            if progress != last_progress {
                last_progress = progress;
                quiet = Duration::ZERO;
                continue;
            }
            quiet += POLL_INTERVAL;
            if quiet >= stall_timeout {
                drop(state);
                eprintln!("Scan stalled, finishing with partial results");
                on_stall();
                self.cancel();
                return ScanOutcome::Stalled;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::mpsc;
    use std::time::Instant;

    fn queue<T>() -> WorkQueue<T> {
        WorkQueue::new(Arc::default())
    }

    fn pending<T>(q: &WorkQueue<T>) -> usize {
        q.state.lock().unwrap().pending
    }

    #[test]
    fn push_and_take_lifo() {
        let q = queue();
        q.push(vec![1, 2, 3]);
        for expected in [3, 2, 1] {
            assert_eq!(q.take(), Some(expected));
            q.finish_one();
        }
        assert_eq!(q.take(), None);
    }

    #[test]
    fn pending_counts_queued_and_in_flight_items() {
        let q = queue();
        q.push(vec![10, 20, 30]);
        assert_eq!(pending(&q), 3);
        let _ = q.take().unwrap();
        assert_eq!(pending(&q), 3, "a taken item stays pending until finished");
        q.finish_one();
        assert_eq!(pending(&q), 2);
    }

    #[test]
    fn push_empty_is_a_noop() {
        let q: WorkQueue<i32> = queue();
        q.push(vec![]);
        assert_eq!(pending(&q), 0);
        assert_eq!(q.take(), None);
    }

    #[test]
    fn cancel_is_sticky_and_keeps_in_flight_items_pending() {
        let q = queue();
        q.push(vec![1, 2, 3]);
        let _in_flight = q.take().unwrap();

        q.cancel();
        assert_eq!(pending(&q), 1, "only the in-flight item is still pending");
        assert_eq!(q.take(), None, "nothing more is handed out");
        q.push(vec![4, 5]);
        assert_eq!(pending(&q), 1, "children of an in-flight item are dropped");
        assert_eq!(q.take(), None);

        q.finish_one();
        assert_eq!(pending(&q), 0);
    }

    #[test]
    fn concurrent_processing_takes_every_item_once() {
        let q = Arc::new(queue());
        q.push((0..100).collect());
        let collected = Arc::new(Mutex::new(Vec::new()));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let q = Arc::clone(&q);
                let collected = Arc::clone(&collected);
                thread::spawn(move || {
                    while let Some(value) = q.take() {
                        collected.lock().unwrap().push(value);
                        q.finish_one();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let mut result = collected.lock().unwrap().clone();
        result.sort();
        assert_eq!(result, (0..100).collect::<Vec<_>>());
        assert_eq!(pending(&q), 0);
    }

    #[test]
    fn take_blocks_until_the_last_item_finishes() {
        let q = Arc::new(queue());
        q.push(vec![1]);
        let _ = q.take().unwrap();
        let waiter = {
            let q = Arc::clone(&q);
            thread::spawn(move || q.take().is_none())
        };
        q.finish_one();
        assert!(waiter.join().unwrap());
    }

    /// Workers must stay alive while items spawn child items. If finish_one() ran before the
    /// children were pushed, other workers would see pending == 0 and exit, collapsing the scan
    /// to one thread.
    #[test]
    fn workers_stay_alive_during_child_spawning() {
        let q = Arc::new(queue());
        q.push(vec![0u32]);
        let thread_ids = Arc::new(Mutex::new(HashSet::new()));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let q = Arc::clone(&q);
                let thread_ids = Arc::clone(&thread_ids);
                thread::spawn(move || {
                    while let Some(depth) = q.take() {
                        thread_ids.lock().unwrap().insert(thread::current().id());
                        thread::sleep(Duration::from_micros(100));
                        if depth < 3 {
                            q.push(vec![depth + 1; 4]);
                        }
                        q.finish_one();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let unique = thread_ids.lock().unwrap().len();
        assert!(unique >= 2, "only {unique} thread(s) took part");
    }

    #[test]
    fn wait_returns_as_soon_as_work_finishes() {
        let q = Arc::new(queue());
        q.push(vec![1]);
        let worker = {
            let q = Arc::clone(&q);
            thread::spawn(move || {
                while q.take().is_some() {
                    thread::sleep(Duration::from_millis(10));
                    q.finish_one();
                }
            })
        };
        let started = Instant::now();
        assert_eq!(
            q.wait(STALL_TIMEOUT, || panic!("no stall expected")),
            ScanOutcome::Complete
        );
        // Well under the polling interval: completion wakes the waiter directly.
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "took {:?}",
            started.elapsed()
        );
        worker.join().unwrap();
    }

    #[test]
    fn caller_flag_cancels_and_wakes_blocked_workers() {
        let cancel = Arc::new(AtomicBool::new(false));
        let q = Arc::new(WorkQueue::new(Arc::clone(&cancel)));
        q.push(vec![1]);
        let _in_flight = q.take().unwrap(); // keeps the work unfinished
        let blocked = {
            let q = Arc::clone(&q);
            thread::spawn(move || q.take())
        };
        cancel.store(true, Ordering::Relaxed);
        let started = Instant::now();
        assert_eq!(
            q.wait(STALL_TIMEOUT, || panic!("no stall expected")),
            ScanOutcome::Cancelled
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "took {:?}",
            started.elapsed()
        );
        assert_eq!(blocked.join().unwrap(), None, "a blocked worker must be released");
    }

    #[test]
    fn a_stuck_worker_is_a_stall() {
        let q = queue();
        q.push(vec![1]);
        let _stuck = q.take().unwrap(); // never finished, no progress recorded
        let reported = AtomicBool::new(false);
        let outcome = q.wait(Duration::from_millis(400), || reported.store(true, Ordering::Relaxed));
        assert_eq!(outcome, ScanOutcome::Stalled);
        assert!(
            reported.load(Ordering::Relaxed),
            "the stuck directories must be reported"
        );
        assert!(q.is_cancelled(), "a stall cancels the queue");
    }

    /// A worker that keeps reading one huge directory makes progress without finishing an item,
    /// and must not be mistaken for a stall.
    #[test]
    fn a_long_directory_that_keeps_reading_is_not_a_stall() {
        let q = Arc::new(queue());
        q.push(vec![1]);
        let worker = {
            let q = Arc::clone(&q);
            thread::spawn(move || {
                let _ = q.take().unwrap();
                for _ in 0..20 {
                    thread::sleep(Duration::from_millis(50)); // 1s in all, past the timeout below
                    q.record_progress();
                }
                q.finish_one();
            })
        };
        let outcome = q.wait(Duration::from_millis(400), || panic!("reading is progress"));
        assert_eq!(outcome, ScanOutcome::Complete);
        worker.join().unwrap();
    }

    /// An in-memory tree for driving `run_scan`: each directory lists its files and subdirectories
    /// by index.
    struct FakeDir {
        id: u64,
        name: &'static str,
        files: Vec<(&'static str, u64, i64, Option<u64>)>,
        dirs: Vec<usize>,
    }

    fn fake_read_dir(
        tree: Arc<Vec<FakeDir>>,
    ) -> impl Fn(&DirJob<usize>, &mut [u8], &mut DirSink<'_, usize>) -> bool + Send + Sync + 'static {
        move |job, _buffer, sink| {
            let dir = &tree[job.handle];
            for &(name, size, mtime, hardlink) in &dir.files {
                sink.file(name, size, mtime, hardlink);
            }
            for &child in &dir.dirs {
                let child_dir = &tree[child];
                sink.dir(child_dir.id, child_dir.name, sink.child_path(child_dir.name), child);
            }
            sink.checkpoint();
            true
        }
    }

    /// The events of a scan of `tree` from its first directory, and how it ended.
    fn scan_fake_tree(tree: &Arc<Vec<FakeDir>>) -> (Vec<ScanEvent>, ScanOutcome) {
        let (tx, rx) = mpsc::channel();
        let root = format!("/{}", tree[0].name);
        run_scan(
            Path::new(&root),
            0,
            tree[0].id,
            tx,
            Arc::default(),
            4,
            fake_read_dir(Arc::clone(tree)),
        );
        let events: Vec<_> = rx.iter().collect();
        let Some(&ScanEvent::ScanDone { outcome }) = events.last() else {
            panic!("the last event must be ScanDone: {events:?}");
        };
        (events, outcome)
    }

    #[test]
    fn run_scan_reports_each_directory_and_file_once() {
        let tree = Arc::new(vec![
            FakeDir {
                id: 1,
                name: "root",
                files: vec![("a.txt", 10, 100, None), ("empty", 0, 900, None)],
                dirs: vec![1, 2],
            },
            FakeDir {
                id: 2,
                name: "x",
                files: vec![("link1", 5, 50, Some(77)), ("b", 3, 30, None)],
                // A second route to "y" (a bind mount, say): it is read once.
                dirs: vec![2],
            },
            FakeDir {
                id: 3,
                name: "y",
                files: vec![("link2", 5, 50, Some(77))], // the same file as x/link1
                dirs: vec![],
            },
        ]);
        let (events, outcome) = scan_fake_tree(&tree);
        assert_eq!(outcome, ScanOutcome::Complete);
        assert!(matches!(&events[0], ScanEvent::ScanStart { root } if root == Path::new("/root")));

        let mut dirs: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ScanEvent::Dir {
                    id,
                    parent,
                    mtime,
                    files,
                    ..
                } => Some((*id, *parent, files.iter().map(|file| file.size).sum::<u64>(), *mtime)),
                _ => None,
            })
            .collect();
        dirs.sort();
        // The hardlink counts in whichever of x and y was read first; together they hold it once.
        let x = dirs[1];
        let y = dirs[2];
        assert_eq!(dirs[0], (1, 0, 10, 900), "the empty file counts towards mtime only");
        // "y" is a child of both the root and "x"; the root is read first, so it claims it.
        assert_eq!((x.0, x.1, y.0, y.1), (2, 1, 3, 1));
        assert_eq!(x.2 + y.2, 3 + 5, "hardlinked bytes counted once");

        let mut files: Vec<_> = events
            .iter()
            .flat_map(|event| match event {
                ScanEvent::Dir { files, .. } => files.as_slice(),
                _ => &[],
            })
            .map(|file| file.name.as_str())
            .collect();
        files.sort();
        assert_eq!(
            files.len(),
            3,
            "no entry for the empty file or the second link: {files:?}"
        );
        assert!(files.contains(&"a.txt") && files.contains(&"b"));
    }

    #[test]
    fn run_scan_stops_promptly_when_cancelled() {
        let cancel = Arc::new(AtomicBool::new(false));
        let next_id = AtomicU64::new(2);
        let read_dir = {
            let cancel = Arc::clone(&cancel);
            // An endless tree: every directory has two children. Cancel after a few reads.
            move |_: &DirJob<()>, _: &mut [u8], sink: &mut DirSink<'_, ()>| {
                for _ in 0..2 {
                    let id = next_id.fetch_add(1, Ordering::Relaxed);
                    if id == 100 {
                        cancel.store(true, Ordering::Relaxed);
                    }
                    sink.dir(id, "d", sink.child_path("d"), ());
                }
                sink.checkpoint();
                true
            }
        };
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        run_scan(Path::new("/root"), (), 1, tx, cancel, 4, read_dir);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "took {:?}",
            started.elapsed()
        );
        // The channel closes with `ScanDone`, even though workers may still be inside directories.
        let events: Vec<_> = rx.iter().collect();
        assert!(matches!(
            events.last(),
            Some(ScanEvent::ScanDone {
                outcome: ScanOutcome::Cancelled
            })
        ));
        assert!(events.len() < 1000, "the walk must stop: {} events", events.len());
    }

    fn shared() -> Shared<()> {
        Shared {
            queue: queue(),
            tx: RwLock::new(None),
            visited_dirs: DashSet::new(),
            visited_files: DashSet::new(),
        }
    }

    fn sink<'a>(shared: &'a Shared<()>, dir_path: &'a str) -> DirSink<'a, ()> {
        DirSink {
            shared,
            dir_id: 1,
            dir_path,
            mtime: 0,
            files: Vec::new(),
            children: Vec::new(),
            entries: 0,
        }
    }

    #[test]
    fn child_paths_join_with_one_separator() {
        let shared = shared();
        let sep = MAIN_SEPARATOR;
        for (dir, expected) in [
            (format!("{sep}"), format!("{sep}name")),
            (format!("{sep}a"), format!("{sep}a{sep}name")),
            (format!("C:{sep}"), format!("C:{sep}name")),
        ] {
            assert_eq!(sink(&shared, &dir).child_path("name"), expected);
        }
    }

    /// One read from the OS can return tens of thousands of entries, each needing a syscall of its
    /// own; on slow storage that takes longer than the stall timeout, so the entries themselves
    /// must count as progress.
    #[test]
    fn reporting_entries_is_progress() {
        let shared = shared();
        let mut sink = sink(&shared, "/dir");
        let progress = || shared.queue.progress.load(Ordering::Relaxed);
        for index in 0..ENTRIES_PER_PROGRESS {
            sink.file(format!("f{index}"), 1, 0, None);
        }
        assert_eq!(progress(), 1);
        for index in 0..ENTRIES_PER_PROGRESS {
            sink.dir(index.into(), "d", sink.child_path("d"), ());
        }
        assert_eq!(progress(), 2);
    }

    #[test]
    fn root_name_is_the_last_component_or_the_whole_root() {
        assert_eq!(root_name(Path::new("/home/user/projects")), "projects");
        assert_eq!(root_name(Path::new("/")), "/");
    }

    #[test]
    fn plain_paths_lose_the_verbatim_prefix() {
        let plain = |path: &str| plain_path(Path::new(path)).to_str().unwrap().to_owned();
        assert_eq!(plain(r"\\?\C:\Users\me\x.txt"), r"C:\Users\me\x.txt");
        assert_eq!(plain(r"\\?\UNC\server\share\x.txt"), r"\\server\share\x.txt");
        for unchanged in [
            r"C:\Users\me\x.txt",
            r"\\?\Volume{0b1c2d3e-0000-0000-0000-100000000000}\x.txt",
            "/home/me/x.txt",
        ] {
            assert_eq!(plain(unchanged), unchanged);
        }
    }

    #[test]
    fn node_id_is_deterministic_and_nonzero() {
        assert_eq!(node_id(0, 12345), node_id(0, 12345));
        for ino in [0u64, 1, 2, 999, u64::MAX] {
            for dev in [0u64, 1, 16777220, u64::MAX] {
                assert_ne!(node_id(dev, ino), 0, "node id must never be the root sentinel 0");
            }
        }
    }

    #[test]
    fn node_id_distinguishes_same_inode_across_devices() {
        // The whole point: the same inode on two different devices must not collide.
        assert_ne!(node_id(1, 2), node_id(2, 2));
        assert_ne!(node_id(0x0100_0004, 2), node_id(0x0100_0005, 2));
    }
}
