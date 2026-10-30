//! A lock-free single-producer single-consumer byte ring.
//!
//! The producer is the device (vCPU) path; the consumer is the backend's
//! audio thread. Neither side ever blocks: [`SpscRing::push_drop_oldest`]
//! frees space by dropping the oldest PCM when the ring is full, so a
//! burst from the guest degrades to a bounded-latency glitch instead of a
//! stall. Positions are byte counters that wrap on subtraction; indices
//! are masked into the power-of-two buffer.
//!
//! Ownership of [`SpscRing::head`] is shared through compare-and-swap: the
//! consumer advances it when popping, the producer advances it when
//! dropping overrun data. Both moves are forward-only and re-validated by
//! the CAS loop, so concurrent advances cannot pass `tail`. `tail` is
//! written by the producer alone, after the payload bytes are in place.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// An atomically accessed index, kept on its own cache line so the
/// producer and consumer do not fight over one.
#[repr(align(64))]
struct Padded(AtomicUsize);

/// One SPSC ring. Both sides hold `&SpscRing`.
pub(crate) struct SpscRing {
    buf: Box<[AtomicU8]>,
    period_end: Box<[AtomicUsize]>,
    mask: usize,
    head: Padded,
    tail: Padded,
}

// Atomic bytes permit copying during overwrite. A copy is returned only if
// the head CAS succeeds; otherwise the consumer retries from the new head.

impl SpscRing {
    /// A ring holding at least `min_bytes`, rounded up to a power of two.
    pub fn with_capacity_bytes(min_bytes: usize) -> Self {
        let cap = min_bytes.max(1).next_power_of_two();
        Self {
            buf: (0..cap).map(|_| AtomicU8::new(0)).collect(),
            period_end: (0..cap).map(|_| AtomicUsize::new(0)).collect(),
            mask: cap - 1,
            head: Padded(AtomicUsize::new(0)),
            tail: Padded(AtomicUsize::new(0)),
        }
    }

    fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Bytes written but not yet consumed.
    pub fn used_bytes(&self) -> usize {
        let head = self.head.0.load(Ordering::Acquire);
        let tail = self.tail.0.load(Ordering::Acquire);
        // Counters share an origin and the ring is drained long before
        // either wraps usize, so plain subtraction is exact.
        tail.wrapping_sub(head).min(self.capacity())
    }

    /// Push `data` as the newest period, dropping the oldest buffered periods
    /// first when there is not enough room. A period larger than the whole
    /// ring is dropped: the ring is sized for whole periods.
    pub fn push_drop_oldest(&self, data: &[u8]) {
        if data.len() > self.capacity() {
            return;
        }
        let mut tail = self.tail.0.load(Ordering::Relaxed);
        loop {
            let head = self.head.0.load(Ordering::Acquire);
            let free = self.capacity() - (tail.wrapping_sub(head));
            if data.len() <= free {
                let start = tail & self.mask;
                for (i, byte) in data.iter().enumerate() {
                    self.buf[(start + i) & self.mask].store(*byte, Ordering::SeqCst);
                    self.period_end[(start + i) & self.mask]
                        .store(tail.wrapping_add(data.len()), Ordering::SeqCst);
                }
                self.tail
                    .0
                    .store(tail.wrapping_add(data.len()), Ordering::SeqCst);
                return;
            }
            // Each live byte records its period's end, so dropping never
            // leaves a partial old period behind. The CAS
            // fails when the consumer retired them first; retry with the
            // fresh head.
            let next = self.period_end[head & self.mask].load(Ordering::SeqCst);
            if self
                .head
                .0
                .compare_exchange_weak(head, next, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                tail = self.tail.0.load(Ordering::Relaxed);
            }
        }
    }

    /// Pop up to `out.len()` bytes, returning how many were moved.
    pub fn pop_max(&self, out: &mut [u8]) -> usize {
        if out.is_empty() {
            return 0;
        }
        loop {
            let head = self.head.0.load(Ordering::Relaxed);
            let tail = self.tail.0.load(Ordering::Acquire);
            let n = out.len().min(tail.wrapping_sub(head));
            if n == 0 {
                return 0;
            }
            for (i, byte) in out[..n].iter_mut().enumerate() {
                *byte = self.buf[head.wrapping_add(i) & self.mask].load(Ordering::SeqCst);
            }
            if self
                .head
                .0
                .compare_exchange_weak(
                    head,
                    head.wrapping_add(n),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_err()
            {
                continue; // the producer dropped overrun data under us
            }
            return n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_pop_round_trips_bytes() {
        let ring = SpscRing::with_capacity_bytes(64);
        ring.push_drop_oldest(b"hello");
        ring.push_drop_oldest(b" world");
        assert_eq!(ring.used_bytes(), 11);
        let mut out = [0u8; 11];
        assert_eq!(ring.pop_max(&mut out), 11);
        assert_eq!(&out, b"hello world");
        assert_eq!(ring.used_bytes(), 0);
    }

    #[test]
    fn pop_returns_short_when_less_is_buffered() {
        let ring = SpscRing::with_capacity_bytes(64);
        ring.push_drop_oldest(b"abc");
        let mut out = [0u8; 8];
        assert_eq!(ring.pop_max(&mut out), 3);
        assert_eq!(&out[..3], b"abc");
    }

    #[test]
    fn overrun_drops_the_oldest_bytes() {
        let ring = SpscRing::with_capacity_bytes(64);
        ring.push_drop_oldest(&[1u8; 48]);
        ring.push_drop_oldest(&[2u8; 48]);
        assert_eq!(ring.used_bytes(), 48);
        let mut out = [0u8; 64];
        assert_eq!(ring.pop_max(&mut out), 48);
        assert_eq!(&out[..48], &[2u8; 48]);
    }

    #[test]
    fn wrapping_writes_are_contiguous_to_the_consumer() {
        let ring = SpscRing::with_capacity_bytes(16);
        ring.push_drop_oldest(b"0123456789");
        let mut out = [0u8; 6];
        ring.pop_max(&mut out); // consume "012345", tail near the end
        ring.push_drop_oldest(b"abcdefghij"); // wraps the buffer end
        let mut all = Vec::new();
        let mut chunk = [0u8; 20];
        loop {
            let n = ring.pop_max(&mut chunk);
            if n == 0 {
                break;
            }
            all.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(all, b"6789abcdefghij");
    }

    #[test]
    fn a_period_larger_than_the_ring_is_dropped() {
        let ring = SpscRing::with_capacity_bytes(64);
        ring.push_drop_oldest(&[7u8; 65]);
        assert_eq!(ring.used_bytes(), 0);
    }

    #[test]
    fn concurrent_producer_and_consumer_keep_periods_intact() {
        // No atomics-ordering proof, but a burst churn catches torn logic:
        // every popped byte must come from the period the producer wrote.
        let ring = std::sync::Arc::new(SpscRing::with_capacity_bytes(1024));
        let producer = std::thread::spawn({
            let ring = ring.clone();
            move || {
                for i in 0..10_000u32 {
                    ring.push_drop_oldest(&i.to_le_bytes());
                }
            }
        });
        let mut got = Vec::new();
        let mut chunk = [0u8; 4];
        while !producer.is_finished() || ring.used_bytes() != 0 {
            let n = ring.pop_max(&mut chunk);
            if n == 0 {
                std::thread::yield_now();
                continue;
            }
            got.extend_from_slice(&chunk[..n]);
        }
        producer.join().unwrap();
        // The stream may have dropped overrun periods, but the bytes that
        // survived are whole periods in order.
        let mut last = None;
        for w in got.as_chunks::<4>().0 {
            let i = u32::from_le_bytes(*w);
            assert!(last.is_none_or(|previous| i > previous));
            last = Some(i);
        }
        assert_eq!(last, Some(9999));
    }
}
