//! Goldfish RTC register protocol, with nanosecond time and a latched high word.

use crate::{BusDevice, DeviceError, IrqLine, Snapshot};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Serialize, Deserialize)]
struct State {
    time: u64,
    high: u32,
    alarm: u64,
    armed: bool,
    enabled: bool,
    pending: bool,
}

pub struct GoldfishRtc {
    state: State,
    irq: Arc<dyn IrqLine>,
}

impl GoldfishRtc {
    pub fn new(nanoseconds: u64, irq: Arc<dyn IrqLine>) -> Self {
        Self {
            state: State {
                time: nanoseconds,
                high: 0,
                alarm: 0,
                armed: false,
                enabled: false,
                pending: false,
            },
            irq,
        }
    }
    pub fn advance(&mut self, nanoseconds: u64) {
        self.state.time = self.state.time.wrapping_add(nanoseconds);
        self.update();
    }
    fn update(&mut self) {
        if self.state.armed && self.state.time >= self.state.alarm {
            self.state.armed = false;
            self.state.pending = true;
        }
        self.irq.set_level(self.state.pending && self.state.enabled);
    }
}

impl BusDevice for GoldfishRtc {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if data.len() != 4 {
            return;
        }
        let value = match offset {
            0 => {
                self.state.high = (self.state.time >> 32) as u32;
                self.state.time as u32
            }
            4 => self.state.high,
            8 => self.state.alarm as u32,
            12 => (self.state.alarm >> 32) as u32,
            16 => u32::from(self.state.enabled),
            24 => u32::from(self.state.armed),
            _ => 0,
        };
        data.copy_from_slice(&value.to_le_bytes());
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        if data.len() != 4 {
            return;
        }
        let value = u32::from_le_bytes(data.try_into().unwrap()) as u64;
        match offset {
            0 => self.state.time = (self.state.time & !0xffff_ffff) | value,
            4 => self.state.time = (self.state.time & 0xffff_ffff) | (value << 32),
            8 => {
                self.state.alarm = (self.state.alarm & !0xffff_ffff) | value;
                self.state.armed = true;
            }
            12 => self.state.alarm = (self.state.alarm & 0xffff_ffff) | (value << 32),
            16 => self.state.enabled = value & 1 != 0,
            20 => {
                self.state.armed = false;
            }
            28 => self.state.pending = false,
            _ => {}
        }
        self.update();
    }
}
impl Snapshot for GoldfishRtc {
    fn state_id(&self) -> String {
        "goldfish-rtc".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        serde_json::to_vec(&self.state).map_err(|e| DeviceError::State(e.to_string()))
    }
    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        self.state =
            serde_json::from_slice(bytes).map_err(|e| DeviceError::State(e.to_string()))?;
        self.update();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Irq(std::sync::atomic::AtomicBool);
    impl IrqLine for Irq {
        fn set_level(&self, high: bool) {
            self.0.store(high, std::sync::atomic::Ordering::SeqCst);
        }
    }
    #[test]
    fn latched_time_alarm_and_snapshot() {
        let irq = Arc::new(Irq::default());
        let mut rtc = GoldfishRtc::new(0xffff_ffff, irq.clone());
        let mut word = [0; 4];
        rtc.read(0, &mut word);
        assert_eq!(u32::from_le_bytes(word), u32::MAX);
        rtc.advance(1);
        rtc.read(4, &mut word);
        assert_eq!(u32::from_le_bytes(word), 0);
        rtc.write(12, &1u32.to_le_bytes());
        rtc.write(8, &1u32.to_le_bytes());
        rtc.write(16, &1u32.to_le_bytes());
        assert!(!irq.0.load(std::sync::atomic::Ordering::SeqCst));
        let state = rtc.save_state().unwrap();
        rtc.advance(1);
        assert!(irq.0.load(std::sync::atomic::Ordering::SeqCst));
        rtc.write(28, &0u32.to_le_bytes());
        assert!(!irq.0.load(std::sync::atomic::Ordering::SeqCst));
        rtc.restore_state(&state).unwrap();
        rtc.advance(1);
        assert!(irq.0.load(std::sync::atomic::Ordering::SeqCst));
    }
}
