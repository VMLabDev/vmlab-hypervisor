use std::sync::{Arc, Condvar, Mutex};

/// One publication of a reusable allocation. Invalidating it excludes new readers.
#[derive(Default)]
pub struct BufferLease {
    state: Mutex<(bool, usize)>,
    released: Condvar,
}

impl BufferLease {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((true, 0)),
            released: Condvar::new(),
        })
    }

    pub fn acquire(self: &Arc<Self>) -> Option<BufferGuard> {
        let mut state = self.state.lock().unwrap();
        if !state.0 {
            return None;
        }
        state.1 += 1;
        Some(BufferGuard(self.clone()))
    }

    /// Stop lending this publication and wait for all existing readers.
    pub fn retire(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = false;
        while state.1 != 0 {
            state = self.released.wait(state).unwrap();
        }
    }

    pub fn try_retire(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.1 != 0 {
            return false;
        }
        state.0 = false;
        true
    }
}

pub struct BufferGuard(Arc<BufferLease>);

impl Drop for BufferGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.1 -= 1;
        self.0.released.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retirement_excludes_new_readers_and_waits_for_all_releases() {
        let lease = BufferLease::new();
        let first = lease.acquire().unwrap();
        let second = lease.acquire().unwrap();
        let writer = lease.clone();
        let (started, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            writer.retire();
        });
        receiver.recv().unwrap();
        while lease.state.lock().unwrap().0 {
            std::thread::yield_now();
        }
        assert!(lease.acquire().is_none());
        drop(first);
        assert!(!worker.is_finished());
        drop(second);
        worker.join().unwrap();
    }
}
