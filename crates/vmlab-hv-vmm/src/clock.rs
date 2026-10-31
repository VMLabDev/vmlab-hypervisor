//! CMOS calendar driven by the VM's KVM clock, including snapshot rollback.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use kvm_ioctls::VmFd;
use parking_lot::Mutex;
use vmlab_hv_devices::{BusDevice, DeviceError, IrqLine, Snapshot, chipset::rtc::Rtc};

use crate::error::VmmError;

pub(crate) fn rtc_base(base: Option<&str>) -> Result<i64, VmmError> {
    match base {
        None => Ok(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| VmmError::Other(format!("host clock: {e}")))?
            .as_secs() as i64),
        Some(base) => chrono::DateTime::parse_from_rfc3339(base)
            .map(|date| date.timestamp())
            .map_err(|e| VmmError::InvalidMachine(format!("invalid rtc_base {base:?}: {e}"))),
    }
}

struct State {
    rtc: Rtc,
    last: u64,
}

/// The timer holds a weak reference so dropping the machine stops it.
pub(crate) struct KvmRtc {
    state: Arc<Mutex<State>>,
    vm: Arc<VmFd>,
}

impl KvmRtc {
    pub(crate) fn new(
        ram_bytes: u64,
        base: i64,
        irq: Arc<dyn IrqLine>,
        vm: Arc<VmFd>,
    ) -> Result<Self, VmmError> {
        let last = vm
            .get_clock()
            .map_err(|e| VmmError::ioctl("KVM_GET_CLOCK", e))?
            .clock;
        let state = Arc::new(Mutex::new(State {
            rtc: Rtc::new(ram_bytes, base, irq),
            last,
        }));
        let weak = Arc::downgrade(&state);
        let weak_vm = Arc::downgrade(&vm);
        std::thread::Builder::new()
            .name("rtc".into())
            .spawn(move || {
                loop {
                    {
                        let (Some(state), Some(vm)) = (weak.upgrade(), weak_vm.upgrade()) else {
                            break;
                        };
                        let mut state = state.lock();
                        if let Ok(clock) = vm.get_clock() {
                            advance(&mut state, clock.clock);
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            })
            .map_err(|e| VmmError::Other(format!("spawning RTC timer: {e}")))?;
        Ok(Self { state, vm })
    }

    fn sync(&self, state: &mut State) -> Result<(), DeviceError> {
        let clock = self
            .vm
            .get_clock()
            .map_err(|e| DeviceError::State(format!("RTC KVM_GET_CLOCK: {e}")))?;
        advance(state, clock.clock);
        Ok(())
    }
}

fn advance(state: &mut State, now: u64) {
    let delta = now.saturating_sub(state.last);
    state.last = now;
    state.rtc.advance(delta);
}

impl BusDevice for KvmRtc {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let mut state = self.state.lock();
        if self.sync(&mut state).is_err() {
            data.fill(0xff);
            return;
        }
        state.rtc.read(offset, data);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut state = self.state.lock();
        if self.sync(&mut state).is_ok() {
            state.rtc.write(offset, data);
        }
    }
}

impl vmlab_hv_devices::Resettable for KvmRtc {
    fn reset(&mut self) -> Result<(), DeviceError> {
        let mut state = self.state.lock();
        self.sync(&mut state)?;
        vmlab_hv_devices::Resettable::reset(&mut state.rtc)
    }
}

impl Snapshot for KvmRtc {
    fn state_id(&self) -> String {
        self.state.lock().rtc.state_id()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = self.state.lock();
        self.sync(&mut state)?;
        state.rtc.save_state()
    }

    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        let mut state = self.state.lock();
        state.rtc.restore_state(bytes)?;
        state.last = self
            .vm
            .get_clock()
            .map_err(|e| DeviceError::State(format!("RTC KVM_GET_CLOCK: {e}")))?
            .clock;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utc_and_offsets_and_rejects_invalid_dates() {
        for base in ["2020-01-01T00:00:00Z", "2020-01-01T10:00:00+10:00"] {
            assert_eq!(rtc_base(Some(base)).unwrap(), 1_577_836_800);
        }
        for base in ["garbage", "2020-02-30T00:00:00Z", "2020-01-01"] {
            assert!(
                rtc_base(Some(base))
                    .unwrap_err()
                    .to_string()
                    .contains("rtc_base")
            );
        }
        let now = rtc_base(None).unwrap();
        assert!(
            (now - SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64)
                .abs()
                <= 1
        );
    }
}
