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

pub struct WorkGuard<'a, T> {
    queue: &'a WorkQueue<T>,
    item: Option<T>,
}

impl<T> WorkGuard<'_, T> {
    pub fn into_inner(mut self) -> T {
        self.item.take().unwrap()
    }
}

impl<T> std::ops::Deref for WorkGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.item.as_ref().unwrap()
    }
}

impl<T> std::ops::DerefMut for WorkGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.item.as_mut().unwrap()
    }
}

impl<T> Drop for WorkGuard<'_, T> {
    fn drop(&mut self) {
        self.queue.finish_one();
    }
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

    pub fn take(&self) -> Option<WorkGuard<'_, T>> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if let Some(item) = inner.queue.pop() {
                return Some(WorkGuard {
                    queue: self,
                    item: Some(item),
                });
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
        assert_eq!(*q.take().unwrap(), 3);
        assert_eq!(*q.take().unwrap(), 2);
        assert_eq!(*q.take().unwrap(), 1);
    }

    #[test]
    fn pending_count() {
        let q = WorkQueue::new();
        q.push(vec![10, 20, 30]);
        assert_eq!(q.pending(), 3);

        let guard = q.take().unwrap();
        assert_eq!(q.pending(), 3);

        drop(guard);
        assert_eq!(q.pending(), 2);

        drop(q.take().unwrap());
        assert_eq!(q.pending(), 1);
        drop(q.take().unwrap());
        assert_eq!(q.pending(), 0);
    }

    #[test]
    fn into_inner() {
        let q = WorkQueue::new();
        q.push(vec![42]);
        let guard = q.take().unwrap();
        let val = guard.into_inner();
        assert_eq!(val, 42);
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
    fn deref_and_deref_mut() {
        let q = WorkQueue::new();
        q.push(vec![String::from("hello")]);

        let mut guard = q.take().unwrap();
        assert_eq!(guard.len(), 5);
        guard.push_str(" world");
        assert_eq!(&*guard, "hello world");
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
                    while let Some(guard) = q.take() {
                        let val = guard.into_inner();
                        collected.lock().unwrap().push(val);
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

        let guard = q.take().unwrap();
        assert_eq!(*guard, 1);

        let q2 = Arc::clone(&q);
        let handle = thread::spawn(move || q2.take().is_none());

        drop(guard);

        assert!(handle.join().unwrap());
        assert_eq!(q.pending(), 0);
    }
}
