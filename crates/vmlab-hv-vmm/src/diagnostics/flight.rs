//! Fixed-size, allocation-free vCPU exit history. Payload bytes are never recorded.

use parking_lot::Mutex;
use serde::Serialize;

pub const EXIT_CAPACITY: usize = 256;

#[derive(Clone, Copy, Default, Serialize)]
pub struct ExitRecord {
    pub unix_ns: u64,
    /// Numeric KVM_EXIT_* value; see linux/kvm.h.
    pub reason: u32,
    pub rip: Option<u64>,
    pub address: Option<u64>,
    pub size: u32,
    pub write: Option<bool>,
    pub errno: Option<i32>,
}

pub struct ExitRing {
    records: Mutex<History>,
}

struct History {
    entries: [ExitRecord; EXIT_CAPACITY],
    next: usize,
    len: usize,
}

impl Default for ExitRing {
    fn default() -> Self {
        Self {
            records: Mutex::new(History {
                entries: [ExitRecord::default(); EXIT_CAPACITY],
                next: 0,
                len: 0,
            }),
        }
    }
}

impl ExitRing {
    #[inline]
    pub fn record(&self, record: ExitRecord) {
        let mut history = self.records.lock();
        let next = history.next;
        history.entries[next] = record;
        history.next = (next + 1) % EXIT_CAPACITY;
        history.len = (history.len + 1).min(EXIT_CAPACITY);
    }

    /// Panic paths must not wait for the faulting thread's locks.
    pub fn snapshot(&self) -> Option<Vec<ExitRecord>> {
        let history = self.records.try_lock()?;
        let start = (history.next + EXIT_CAPACITY - history.len) % EXIT_CAPACITY;
        Some(
            (0..history.len)
                .map(|i| history.entries[(start + i) % EXIT_CAPACITY])
                .collect(),
        )
    }
}

pub fn unix_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_and_order() {
        let ring = ExitRing::default();
        for reason in 0..1000 {
            ring.record(ExitRecord {
                reason,
                ..Default::default()
            });
        }
        let entries = ring.snapshot().unwrap();
        assert_eq!(entries.len(), EXIT_CAPACITY);
        assert_eq!(entries[0].reason, 744);
        assert_eq!(entries[255].reason, 999);
    }

    #[test]
    fn snapshot_never_waits_for_the_faulting_thread() {
        let ring = ExitRing::default();
        let _guard = ring.records.lock();
        assert!(ring.snapshot().is_none());
    }

    #[test]
    #[ignore = "manual release-mode microbenchmark; no VM boot"]
    fn recorder_overhead() {
        use std::hint::black_box;
        const N: u32 = 2_000_000;
        let ring = ExitRing::default();
        let begin = std::time::Instant::now();
        for reason in 0..N {
            black_box(ExitRecord {
                reason: black_box(reason),
                ..Default::default()
            });
        }
        let baseline = begin.elapsed();
        let begin = std::time::Instant::now();
        for reason in 0..N {
            ring.record(black_box(ExitRecord {
                unix_ns: unix_ns(),
                reason: black_box(reason),
                rip: Some(0x1234),
                address: Some(0x3f8),
                size: 1,
                write: Some(true),
                errno: None,
            }));
        }
        let recorded = begin.elapsed();
        eprintln!(
            "flight recorder: baseline {:.2} ns/exit; recorded {:.2} ns/exit; delta {:.2} ns/exit (N={N})",
            baseline.as_nanos() as f64 / N as f64,
            recorded.as_nanos() as f64 / N as f64,
            (recorded.as_nanos() as f64 - baseline.as_nanos() as f64) / N as f64
        );
        black_box(ring.snapshot());
    }
}
