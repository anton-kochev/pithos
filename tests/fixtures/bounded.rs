//! Bounded concurrency for fake-Docker tests. Unbounded parallel runs race the
//! fixed Docker call limits on macOS (slow Python startup, first-exec scans);
//! fully serial runs are needlessly slow.
use std::sync::{Condvar, Mutex};

pub struct Bounded {
    active: Mutex<usize>,
    freed: Condvar,
    limit: usize,
}

pub struct Permit<'a>(&'a Bounded);

impl Bounded {
    pub const fn new(limit: usize) -> Self {
        Self {
            active: Mutex::new(0),
            freed: Condvar::new(),
            limit,
        }
    }

    pub fn acquire(&self) -> Permit<'_> {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        while *active >= self.limit {
            active = self.freed.wait(active).unwrap_or_else(|e| e.into_inner());
        }
        *active += 1;
        Permit(self)
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *self.0.active.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        self.0.freed.notify_one();
    }
}
