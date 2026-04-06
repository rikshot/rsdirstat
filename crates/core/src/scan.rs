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
                return Some(WorkGuard { queue: self, item: Some(item) });
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
