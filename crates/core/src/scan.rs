//! The platform-independent half of a scan: the worker threads, the work queue, and each
//! directory's bookkeeping. A platform crate supplies only how to open the root and how to read
//! one directory ([`run_scan`]'s `read_dir`), reporting each entry to a [`DirSink`].

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use dashmap::DashSet;

/// What a scan reports, in order: `ScanStart`, then the entries, then `ScanDone`. In-process only;
/// nothing here goes over the wire.
#[derive(Clone, Debug)]
pub enum ScanEvent {
    ScanStart {
        path: String,
    },
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
    /// The last event a consumer should apply. After a cancelled or stalled scan, a worker still
    /// inside a directory may send a few more, which must be ignored.
    ScanDone,
}

/// A file of non-zero size in a `ScanEvent::Dir`.
#[derive(Clone, Debug)]
pub struct ScannedFile {
    pub name: String,
    pub size: u64,
    pub mtime: i64,
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
pub fn raise_fd_limit() {
    // The scanner keeps every pending directory fd open at once, so the default soft limit
    // (256 on macOS) is easily exhausted on large trees. Raise it to the hard limit.
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

/// Open the subdirectory `name` of `parent`, or `None` if it can't be opened (it is then left out
/// of the scan, like any unreadable directory).
#[cfg(unix)]
pub fn open_dir_at(parent: &std::os::fd::OwnedFd, name: &std::ffi::CStr) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A path for showing to people: Windows paths are opened with the `\\?\` extended-length prefix,
/// which is noise in messages.
pub fn display_path(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

/// The scan root's name as shown in the breadcrumb: its last component, or the whole path for a
/// filesystem root (`/`, `C:\`).
pub fn root_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || display_path(&path.display().to_string()).to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Separator for building child paths, which on Windows are used to open the children.
const SEPARATOR: char = if cfg!(windows) { '\\' } else { '/' };

/// Size of each worker's buffer for directory reads.
const READ_BUFFER_SIZE: usize = 1024 * 1024;

/// How long the scan may go without any progress before it is treated as stuck. Every directory
/// read counts as progress, so only a syscall blocked this long on every busy worker (a dead
/// network mount, typically) trips it, not a merely slow or huge directory.
const STALL_TIMEOUT: Duration = Duration::from_secs(3);

/// How often the waiting caller samples progress and the cancel flag.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// One directory waiting to be read.
pub struct DirJob<H> {
    /// The open directory (a file descriptor or handle); closed once it has been read.
    pub handle: H,
    pub id: u64,
    pub parent_id: u64,
    pub name: String,
    /// Full path, for opening children by path (Windows) and for reporting a stuck directory.
    pub path: String,
}

/// How a scan ended.
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

/// Walk the tree under `root` on one worker thread per CPU, sending `ScanStart`, then a `File` per
/// non-empty file and a `Dir` per directory (after its files), then `ScanDone`.
///
/// `read_dir` reads one directory: it reports every entry to the [`DirSink`] and calls
/// [`DirSink::checkpoint`] after each read from the OS, stopping when it returns false.
///
/// Returns once the walk finishes, the `cancel` flag is set (within [`POLL_INTERVAL`]), or the scan
/// stalls. In the last two cases a worker may still be inside a directory, and its remaining events
/// can arrive after `ScanDone`: consumers must stop at `ScanDone`. Workers are detached rather than
/// joined so that one blocked in a syscall can't hold the scan open.
pub fn run_scan<H, F>(root: DirJob<H>, tx: Sender<ScanEvent>, cancel: Arc<AtomicBool>, read_dir: F) -> ScanOutcome
where
    H: Send + 'static,
    F: Fn(&DirJob<H>, &mut [u8], &mut DirSink<'_, H>) + Send + Sync + 'static,
{
    let _ = tx.send(ScanEvent::ScanStart {
        path: root.name.clone(),
    });

    let shared = Arc::new(Shared {
        queue: WorkQueue::new(cancel),
        visited_dirs: DashSet::from_iter([root.id]),
        visited_files: DashSet::new(),
    });
    shared.queue.push(vec![root]);

    let threads = thread::available_parallelism().map_or(4, |n| n.get());
    let active: Arc<[Mutex<String>]> = (0..threads).map(|_| Mutex::default()).collect();
    let read_dir = Arc::new(read_dir);
    for worker_index in 0..threads {
        let shared = Arc::clone(&shared);
        let tx = tx.clone();
        let read_dir = Arc::clone(&read_dir);
        let active = Arc::clone(&active);
        thread::spawn(move || run_worker(&shared, &tx, &*read_dir, &active[worker_index]));
    }

    let outcome = shared.queue.wait(STALL_TIMEOUT, || {
        eprintln!("Stuck directories:");
        for path in active.iter() {
            let path = path.lock().unwrap();
            if !path.is_empty() {
                eprintln!("  {}", display_path(&path));
            }
        }
    });
    let _ = tx.send(ScanEvent::ScanDone);
    outcome
}

struct Shared<H> {
    queue: WorkQueue<DirJob<H>>,
    /// Directories already queued, so one reachable under two names (a bind mount, a firmlink) is
    /// read once.
    visited_dirs: DashSet<u64>,
    /// Files with several names (hardlinks), counted under the first name seen.
    visited_files: DashSet<u64>,
}

fn run_worker<H, F>(shared: &Shared<H>, tx: &Sender<ScanEvent>, read_dir: &F, active: &Mutex<String>)
where
    F: Fn(&DirJob<H>, &mut [u8], &mut DirSink<'_, H>),
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
        };
        read_dir(&job, &mut buffer, &mut sink);
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
        let _ = tx.send(ScanEvent::Dir {
            id,
            parent: parent_id,
            name,
            mtime,
            files,
        });
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
}

impl<H> DirSink<'_, H> {
    /// The full path of the entry `name` in this directory.
    pub fn child_path(&self, name: &str) -> String {
        format!("{}{SEPARATOR}{name}", self.dir_path.trim_end_matches(SEPARATOR))
    }

    /// A subdirectory to read, opened as `handle`. One already reached under another name is
    /// skipped (and `handle` closed).
    pub fn dir(&mut self, id: u64, name: &str, path: String, handle: H) {
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
    /// Bumped on every directory read and finished item; the waiter watches it for stalls.
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
    ) -> impl Fn(&DirJob<usize>, &mut [u8], &mut DirSink<'_, usize>) + Send + Sync + 'static {
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
        }
    }

    fn root_job(tree: &[FakeDir]) -> DirJob<usize> {
        DirJob {
            handle: 0,
            id: tree[0].id,
            parent_id: 0,
            name: tree[0].name.to_owned(),
            path: format!("/{}", tree[0].name),
        }
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
        let (tx, rx) = mpsc::channel();
        let outcome = run_scan(root_job(&tree), tx, Arc::default(), fake_read_dir(Arc::clone(&tree)));
        assert_eq!(outcome, ScanOutcome::Complete);

        let events: Vec<_> = rx.iter().collect();
        assert!(matches!(&events[0], ScanEvent::ScanStart { path } if path == "root"));
        assert!(matches!(events.last(), Some(ScanEvent::ScanDone)));

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
            }
        };
        let root = DirJob {
            handle: (),
            id: 1,
            parent_id: 0,
            name: "root".into(),
            path: "/root".into(),
        };
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        assert_eq!(run_scan(root, tx, cancel, read_dir), ScanOutcome::Cancelled);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "took {:?}",
            started.elapsed()
        );
        let events: Vec<_> = rx.iter().take_while(|e| !matches!(e, ScanEvent::ScanDone)).collect();
        assert!(events.len() < 1000, "the walk must stop: {} events", events.len());
    }

    #[test]
    fn child_paths_join_with_one_separator() {
        let shared = Shared {
            queue: queue(),
            visited_dirs: DashSet::new(),
            visited_files: DashSet::new(),
        };
        let sep = SEPARATOR;
        for (dir, expected) in [
            (format!("{sep}"), format!("{sep}name")),
            (format!("{sep}a"), format!("{sep}a{sep}name")),
            (format!("C:{sep}"), format!("C:{sep}name")),
        ] {
            let sink: DirSink<'_, ()> = DirSink {
                shared: &shared,
                dir_id: 1,
                dir_path: &dir,
                mtime: 0,
                files: Vec::new(),
                children: Vec::new(),
            };
            assert_eq!(sink.child_path("name"), expected);
        }
    }

    #[test]
    fn root_name_is_the_last_component_or_the_whole_root() {
        assert_eq!(root_name(Path::new("/home/user/projects")), "projects");
        assert_eq!(root_name(Path::new("/")), "/");
        assert_eq!(display_path(r"\\?\C:\Windows"), r"C:\Windows");
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
