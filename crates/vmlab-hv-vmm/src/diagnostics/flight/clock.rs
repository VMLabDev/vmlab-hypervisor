//! Host exit timestamps: cheap counter reads, wall-clock conversion on dump.

use std::arch::x86_64::{__cpuid, _mm_lfence, _rdtsc};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Clone, Copy, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Clock {
    InvariantTsc { hz: u64 },
    SystemTime,
}

pub(super) struct Anchor {
    pub ticks: u64,
    pub unix_ns: u64,
}

static CLOCK: OnceLock<Clock> = OnceLock::new();

fn linux_uses_tsc() -> bool {
    std::fs::read_to_string("/sys/devices/system/clocksource/clocksource0/current_clocksource")
        .is_ok_and(|name| name.trim() == "tsc")
}

// Serialization is confined to calibration and dump-time anchors.
fn ordered_tsc() -> u64 {
    // SAFETY: RDTSC and SSE2 are available on the supported x86-64 hosts.
    unsafe {
        _mm_lfence();
        let ticks = _rdtsc();
        _mm_lfence();
        ticks
    }
}

fn monotonic_sample() -> (u64, Instant) {
    (0..3)
        .map(|_| {
            let before = ordered_tsc();
            let instant = Instant::now();
            let after = ordered_tsc();
            (after.wrapping_sub(before), before, instant)
        })
        .min_by_key(|sample| sample.0)
        .map(|(span, before, instant)| (before.wrapping_add(span / 2), instant))
        .unwrap()
}

impl Clock {
    pub(super) fn global() -> Self {
        *CLOCK.get_or_init(|| {
            let invariant =
                __cpuid(0x8000_0000).eax >= 0x8000_0007 && __cpuid(0x8000_0007).edx & (1 << 8) != 0;
            if invariant && linux_uses_tsc() {
                let (start_ticks, start) = monotonic_sample();
                std::thread::sleep(Duration::from_millis(20));
                let (end_ticks, end) = monotonic_sample();
                let elapsed = end.duration_since(start).as_nanos();
                if let Some(hz) = end_ticks
                    .checked_sub(start_ticks)
                    .and_then(|ticks| (u128::from(ticks) * 1_000_000_000).checked_div(elapsed))
                    .and_then(|hz| u64::try_from(hz).ok())
                    .filter(|hz| (1_000_000..=1_000_000_000_000).contains(hz))
                {
                    return Clock::InvariantTsc { hz };
                }
            }
            Clock::SystemTime
        })
    }

    #[inline]
    pub(super) fn capture(self) -> u64 {
        match self {
            // SAFETY: selected only for an invariant TSC trusted by Linux.
            // Deliberately unfenced: this is a diagnostic time sample, not an
            // instruction-ordering barrier.
            Self::InvariantTsc { .. } => unsafe { _rdtsc() },
            Self::SystemTime => super::unix_ns(),
        }
    }

    pub(super) fn anchor(self) -> Option<Anchor> {
        match self {
            Self::SystemTime => None,
            Self::InvariantTsc { .. } if !linux_uses_tsc() => None,
            Self::InvariantTsc { .. } => (0..3)
                .map(|_| {
                    let before = ordered_tsc();
                    let unix_ns = super::unix_ns();
                    let after = ordered_tsc();
                    (after.wrapping_sub(before), before, unix_ns)
                })
                .min_by_key(|sample| sample.0)
                .map(|(span, before, unix_ns)| Anchor {
                    ticks: before.wrapping_add(span / 2),
                    unix_ns,
                }),
        }
    }

    pub(super) fn convert(self, ticks: u64, anchor: Option<&Anchor>) -> (Option<u64>, Option<u64>) {
        match self {
            Self::SystemTime => (Some(ticks), None),
            Self::InvariantTsc { hz } => {
                let unix_ns = anchor.and_then(|anchor| {
                    let age = anchor.ticks.checked_sub(ticks)?;
                    let ns = (u128::from(age) * 1_000_000_000).checked_div(u128::from(hz))?;
                    anchor.unix_ns.checked_sub(u64::try_from(ns).ok()?)
                });
                (unix_ns, Some(ticks))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_uses_dump_anchor_and_preserves_unconvertible_raw_ticks() {
        let clock = Clock::InvariantTsc { hz: 2_000_000_000 };
        let anchor = Anchor {
            ticks: 4_000_000_000,
            unix_ns: 10_000_000_000,
        };
        assert_eq!(
            clock.convert(2_000_000_000, Some(&anchor)),
            (Some(9_000_000_000), Some(2_000_000_000))
        );
        assert_eq!(
            clock.convert(anchor.ticks + 1, Some(&anchor)),
            (None, Some(anchor.ticks + 1))
        );
        assert_eq!(clock.convert(1, None), (None, Some(1)));
        assert_eq!(Clock::SystemTime.convert(123, None), (Some(123), None));
    }
}
