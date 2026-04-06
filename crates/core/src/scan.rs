use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
pub fn raise_fd_limit() {
    unsafe {
        let mut rlim: libc::rlimit = std::mem::zeroed();
        libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim);
        rlim.rlim_cur = rlim.rlim_max;
        libc::setrlimit(libc::RLIMIT_NOFILE, &rlim);
    }
}

struct WorkQueueInner<T> {
    queue: Vec<T>,
    pending: usize,
}

pub struct WorkQueue<T> {
    inner: Mutex<WorkQueueInner<T>>,
    condvar: Condvar,
}

impl<T> Default for WorkQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> WorkQueue<T> {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(WorkQueueInner {
                queue: Vec::new(),
                pending: 0,
            }),
            condvar: Condvar::new(),
        }
    }

    pub fn push(&self, items: Vec<T>) {
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

    /// Takes the next item from the queue, blocking until one is available.
    /// Returns `None` when all work is done (queue empty and pending == 0).
    /// The caller MUST call `finish_one()` after processing the item.
    pub fn take(&self) -> Option<T> {
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

    /// Signals that one work item has been fully processed.
    /// Must be called AFTER processing is complete and any child items have been pushed.
    pub fn finish_one(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.pending -= 1;
        if inner.pending == 0 {
            self.condvar.notify_all();
        }
    }

    pub fn pending(&self) -> usize {
        self.inner.lock().unwrap().pending
    }

    pub fn cancel(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.pending = 0;
        inner.queue.clear();
        self.condvar.notify_all();
    }

    #[cfg(test)]
    fn queue_len(&self) -> usize {
        self.inner.lock().unwrap().queue.len()
    }

    pub fn wait_with_stall_detection(&self, on_stall: impl Fn()) {
        let mut stall_count = 0u32;
        let mut last_pending = usize::MAX;
        loop {
            thread::sleep(Duration::from_millis(200));
            let pending = self.pending();
            if pending == 0 {
                break;
            }
            if pending == last_pending {
                stall_count += 1;
                if stall_count >= 15 {
                    eprintln!("Scan stalled with {pending} items pending, finishing with partial results");
                    on_stall();
                    self.cancel();
                    break;
                }
            } else {
                stall_count = 0;
            }
            last_pending = pending;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn push_and_take_lifo() {
        let q = WorkQueue::new();
        q.push(vec![1, 2, 3]);
        assert_eq!(q.take().unwrap(), 3);
        q.finish_one();
        assert_eq!(q.take().unwrap(), 2);
        q.finish_one();
        assert_eq!(q.take().unwrap(), 1);
        q.finish_one();
    }

    #[test]
    fn pending_count() {
        let q = WorkQueue::new();
        q.push(vec![10, 20, 30]);
        assert_eq!(q.pending(), 3);

        let _ = q.take().unwrap();
        assert_eq!(q.pending(), 3);
        q.finish_one();
        assert_eq!(q.pending(), 2);

        let _ = q.take().unwrap();
        q.finish_one();
        assert_eq!(q.pending(), 1);

        let _ = q.take().unwrap();
        q.finish_one();
        assert_eq!(q.pending(), 0);
    }

    #[test]
    fn push_empty_noop() {
        let q: WorkQueue<i32> = WorkQueue::new();
        q.push(vec![]);
        assert_eq!(q.pending(), 0);
        assert_eq!(q.queue_len(), 0);
    }

    #[test]
    fn cancel_clears() {
        let q = WorkQueue::new();
        q.push(vec![1, 2, 3]);
        assert_eq!(q.pending(), 3);

        q.cancel();
        assert_eq!(q.pending(), 0);
        assert_eq!(q.queue_len(), 0);
    }

    #[test]
    fn take_returns_none_when_empty_and_no_pending() {
        let q: WorkQueue<i32> = WorkQueue::new();
        assert!(q.take().is_none());
    }

    #[test]
    fn concurrent_processing() {
        let q = Arc::new(WorkQueue::new());
        let items: Vec<i32> = (0..100).collect();
        q.push(items);

        let collected = Arc::new(Mutex::new(Vec::new()));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let q = Arc::clone(&q);
                let collected = Arc::clone(&collected);
                thread::spawn(move || {
                    while let Some(val) = q.take() {
                        collected.lock().unwrap().push(val);
                        q.finish_one();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(q.pending(), 0);

        let mut result = collected.lock().unwrap().clone();
        result.sort();
        let expected: Vec<i32> = (0..100).collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn take_blocks_then_unblocks() {
        let q = Arc::new(WorkQueue::new());
        q.push(vec![1]);

        let val = q.take().unwrap();
        assert_eq!(val, 1);

        let q2 = Arc::clone(&q);
        let handle = thread::spawn(move || q2.take().is_none());

        q.finish_one();

        assert!(handle.join().unwrap());
        assert_eq!(q.pending(), 0);
    }

    /// Regression test: workers must stay alive while items spawn child items.
    /// If finish_one() is called before child items are pushed, other workers
    /// see pending==0 and exit, collapsing the scan to single-threaded.
    #[test]
    fn workers_stay_alive_during_child_spawning() {
        let q = Arc::new(WorkQueue::new());
        let thread_ids = Arc::new(Mutex::new(std::collections::HashSet::new()));

        // Push one root item that will spawn children
        q.push(vec![0u32]);

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let q = Arc::clone(&q);
                let thread_ids = Arc::clone(&thread_ids);
                thread::spawn(move || {
                    while let Some(depth) = q.take() {
                        thread_ids.lock().unwrap().insert(thread::current().id());

                        // Simulate I/O work that discovers children
                        thread::sleep(Duration::from_micros(100));
                        if depth < 3 {
                            q.push(vec![depth + 1; 4]);
                        }
                        q.finish_one();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(q.pending(), 0);
        let unique_threads = thread_ids.lock().unwrap().len();
        assert!(
            unique_threads >= 2,
            "only {unique_threads} thread(s) participated — work queue lost parallelism"
        );
    }
}
