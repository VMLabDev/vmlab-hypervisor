//! The virtio-pci (modern) transport and the virtio devices.
//!
//! [`pci`] wraps any [`VirtioDevice`](crate::VirtioDevice) in a VIRTIO 1.2
//! §4.1 virtio-pci transport: one 64-bit memory BAR carries the common
//! configuration, notification, ISR status, device-specific configuration and
//! MSI-X structures, and legacy INTx is available for guests that prefer it.
//! [`block`] is virtio-blk over the disk-format seam, and [`fs`] is the
//! vhost-user-fs front-end whose queues an external virtiofsd serves.

pub mod block;
pub mod console;
pub mod fs;
pub mod net;
pub mod pci;

/// Test doubles shared by the virtio device and transport tests: an
/// in-memory [`vmlab_disk::DiskImage`] and interrupt sinks that record what
/// they delivered.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;

    use parking_lot::Mutex;
    use vmlab_disk::{DiskImage, Error, Result, SnapshotInfo};

    /// An in-memory disk image with a deterministic byte pattern. Content
    /// and discard log are shared by handle so tests can observe them after
    /// the disk is boxed into a device.
    pub(crate) struct MemDisk {
        data: Arc<Mutex<Vec<u8>>>,
        read_only: bool,
        discards: Arc<Mutex<Vec<(u64, u64)>>>,
    }

    impl MemDisk {
        /// Writable disk of `len` bytes; byte i is `(i % 251) as u8`.
        pub fn new(len: usize) -> Self {
            Self {
                data: Arc::new(Mutex::new((0..len).map(|i| (i % 251) as u8).collect())),
                read_only: false,
                discards: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Read-only disk of `len` bytes with the same pattern.
        pub fn new_read_only(len: usize) -> Self {
            let mut d = Self::new(len);
            d.read_only = true;
            d
        }

        /// Shared view of the disk content.
        pub fn content(&self) -> Arc<Mutex<Vec<u8>>> {
            Arc::clone(&self.data)
        }

        /// Shared log of discard hints.
        pub fn discard_log(&self) -> Arc<Mutex<Vec<(u64, u64)>>> {
            Arc::clone(&self.discards)
        }
    }

    impl DiskImage for MemDisk {
        fn size(&self) -> u64 {
            self.data.lock().len() as u64
        }

        fn read_only(&self) -> bool {
            self.read_only
        }

        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let data = self.data.lock();
            let end = offset
                .checked_add(buf.len() as u64)
                .ok_or_else(|| Error::Unsupported("offset overflow".into()))?;
            if end > data.len() as u64 {
                return Err(Error::Unsupported("read past end".into()));
            }
            buf.copy_from_slice(&data[offset as usize..end as usize]);
            Ok(())
        }

        fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
            if self.read_only {
                return Err(Error::ReadOnly);
            }
            let mut data = self.data.lock();
            let end = offset
                .checked_add(buf.len() as u64)
                .ok_or_else(|| Error::Unsupported("offset overflow".into()))?;
            if end > data.len() as u64 {
                return Err(Error::Unsupported("write past end".into()));
            }
            data[offset as usize..end as usize].copy_from_slice(buf);
            Ok(())
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }

        fn discard(&mut self, offset: u64, len: u64) -> Result<()> {
            if self.read_only {
                return Err(Error::ReadOnly);
            }
            self.discards.lock().push((offset, len));
            Ok(())
        }

        fn snapshots(&self) -> Result<Vec<SnapshotInfo>> {
            Ok(Vec::new())
        }

        fn snapshot_create(
            &mut self,
            _name: &str,
            _vm_state_size: u64,
            _vm_clock_nsec: u64,
        ) -> Result<()> {
            Err(Error::Unsupported("memdisk snapshots".into()))
        }

        fn snapshot_apply(&mut self, _name: &str) -> Result<SnapshotInfo> {
            Err(Error::Unsupported("memdisk snapshots".into()))
        }

        fn snapshot_delete(&mut self, _name: &str) -> Result<()> {
            Err(Error::Unsupported("memdisk snapshots".into()))
        }

        fn write_vmstate(&mut self, _offset: u64, _buf: &[u8]) -> Result<()> {
            Err(Error::Unsupported("memdisk vmstate".into()))
        }

        fn read_vmstate(&mut self, _offset: u64, _buf: &mut [u8]) -> Result<()> {
            Err(Error::Unsupported("memdisk vmstate".into()))
        }
    }

    /// Records delivered MSI/MSI-X messages.
    #[derive(Default)]
    pub(crate) struct RecMsi {
        pub messages: Mutex<Vec<(u64, u32)>>,
    }

    impl crate::MsiSink for RecMsi {
        fn signal(&self, msg: crate::MsiMessage) {
            self.messages.lock().push((msg.address, msg.data));
        }
    }

    /// Records the level of a legacy interrupt line.
    #[derive(Default)]
    pub(crate) struct RecIrq {
        pub level: Mutex<bool>,
    }

    impl crate::IrqLine for RecIrq {
        fn set_level(&self, high: bool) {
            *self.level.lock() = high;
        }
    }
}
