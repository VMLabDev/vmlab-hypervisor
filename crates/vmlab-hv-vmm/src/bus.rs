//! Dispatch of guest port-I/O and MMIO accesses to bus devices.

use std::sync::Arc;
use parking_lot::Mutex;

use vmlab_hv_devices::BusDevice;

/// A bus region: `[base, base+len)` routed to one device.
struct Region {
    base: u64,
    len: u64,
    dev: Arc<Mutex<dyn BusDevice>>,
}

/// Port-I/O bus. Unhandled reads fill with `0xFF` and unhandled writes are
/// dropped, matching x86 convention.
#[derive(Default)]
pub struct IoBus {
    regions: Vec<Region>,
}

impl IoBus {
    pub fn new() -> Self {
        IoBus::default()
    }

    /// Register `dev` for port range `[base, base+len)`.
    pub fn register(
        &mut self,
        base: u16,
        len: u16,
        dev: Arc<Mutex<dyn BusDevice>>,
    ) -> Result<(), String> {
        let (base, len) = (base as u64, len as u64);
        self.check_overlap(base, len, "port I/O")?;
        self.regions.push(Region { base, len, dev });
        Ok(())
    }

    fn check_overlap(&self, base: u64, len: u64, what: &str) -> Result<(), String> {
        if self
            .regions
            .iter()
            .any(|r| base < r.base + r.len && r.base < base + len)
        {
            return Err(format!(
                "{what} range [{base:#x}, {:#x}) overlaps an existing device",
                base + len
            ));
        }
        Ok(())
    }

    pub fn read(&self, port: u16, data: &mut [u8]) {
        let port = port as u64;
        let Some(region) = self
            .regions
            .iter()
            .find(|r| r.base <= port && port < r.base + r.len)
        else {
            data.fill(0xFF);
            return;
        };
        let offset = port - region.base;
        let mut dev = region.dev.lock();
        dev.read(offset, data);
    }

    pub fn write(&self, port: u16, data: &[u8]) {
        let port = port as u64;
        let Some(region) = self
            .regions
            .iter()
            .find(|r| r.base <= port && port < r.base + r.len)
        else {
            return;
        };
        let offset = port - region.base;
        let mut dev = region.dev.lock();
        dev.write(offset, data);
    }
}

/// MMIO bus, same dispatch over 64-bit addresses.
#[derive(Default)]
pub struct MmioBus {
    regions: Vec<Region>,
}

impl MmioBus {
    pub fn new() -> Self {
        MmioBus::default()
    }

    /// Register `dev` for address range `[base, base+len)`.
    pub fn register(
        &mut self,
        base: u64,
        len: u64,
        dev: Arc<Mutex<dyn BusDevice>>,
    ) -> Result<(), String> {
        self.check_overlap(base, len)?;
        self.regions.push(Region { base, len, dev });
        Ok(())
    }

    /// Remove the region exactly covering `[base, base+len)`, if present.
    pub fn unregister(&mut self, base: u64, len: u64) {
        self.regions
            .retain(|r| !(r.base == base && r.len == len));
    }

    /// Drop every region pointing at this device address (used when a BAR is
    /// reprogrammed).
    pub fn unregister_dev(&mut self, dev: &Arc<Mutex<dyn BusDevice>>) {
        self.regions.retain(|r| !Arc::ptr_eq(&r.dev, dev));
    }

    fn check_overlap(&self, base: u64, len: u64) -> Result<(), String> {
        if self
            .regions
            .iter()
            .any(|r| base < r.base + r.len && r.base < base + len)
        {
            return Err(format!(
                "MMIO range [{base:#x}, {:#x}) overlaps an existing device",
                base + len
            ));
        }
        Ok(())
    }

    pub fn read(&self, addr: u64, data: &mut [u8]) {
        let Some(region) = self
            .regions
            .iter()
            .find(|r| r.base <= addr && addr < r.base + r.len)
        else {
            data.fill(0xFF);
            return;
        };
        let offset = addr - region.base;
        let mut dev = region.dev.lock();
        dev.read(offset, data);
    }

    pub fn write(&self, addr: u64, data: &[u8]) {
        let Some(region) = self
            .regions
            .iter()
            .find(|r| r.base <= addr && addr < r.base + r.len)
        else {
            return;
        };
        let offset = addr - region.base;
        let mut dev = region.dev.lock();
        dev.write(offset, data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8-byte scratch device recording writes.
    struct Scratch {
        bytes: [u8; 16],
    }

    impl BusDevice for Scratch {
        fn read(&mut self, offset: u64, data: &mut [u8]) {
            for (i, b) in data.iter_mut().enumerate() {
                let idx = (offset as usize + i).min(self.bytes.len() - 1);
                *b = self.bytes[idx];
            }
        }

        fn write(&mut self, offset: u64, data: &[u8]) {
            for (i, b) in data.iter().enumerate() {
                let idx = (offset as usize + i).min(self.bytes.len() - 1);
                self.bytes[idx] = *b;
            }
        }
    }

    fn scratch() -> Arc<Mutex<Scratch>> {
        Arc::new(Mutex::new(Scratch { bytes: [0; 16] }))
    }

    #[test]
    fn io_dispatch_and_unhandled() {
        let mut bus = IoBus::new();
        let dev: Arc<Mutex<dyn BusDevice>> = scratch();
        bus.register(0x3F8, 8, dev.clone()).unwrap();
        bus.write(0x3F9, &[1, 2, 3]);
        let mut out = [0u8; 3];
        bus.read(0x3F8, &mut out);
        assert_eq!(out, [0, 1, 2]);
        // Outside the device range (even within the exit) reads 0xFF.
        let mut far = [0u8; 2];
        bus.read(0x400, &mut far);
        assert_eq!(far, [0xFF, 0xFF]);
    }

    #[test]
    fn io_overlap_rejected() {
        let mut bus = IoBus::new();
        bus.register(0x100, 8, scratch()).unwrap();
        assert!(bus.register(0x105, 4, scratch()).is_err());
        assert!(bus.register(0x108, 4, scratch()).is_ok());
        // Wrapping a device is also an overlap.
        assert!(bus.register(0xF8, 0x20, scratch()).is_err());
    }

    #[test]
    fn mmio_unregister_dev() {
        let mut bus = MmioBus::new();
        let dev: Arc<Mutex<dyn BusDevice>> = scratch();
        bus.register(0x1000, 0x100, dev.clone()).unwrap();
        bus.unregister_dev(&dev);
        // Re-registering the same range must now succeed.
        bus.register(0x1000, 0x100, dev).unwrap();
    }

    #[test]
    fn mmio_unhandled_read_fills_ff() {
        let bus = MmioBus::new();
        let mut out = [0u8; 4];
        bus.read(0xDEAD_BEEF, &mut out);
        assert_eq!(out, [0xFF; 4]);
        // Writes to nowhere are dropped, not errors.
        bus.write(0xDEAD_BEEF, &[1, 2, 3]);
    }
}
