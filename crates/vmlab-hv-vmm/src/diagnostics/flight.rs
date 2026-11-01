//! Per-vCPU, single-writer exit history. Readers copy only complete entries.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering, fence};

use serde::Serialize;

mod clock;
use clock::Clock;

pub const EXIT_CAPACITY: usize = 256;
const WRITING: u64 = 1 << 63;
const HAS_RIP: u8 = 1;
const HAS_ADDRESS: u8 = 2;
const HAS_DIRECTION: u8 = 4;
const IS_WRITE: u8 = 8;
const HAS_ERRNO: u8 = 16;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize)]
pub struct ExitMetadata {
    /// Numeric KVM_EXIT_* value; see linux/kvm.h.
    pub reason: u32,
    pub rip: Option<u64>,
    pub address: Option<u64>,
    pub size: u32,
    pub write: Option<bool>,
    pub errno: Option<i32>,
}

#[derive(Serialize)]
pub struct ExitRecord {
    pub unix_ns: Option<u64>,
    pub host_tsc: Option<u64>,
    #[serde(flatten)]
    pub exit: ExitMetadata,
}

#[repr(C, align(64))]
#[derive(Default)]
struct Published {
    sequence: AtomicU64,
    full: AtomicBool,
}

#[repr(C, align(64))]
#[derive(Default)]
struct Entry {
    sequence: AtomicU64,
    ticks: AtomicU64,
    rip: AtomicU64,
    address: AtomicU64,
    reason: AtomicU32,
    size: AtomicU32,
    errno: AtomicI32,
    flags: AtomicU8,
}

#[repr(C, align(64))]
pub struct ExitRing {
    published: Published,
    entries: [Entry; EXIT_CAPACITY],
    writer_claimed: AtomicBool,
    clock: Clock,
}

pub struct ExitWriter<'a> {
    ring: &'a ExitRing,
    sequence: u64,
    filled: usize,
    clock: Clock,
}

impl Default for ExitRing {
    fn default() -> Self {
        Self::with_clock(Clock::global())
    }
}

impl ExitRing {
    fn with_clock(clock: Clock) -> Self {
        Self {
            published: Published::default(),
            entries: std::array::from_fn(|_| Entry::default()),
            writer_claimed: AtomicBool::new(false),
            clock,
        }
    }

    /// Claim the sole producer once, outside the exit loop.
    pub fn writer(&self) -> Option<ExitWriter<'_>> {
        self.writer_claimed
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        let sequence = self.published.sequence.load(Ordering::Acquire);
        let filled = if self.published.full.load(Ordering::Relaxed) {
            EXIT_CAPACITY
        } else {
            sequence.min(EXIT_CAPACITY as u64) as usize
        };
        Some(ExitWriter {
            ring: self,
            sequence,
            filled,
            clock: self.clock,
        })
    }

    pub fn clock_info(&self) -> impl Serialize {
        self.clock
    }

    #[cfg(test)]
    #[inline]
    pub fn timestamp_for_benchmark(&self) -> u64 {
        self.clock.capture()
    }

    /// Reject overwritten entries without waiting for the producer.
    pub fn snapshot(&self) -> Option<Vec<ExitRecord>> {
        let published = self.published.sequence.load(Ordering::Acquire);
        let len = if self.published.full.load(Ordering::Relaxed) {
            EXIT_CAPACITY
        } else {
            published.min(EXIT_CAPACITY as u64) as usize
        };
        let first = published.wrapping_sub(len as u64).wrapping_add(1);
        let mut copied = Vec::with_capacity(len);
        for offset in 0..len {
            let sequence = first.wrapping_add(offset as u64);
            let entry = &self.entries[sequence.wrapping_sub(1) as usize % EXIT_CAPACITY];
            if entry.sequence.load(Ordering::Acquire) != sequence {
                return None;
            }
            let ticks = entry.ticks.load(Ordering::Relaxed);
            let flags = entry.flags.load(Ordering::Relaxed);
            let exit = ExitMetadata {
                reason: entry.reason.load(Ordering::Relaxed),
                rip: (flags & HAS_RIP != 0).then_some(entry.rip.load(Ordering::Relaxed)),
                address: (flags & HAS_ADDRESS != 0)
                    .then_some(entry.address.load(Ordering::Relaxed)),
                size: entry.size.load(Ordering::Relaxed),
                write: (flags & HAS_DIRECTION != 0).then_some(flags & IS_WRITE != 0),
                errno: (flags & HAS_ERRNO != 0).then_some(entry.errno.load(Ordering::Relaxed)),
            };
            // If any field came from the next overwrite, this acquire fence
            // pairs with its release fence. The second sequence read must
            // then see its invalidation or a newer completed sequence.
            fence(Ordering::Acquire);
            if entry.sequence.load(Ordering::Relaxed) != sequence {
                return None;
            }
            copied.push((ticks, exit));
        }
        let anchor = self.clock.anchor();
        Some(
            copied
                .into_iter()
                .map(|(ticks, exit)| {
                    let (unix_ns, host_tsc) = self.clock.convert(ticks, anchor.as_ref());
                    ExitRecord {
                        unix_ns,
                        host_tsc,
                        exit,
                    }
                })
                .collect(),
        )
    }
}

impl ExitWriter<'_> {
    /// No allocation, formatting, locks, or atomic RMWs on this path.
    #[inline]
    pub fn record(&mut self, exit: ExitMetadata) {
        let ticks = self.clock.capture();
        self.sequence = self.sequence.wrapping_add(1);
        let entry = &self.ring.entries[self.sequence.wrapping_sub(1) as usize % EXIT_CAPACITY];
        // The invalid tag is half a u64 cycle away from any live generation,
        // including when the sequence wraps through zero.
        entry
            .sequence
            .store(self.sequence ^ WRITING, Ordering::Relaxed);
        fence(Ordering::Release);
        let mut flags = 0;
        if let Some(rip) = exit.rip {
            entry.rip.store(rip, Ordering::Relaxed);
            flags |= HAS_RIP;
        }
        if let Some(address) = exit.address {
            entry.address.store(address, Ordering::Relaxed);
            flags |= HAS_ADDRESS;
        }
        if let Some(write) = exit.write {
            flags |= HAS_DIRECTION;
            if write {
                flags |= IS_WRITE;
            }
        }
        if let Some(errno) = exit.errno {
            entry.errno.store(errno, Ordering::Relaxed);
            flags |= HAS_ERRNO;
        }
        entry.ticks.store(ticks, Ordering::Relaxed);
        entry.reason.store(exit.reason, Ordering::Relaxed);
        entry.size.store(exit.size, Ordering::Relaxed);
        entry.flags.store(flags, Ordering::Relaxed);
        entry.sequence.store(self.sequence, Ordering::Release);
        if self.filled < EXIT_CAPACITY {
            self.filled += 1;
            if self.filled == EXIT_CAPACITY {
                self.ring.published.full.store(true, Ordering::Release);
            }
        }
        self.ring
            .published
            .sequence
            .store(self.sequence, Ordering::Release);
    }
}

impl Drop for ExitWriter<'_> {
    fn drop(&mut self) {
        self.ring.writer_claimed.store(false, Ordering::Release);
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

    fn fixture() -> ExitRing {
        ExitRing::with_clock(Clock::SystemTime)
    }

    #[test]
    fn bounds_order_and_single_writer_lease() {
        let ring = fixture();
        let mut writer = ring.writer().unwrap();
        assert!(ring.writer().is_none());
        for reason in 0..1000 {
            writer.record(ExitMetadata {
                reason,
                ..Default::default()
            });
        }
        let entries = ring.snapshot().unwrap();
        assert_eq!(entries.len(), EXIT_CAPACITY);
        assert_eq!(entries[0].exit.reason, 744);
        assert_eq!(entries[255].exit.reason, 999);
        drop(writer);
        let mut writer = ring.writer().unwrap();
        writer.record(ExitMetadata {
            reason: 1000,
            ..Default::default()
        });
        assert_eq!(ring.snapshot().unwrap().last().unwrap().exit.reason, 1000);
        assert_eq!(std::mem::align_of::<ExitRing>(), 64);
        assert_eq!(std::mem::size_of::<Entry>(), 64);
    }

    #[test]
    fn snapshot_never_waits_for_the_faulting_thread() {
        let ring = fixture();
        let mut writer = ring.writer().unwrap();
        for _ in 0..EXIT_CAPACITY {
            writer.record(ExitMetadata::default());
        }
        ring.entries[0]
            .sequence
            .store((EXIT_CAPACITY as u64 + 1) ^ WRITING, Ordering::Relaxed);
        assert!(ring.snapshot().is_none());
    }

    #[test]
    fn option_fields_and_counter_wrap_do_not_publish_stale_data() {
        let ring = fixture();
        let mut writer = ring.writer().unwrap();
        writer.sequence = u64::MAX - 300;
        for reason in 0..1000 {
            writer.record(ExitMetadata {
                reason,
                rip: Some(0),
                address: Some(0),
                size: 1,
                write: Some(false),
                errno: Some(0),
            });
        }
        let entries = ring.snapshot().unwrap();
        assert_eq!(entries[0].exit.reason, 744);
        assert_eq!(entries.last().unwrap().exit.rip, Some(0));
        assert_eq!(entries.last().unwrap().exit.write, Some(false));
        for _ in 0..EXIT_CAPACITY {
            writer.record(ExitMetadata::default());
        }
        assert!(
            ring.snapshot()
                .unwrap()
                .iter()
                .all(|r| r.exit == ExitMetadata::default())
        );
    }

    fn validate(records: &[ExitRecord]) {
        for record in records {
            let exit = record.exit;
            assert_eq!(exit.rip, Some(u64::from(exit.reason) + 0x5000));
            assert_eq!(exit.address, Some(u64::from(exit.reason) * 2));
            assert_eq!(exit.size, exit.reason * 3);
            assert_eq!(exit.write, Some(exit.reason % 2 == 0));
            assert_eq!(exit.errno, Some(exit.reason as i32));
            assert!(record.unix_ns.is_some());
        }
        assert!(
            records
                .windows(2)
                .all(|pair| pair[0].exit.reason + 1 == pair[1].exit.reason)
        );
    }

    #[test]
    fn concurrent_readers_never_mix_fields_from_different_exits() {
        let ring = std::sync::Arc::new(fixture());
        let done = std::sync::Arc::new(AtomicBool::new(false));
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let ring = ring.clone();
                let done = done.clone();
                scope.spawn(move || {
                    while !done.load(Ordering::Acquire) {
                        if let Some(records) = ring.snapshot() {
                            validate(&records);
                        }
                    }
                    validate(&ring.snapshot().unwrap());
                });
            }
            let mut writer = ring.writer().unwrap();
            for reason in 1..=100_000 {
                writer.record(ExitMetadata {
                    reason,
                    rip: Some(u64::from(reason) + 0x5000),
                    address: Some(u64::from(reason) * 2),
                    size: reason * 3,
                    write: Some(reason % 2 == 0),
                    errno: Some(reason as i32),
                });
            }
            done.store(true, Ordering::Release);
        });
        let records = ring.snapshot().unwrap();
        assert_eq!(records.len(), EXIT_CAPACITY);
        assert_eq!(records.last().unwrap().exit.reason, 100_000);
    }

    #[test]
    fn dump_converts_live_timestamps_without_changing_the_raw_counter() {
        let ring = ExitRing::default();
        let before = unix_ns();
        ring.writer().unwrap().record(ExitMetadata::default());
        let record = ring.snapshot().unwrap().pop().unwrap();
        let after = unix_ns();
        let estimated = record.unix_ns.unwrap();
        assert!(estimated >= before.saturating_sub(1_000_000));
        assert!(estimated <= after + 1_000_000);
        assert_eq!(
            record.host_tsc.is_some(),
            matches!(ring.clock, Clock::InvariantTsc { .. })
        );
    }

    #[test]
    #[ignore = "manual release-mode microbenchmark; no VM boot"]
    fn recorder_overhead() {
        use std::hint::black_box;
        const N: u32 = 2_000_000;
        let ring = ExitRing::default();
        let mut writer = ring.writer().unwrap();
        let begin = std::time::Instant::now();
        for reason in 0..N {
            black_box(ExitMetadata {
                reason: black_box(reason),
                ..Default::default()
            });
        }
        let baseline = begin.elapsed();
        let begin = std::time::Instant::now();
        for reason in 0..N {
            writer.record(black_box(ExitMetadata {
                reason,
                address: Some(0x3f8),
                size: 1,
                write: Some(true),
                ..Default::default()
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
