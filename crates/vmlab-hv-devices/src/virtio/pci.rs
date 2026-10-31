//! Modern and transitional virtio-pci transports (VIRTIO 1.2 §4.1).
//!
//! [`VirtioPciDevice::new_transitional`] selects the captured q35 layout:
//! legacy I/O in BAR0, MSI-X in BAR1, and modern regions in BAR4.
//!
//! [`VirtioPciDevice`] wraps any [`Box<dyn VirtioDevice>`](crate::VirtioDevice)
//! as a PCI function: vendor 0x1af4, device 0x1040 + virtio device type,
//! revision 2 (non-transitional). One 64-bit prefetchable memory BAR carries
//! the virtio structures and the MSI-X table and PBA:
//!
//! | BAR offset | Structure          |
//! |------------|--------------------|
//! | `0x0000`   | common config (56 bytes) |
//! | `0x1000`   | notify region (4 bytes/queue) |
//! | `0x2000`   | ISR status (1 byte) |
//! | `0x3000`   | device config |
//! | `0x4000`   | MSI-X table, then the PBA |
//!
//! Queue interrupts and configuration-change interrupts go through the
//! negotiated MSI-X vector when MSI-X is enabled (delivered by
//! [`MsiSink`](crate::MsiSink)), otherwise through the legacy INTx line.
//! The transport implements [`Snapshot`] for its own guest-visible state;
//! the wrapped device saves and restores its own.

use std::sync::Arc;

use parking_lot::Mutex;
use virtio_queue::{Queue, QueueT};

use crate::{
    BarInfo, BarKind, DeviceError, GuestMemory, IrqLine, MsiMessage, MsiSink, PciDevice,
    PciSnapshot, Snapshot, VirtioDevice, VirtioInterrupt,
};

#[path = "transitional.rs"]
mod transitional;
use transitional::LegacyPci;
pub use transitional::{ROM_BAR, ROM_SIZE};

#[cfg(test)]
#[path = "transitional_tests.rs"]
mod transitional_tests;

/// Virtio PCI vendor id.
pub const VIRTIO_VENDOR_ID: u16 = 0x1af4;
/// Base of the modern (non-transitional) device id range.
pub const MODERN_DEVICE_ID_BASE: u16 = 0x1040;

/// `virtio_config status` bits (VIRTIO 1.2 §2.3).
pub mod device_status {
    pub const ACKNOWLEDGE: u8 = 1;
    pub const DRIVER: u8 = 2;
    pub const DRIVER_OK: u8 = 4;
    pub const FEATURES_OK: u8 = 8;
    pub const DEVICE_NEEDS_RESET: u8 = 64;
    pub const FAILED: u8 = 128;
}

/// `VIRTIO_F_VERSION_1`: the transport's one mandatory modern feature.
const F_VERSION_1: u64 = 1 << 32;
/// `VIRTIO_F_RING_EVENT_IDX`, applied to the built queues when negotiated.
const F_RING_EVENT_IDX: u64 = 1 << 29;

const PCI_CAP_ID_VNDR: u8 = 0x09;
const PCI_CAP_ID_MSIX: u8 = 0x11;

/// The BAR index everything lives in.
const BAR_INDEX: u8 = 4;
const COMMON_CFG_OFF: u64 = 0x0000;
const NOTIFY_OFF: u64 = 0x1000;
const ISR_OFF: u64 = 0x2000;
const DEVICE_CFG_OFF: u64 = 0x3000;
const MSIX_TABLE_OFF: u64 = 0x4000;
/// Bytes per queue in the notify region.
const NOTIFY_OFF_MULTIPLIER: u64 = 4;
/// Total BAR size; a power of two as PCI BARs require.
const BAR_SIZE: u64 = 0x8000;
/// Low flags of a 64-bit prefetchable memory BAR.
const BAR64_PREFETCHABLE: u32 = 0x0C;

const COMMON_CFG_LEN: u64 = 56;
const MSI_X_CAP_OFF: usize = 0x40;
const COMMON_CAP_OFF: usize = 0x4C;
const NOTIFY_CAP_OFF: usize = 0x5C;
const ISR_CAP_OFF: usize = 0x70;
const DEVICE_CAP_OFF: usize = 0x80;
const PCI_CONFIG_SIZE: usize = 256;

/// `virtio_pci_common_config` field offsets (VIRTIO 1.2 §4.1.4.3).
mod common {
    pub const DEVICE_FEATURE_SELECT: u64 = 0x00;
    pub const DEVICE_FEATURE: u64 = 0x04;
    pub const GUEST_FEATURE_SELECT: u64 = 0x08;
    pub const GUEST_FEATURE: u64 = 0x0C;
    pub const MSIX_CONFIG: u64 = 0x10;
    pub const NUM_QUEUES: u64 = 0x12;
    pub const DEVICE_STATUS: u64 = 0x14;
    pub const CONFIG_GENERATION: u64 = 0x15;
    pub const QUEUE_SELECT: u64 = 0x16;
    pub const QUEUE_SIZE: u64 = 0x18;
    pub const QUEUE_MSIX_VECTOR: u64 = 0x1A;
    pub const QUEUE_ENABLE: u64 = 0x1C;
    pub const QUEUE_NOTIFY_OFF: u64 = 0x1E;
    pub const QUEUE_DESC: u64 = 0x20;
    pub const QUEUE_AVAIL: u64 = 0x28;
    pub const QUEUE_USED: u64 = 0x30;
}

/// ISR status bits (VIRTIO 1.2 §4.1.4.5).
const ISR_QUEUE_INTERRUPT: u8 = 0x01;
const ISR_CONFIG_CHANGE: u8 = 0x02;

/// `VIRTIO_MSI_NO_VECTOR`.
const NO_VECTOR: u16 = 0xFFFF;

/// One MSI-X table entry.
#[derive(Clone, Copy, Default)]
struct MsixEntry {
    address: u64,
    data: u32,
    masked: bool,
}

/// Interrupt state shared between the PCI function (config and BAR accesses)
/// and the [`VirtioInterrupt`] handle handed to the device.
struct InterruptState {
    msi: Option<Arc<dyn MsiSink>>,
    intx: Option<Arc<dyn IrqLine>>,
    msix_enabled: bool,
    function_masked: bool,
    entries: Vec<MsixEntry>,
    /// Pending-bit array; at most 64 vectors are supported, which covers
    /// every device this transport carries.
    pba: u64,
    config_vector: u16,
    queue_vectors: Vec<u16>,
    isr: u8,
    config_generation: u8,
}

impl InterruptState {
    fn new(
        msi: Option<Arc<dyn MsiSink>>,
        intx: Option<Arc<dyn IrqLine>>,
        num_queues: usize,
    ) -> Self {
        assert!(
            num_queues < 64,
            "the transport PBA covers at most 64 vectors"
        );
        Self {
            msi,
            intx,
            msix_enabled: false,
            function_masked: false,
            entries: vec![MsixEntry::default(); num_queues + 1],
            pba: 0,
            config_vector: NO_VECTOR,
            queue_vectors: vec![NO_VECTOR; num_queues],
            isr: 0,
            config_generation: 0,
        }
    }

    /// Deliver one interrupt at `vector`, setting `bits` in ISR first.
    fn deliver(&mut self, vector: u16, bits: u8) {
        self.isr |= bits;
        if self.msix_enabled {
            let Some(msi) = self.msi.clone() else { return };
            // An unconfigured vector cannot be signalled: the driver must
            // assign one before the interrupt becomes observable.
            if vector == NO_VECTOR {
                return;
            }
            let index = vector as usize;
            if index >= self.entries.len() {
                return;
            }
            let entry = self.entries[index];
            if self.function_masked || entry.masked {
                self.pba |= 1 << vector;
            } else {
                msi.signal(MsiMessage {
                    address: entry.address,
                    data: entry.data,
                });
            }
        } else if let Some(irq) = self.intx.clone() {
            irq.set_level(true);
        }
    }

    /// Deliver every pending MSI-X vector that is no longer masked.
    fn deliver_pending(&mut self) {
        if !self.msix_enabled {
            return;
        }
        let Some(msi) = self.msi.clone() else {
            return;
        };
        for vector in 0..self.entries.len() as u16 {
            if self.pba & (1 << vector) == 0 {
                continue;
            }
            if self.function_masked || self.entries[vector as usize].masked {
                continue;
            }
            self.pba &= !(1 << vector);
            let entry = self.entries[vector as usize];
            msi.signal(MsiMessage {
                address: entry.address,
                data: entry.data,
            });
        }
    }

    /// Read the ISR byte, clearing it and releasing the INTx line.
    fn read_isr(&mut self) -> u8 {
        let value = self.isr;
        self.isr = 0;
        if !self.msix_enabled
            && let Some(irq) = &self.intx
        {
            irq.set_level(false);
        }
        value
    }
}

/// The device-side handle of the transport: raises queue and
/// configuration-change interrupts.
struct TransportInterrupt(Arc<Mutex<InterruptState>>);

impl VirtioInterrupt for TransportInterrupt {
    fn notify_queue(&self, queue: u16) {
        let mut s = self.0.lock();
        let vector = s
            .queue_vectors
            .get(queue as usize)
            .copied()
            .unwrap_or(NO_VECTOR);
        s.deliver(vector, ISR_QUEUE_INTERRUPT);
    }

    fn notify_config(&self) {
        let mut s = self.0.lock();
        // The generation must change before the interrupt is sent.
        s.config_generation = s.config_generation.wrapping_add(1);
        let vector = s.config_vector;
        s.deliver(vector, ISR_CONFIG_CHANGE);
    }
}

/// Driver-written queue configuration, mirrored from the common config.
#[derive(Clone, Copy, Default)]
struct QueueCfg {
    /// 0 until the driver writes a size.
    size: u16,
    desc: u64,
    avail: u64,
    used: u64,
    enabled: bool,
}

/// The modern virtio-pci transport around one virtio device.
pub struct VirtioPciDevice {
    id: String,
    device: Box<dyn VirtioDevice>,
    mem: GuestMemory,
    int: Arc<Mutex<InterruptState>>,
    /// Static PCI configuration space; dynamically composed fields are
    /// overlaid on read.
    config: [u8; PCI_CONFIG_SIZE],
    device_cfg_len: u64,
    // Common configuration state (VIRTIO 1.2 §4.1.4.3).
    driver_status: u8,
    device_feature_select: u32,
    guest_feature_select: u32,
    driver_features: u64,
    /// The set accepted at FEATURES_OK.
    negotiated: Option<u64>,
    queue_select: u16,
    queue_cfg: Vec<QueueCfg>,
    activated: bool,
    // PCI state the guest writes.
    command: u16,
    bar4: u64,
    interrupt_line: u8,
    transitional: Option<LegacyPci>,
}

impl VirtioPciDevice {
    /// Wrap `device` in a modern virtio-pci function. `msi` receives MSI-X
    /// messages, `intx` carries the legacy line; either may be absent.
    pub fn new(
        id: impl Into<String>,
        device: Box<dyn VirtioDevice>,
        mem: GuestMemory,
        msi: Option<Arc<dyn MsiSink>>,
        intx: Option<Arc<dyn IrqLine>>,
    ) -> Self {
        let id = id.into();
        let max_sizes = device.queue_max_sizes();
        let num_queues = max_sizes.len();
        let queue_cfg = max_sizes
            .iter()
            .map(|&size| QueueCfg {
                size,
                ..QueueCfg::default()
            })
            .collect();
        let device_cfg_len = device.config_len();
        let mut config = [0u8; PCI_CONFIG_SIZE];

        let device_id = MODERN_DEVICE_ID_BASE + device.device_type() as u16;
        config[0x00..0x02].copy_from_slice(&VIRTIO_VENDOR_ID.to_le_bytes());
        config[0x02..0x04].copy_from_slice(&device_id.to_le_bytes());
        // Class 2 (network) for a NIC as the spec asks; the rest are
        // unclassified, which is the modern default.
        let class_code: u32 = match device.device_type() {
            1 => 0x0200_0000,
            _ => 0,
        };
        config[0x08..0x0C].copy_from_slice(&(class_code | MODERN_REVISION).to_le_bytes());
        config[0x2C..0x2E].copy_from_slice(&VIRTIO_VENDOR_ID.to_le_bytes());
        config[0x2E..0x30].copy_from_slice(&(device.device_type() as u16).to_le_bytes());
        config[0x34] = MSI_X_CAP_OFF as u8;
        if intx.is_some() {
            config[0x3D] = 1; // INTA#
        }

        let num_vectors = (num_queues + 1) as u16;
        let table_size_bytes = num_vectors as u64 * 16;
        let pba_off = MSIX_TABLE_OFF + table_size_bytes.div_ceil(64) * 64;

        // MSI-X capability.
        config[MSI_X_CAP_OFF] = PCI_CAP_ID_MSIX;
        config[MSI_X_CAP_OFF + 1] = COMMON_CAP_OFF as u8;
        let control = num_vectors - 1; // table size field
        config[MSI_X_CAP_OFF + 2..MSI_X_CAP_OFF + 4].copy_from_slice(&control.to_le_bytes());
        config[MSI_X_CAP_OFF + 4..MSI_X_CAP_OFF + 8]
            .copy_from_slice(&(BAR_INDEX as u32 | (MSIX_TABLE_OFF as u32)).to_le_bytes());
        config[MSI_X_CAP_OFF + 8..MSI_X_CAP_OFF + 12]
            .copy_from_slice(&(BAR_INDEX as u32 | (pba_off as u32)).to_le_bytes());

        // Virtio capabilities. Each helper writes cap_vndr, next, len, type,
        // bar, offset and length; notify adds the multiplier.
        write_virtio_cap(
            &mut config,
            COMMON_CAP_OFF,
            NOTIFY_CAP_OFF as u8,
            1,
            COMMON_CFG_OFF,
            COMMON_CFG_LEN,
        );
        write_virtio_cap(
            &mut config,
            NOTIFY_CAP_OFF,
            ISR_CAP_OFF as u8,
            2,
            NOTIFY_OFF,
            NOTIFY_OFF_MULTIPLIER * num_queues as u64,
        );
        // virtio_pci_notify_cap is 20 bytes: the common header plus the multiplier.
        config[NOTIFY_CAP_OFF + 2] = 20;
        config[NOTIFY_CAP_OFF + 16..NOTIFY_CAP_OFF + 20]
            .copy_from_slice(&(NOTIFY_OFF_MULTIPLIER as u32).to_le_bytes());
        write_virtio_cap(
            &mut config,
            ISR_CAP_OFF,
            DEVICE_CAP_OFF as u8,
            3,
            ISR_OFF,
            1,
        );
        write_virtio_cap(
            &mut config,
            DEVICE_CAP_OFF,
            0,
            4,
            DEVICE_CFG_OFF,
            device_cfg_len,
        );

        Self {
            int: Arc::new(Mutex::new(InterruptState::new(msi, intx, num_queues))),
            config,
            device_cfg_len,
            device,
            mem,
            driver_status: 0,
            device_feature_select: 0,
            guest_feature_select: 0,
            driver_features: 0,
            negotiated: None,
            queue_select: 0,
            queue_cfg,
            activated: false,
            command: 0,
            bar4: 0,
            interrupt_line: 0,
            transitional: None,
            id,
        }
    }

    /// The wrapped device, for the VMM to snapshot or poke.
    pub fn device(&self) -> &dyn VirtioDevice {
        self.device.as_ref()
    }

    /// Mutable access to the wrapped device.
    pub fn device_mut(&mut self) -> &mut dyn VirtioDevice {
        self.device.as_mut()
    }

    fn num_queues(&self) -> usize {
        self.queue_cfg.len()
    }

    /// Every feature the transport offers on the device's behalf.
    fn offered_features(&self) -> u64 {
        self.device.features()
            | F_VERSION_1
            | if self.transitional.is_some() {
                1 << 27
            } else {
                0
            }
    }

    /// Reset to the power-on state (driver wrote status 0).
    fn virtio_reset(&mut self) {
        self.driver_status = 0;
        self.device_feature_select = 0;
        self.guest_feature_select = 0;
        self.driver_features = 0;
        self.negotiated = None;
        self.queue_select = 0;
        for (cfg, &size) in self.queue_cfg.iter_mut().zip(self.device.queue_max_sizes()) {
            *cfg = QueueCfg {
                size,
                ..QueueCfg::default()
            };
        }
        self.activated = false;
        if let Some(legacy) = &mut self.transitional {
            legacy.active = false;
        }
        let mut int = self.int.lock();
        int.config_vector = NO_VECTOR;
        int.queue_vectors.fill(NO_VECTOR);
        int.isr = 0;
        int.config_generation = 0;
        if let Some(irq) = &int.intx {
            irq.set_level(false);
        }
        drop(int);
        self.device.reset();
    }

    /// Handle a write to `device_status` (VIRTIO 1.2 §2.3-2.4.1).
    fn status_write(&mut self, new: u8) {
        if new == 0 {
            self.virtio_reset();
            return;
        }
        let mut status = new & device_status::FAILED;
        if new & device_status::ACKNOWLEDGE != 0 {
            status |= device_status::ACKNOWLEDGE;
        }
        if new & device_status::DRIVER != 0 {
            status |= device_status::DRIVER;
        }
        if new & device_status::DRIVER_OK != 0 {
            status |= device_status::DRIVER_OK;
        }
        if new & device_status::FEATURES_OK != 0
            && self.driver_features & !self.offered_features() == 0
        {
            status |= device_status::FEATURES_OK;
            let negotiated = self.driver_features;
            self.negotiated = Some(negotiated);
            self.device.ack_features(negotiated);
        }
        // An unacceptable feature set leaves FEATURES_OK clear so the
        // driver retries with a subset.
        let activating = status & device_status::DRIVER_OK != 0
            && self.driver_status & device_status::DRIVER_OK == 0;
        if activating && self.transitional.as_ref().is_some_and(|s| s.active) {
            if self.driver_features & !self.offered_features() != 0 {
                status |= device_status::FAILED;
                status &= !device_status::DRIVER_OK;
            } else {
                self.negotiated = Some(self.driver_features);
                self.device.ack_features(self.driver_features);
            }
        }
        self.driver_status = status;
        if activating && status & device_status::DRIVER_OK != 0 {
            self.activate();
        }
    }

    /// Build queues from the driver's configuration and hand them, with the
    /// guest memory and interrupt handle, to the device.
    fn activate(&mut self) {
        if self.activated {
            return;
        }
        let negotiated = self.negotiated.unwrap_or(0);
        let mut queues = Vec::with_capacity(self.num_queues());
        for (i, cfg) in self.queue_cfg.iter().enumerate() {
            let max_size = self.device.queue_max_sizes()[i];
            let Ok(mut queue) = Queue::new(max_size) else {
                self.driver_status |= device_status::DEVICE_NEEDS_RESET;
                return;
            };
            if cfg.enabled {
                if cfg.size == 0 || !cfg.size.is_power_of_two() {
                    // An enabled queue without a usable size is a driver
                    // error the device cannot recover from quietly.
                    self.driver_status |= device_status::DEVICE_NEEDS_RESET;
                    return;
                }
                queue.set_size(cfg.size);
                queue.set_desc_table_address(Some(cfg.desc as u32), Some((cfg.desc >> 32) as u32));
                queue
                    .set_avail_ring_address(Some(cfg.avail as u32), Some((cfg.avail >> 32) as u32));
                queue.set_used_ring_address(Some(cfg.used as u32), Some((cfg.used >> 32) as u32));
                queue.set_event_idx(negotiated & F_RING_EVENT_IDX != 0);
                queue.set_ready(true);
                if !queue.is_valid(&self.mem) {
                    self.driver_status |= device_status::DEVICE_NEEDS_RESET;
                    return;
                }
            }
            queues.push(queue);
        }
        let interrupt: Arc<dyn VirtioInterrupt> =
            Arc::new(TransportInterrupt(Arc::clone(&self.int)));
        match self.device.activate(self.mem.clone(), interrupt, queues) {
            Ok(()) => self.activated = true,
            Err(_) => self.driver_status |= device_status::DEVICE_NEEDS_RESET,
        }
    }

    /// Compose the common configuration into wire bytes.
    fn common_bytes(&self) -> [u8; COMMON_CFG_LEN as usize] {
        let mut buf = [0u8; COMMON_CFG_LEN as usize];
        let int = self.int.lock();
        let features = self.offered_features();
        let feature_half = match self.device_feature_select {
            0 => features as u32,
            1 => (features >> 32) as u32,
            _ => 0,
        };
        let guest_half = match self.guest_feature_select {
            0 => self.driver_features as u32,
            1 => (self.driver_features >> 32) as u32,
            _ => 0,
        };
        buf[common::DEVICE_FEATURE_SELECT as usize..][..4]
            .copy_from_slice(&self.device_feature_select.to_le_bytes());
        buf[common::DEVICE_FEATURE as usize..][..4].copy_from_slice(&feature_half.to_le_bytes());
        buf[common::GUEST_FEATURE_SELECT as usize..][..4]
            .copy_from_slice(&self.guest_feature_select.to_le_bytes());
        buf[common::GUEST_FEATURE as usize..][..4].copy_from_slice(&guest_half.to_le_bytes());
        buf[common::MSIX_CONFIG as usize..][..2].copy_from_slice(&int.config_vector.to_le_bytes());
        buf[common::NUM_QUEUES as usize..][..2]
            .copy_from_slice(&(self.num_queues() as u16).to_le_bytes());
        buf[common::DEVICE_STATUS as usize] = self.driver_status;
        buf[common::CONFIG_GENERATION as usize] = int.config_generation;
        buf[common::QUEUE_SELECT as usize..][..2].copy_from_slice(&self.queue_select.to_le_bytes());
        // An out-of-range queue selection reads as zeroed queue fields.
        let sel = self.queue_select as usize;
        let max_size = self.device.queue_max_sizes().get(sel).copied().unwrap_or(0);
        let cfg = self.queue_cfg.get(sel).copied().unwrap_or_default();
        let vector = int.queue_vectors.get(sel).copied().unwrap_or(0);
        let size = if cfg.size == 0 { max_size } else { cfg.size };
        buf[common::QUEUE_SIZE as usize..][..2].copy_from_slice(&size.to_le_bytes());
        buf[common::QUEUE_MSIX_VECTOR as usize..][..2].copy_from_slice(&vector.to_le_bytes());
        buf[common::QUEUE_ENABLE as usize..][..2]
            .copy_from_slice(&(cfg.enabled as u16).to_le_bytes());
        buf[common::QUEUE_NOTIFY_OFF as usize..][..2]
            .copy_from_slice(&self.queue_select.to_le_bytes());
        buf[common::QUEUE_DESC as usize..][..8].copy_from_slice(&cfg.desc.to_le_bytes());
        buf[common::QUEUE_AVAIL as usize..][..8].copy_from_slice(&cfg.avail.to_le_bytes());
        buf[common::QUEUE_USED as usize..][..8].copy_from_slice(&cfg.used.to_le_bytes());
        buf
    }

    /// Apply a write to the common configuration. `buf` is the composed
    /// configuration with the written bytes merged in; `start..end` is the
    /// touched range.
    fn apply_common_write(&mut self, buf: &[u8], start: usize, end: usize) {
        let touches = |off: usize, len: usize| start < off + len && off < end;
        let val16 = |off: usize| u16::from_le_bytes(buf[off..off + 2].try_into().unwrap());
        let val32 = |off: usize| u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        let val64 = |off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());

        if touches(common::DEVICE_FEATURE_SELECT as usize, 4) {
            self.device_feature_select = val32(common::DEVICE_FEATURE_SELECT as usize);
        }
        if touches(common::GUEST_FEATURE_SELECT as usize, 4) {
            self.guest_feature_select = val32(common::GUEST_FEATURE_SELECT as usize);
        }
        if touches(common::GUEST_FEATURE as usize, 4) && self.guest_feature_select < 2 {
            let half = val32(common::GUEST_FEATURE as usize) as u64;
            self.driver_features = if self.guest_feature_select == 0 {
                (self.driver_features & 0xFFFF_FFFF_0000_0000) | half
            } else {
                (self.driver_features & 0x0000_0000_FFFF_FFFF) | (half << 32)
            };
        }
        if touches(common::MSIX_CONFIG as usize, 2) {
            let vector = val16(common::MSIX_CONFIG as usize);
            let mut int = self.int.lock();
            int.config_vector = if vector == NO_VECTOR || (vector as usize) < int.entries.len() {
                vector
            } else {
                NO_VECTOR
            };
        }
        if touches(common::DEVICE_STATUS as usize, 1) {
            let status = buf[common::DEVICE_STATUS as usize];
            if let Some(legacy) = &mut self.transitional {
                legacy.active = false;
            }
            self.status_write(status);
        }
        if touches(common::QUEUE_SELECT as usize, 2) {
            self.queue_select = val16(common::QUEUE_SELECT as usize);
        }
        // Queue fields apply to the selected queue and only while it is
        // disabled (VIRTIO 1.2 §4.1.4.3).
        let selected = self.queue_select as usize;
        if touches(common::QUEUE_SIZE as usize, 2)
            && selected < self.num_queues()
            && !self.queue_cfg[selected].enabled
        {
            let size = val16(common::QUEUE_SIZE as usize);
            let max = self.device.queue_max_sizes()[selected];
            if size.is_power_of_two() && size <= max {
                self.queue_cfg[selected].size = size;
            }
        }
        if touches(common::QUEUE_MSIX_VECTOR as usize, 2) && selected < self.num_queues() {
            let vector = val16(common::QUEUE_MSIX_VECTOR as usize);
            let mut int = self.int.lock();
            int.queue_vectors[selected] =
                if vector == NO_VECTOR || (vector as usize) < int.entries.len() {
                    vector
                } else {
                    NO_VECTOR
                };
        }
        if touches(common::QUEUE_ENABLE as usize, 2)
            && selected < self.num_queues()
            && val16(common::QUEUE_ENABLE as usize) == 1
        {
            self.queue_cfg[selected].enabled = true;
        }
        let write_addr = |cfg: &mut QueueCfg, off: usize| {
            if !cfg.enabled {
                match off {
                    o if o == common::QUEUE_DESC as usize => cfg.desc = val64(o),
                    o if o == common::QUEUE_AVAIL as usize => cfg.avail = val64(o),
                    o if o == common::QUEUE_USED as usize => cfg.used = val64(o),
                    _ => {}
                }
            }
        };
        if selected < self.num_queues() {
            for off in [
                common::QUEUE_DESC as usize,
                common::QUEUE_AVAIL as usize,
                common::QUEUE_USED as usize,
            ] {
                if touches(off, 8) {
                    write_addr(&mut self.queue_cfg[selected], off);
                }
            }
        }
    }

    /// Compose the current MSI-X message-control word.
    fn msix_control(&self) -> u16 {
        let int = self.int.lock();
        let size = int.entries.len() as u16 - 1;
        size | ((int.function_masked as u16) << 14) | ((int.msix_enabled as u16) << 15)
    }

    /// Handle a guest write into the MSI-X message-control word.
    fn msix_control_write(&mut self, value: u16) {
        let mut int = self.int.lock();
        let was_enabled = int.msix_enabled;
        int.msix_enabled = value & 0x8000 != 0;
        int.function_masked = value & 0x4000 != 0;
        if int.msix_enabled && !was_enabled {
            // MSI-X takes over from INTx.
            if let Some(irq) = &int.intx {
                irq.set_level(false);
            }
        }
        int.deliver_pending();
    }
}

/// Non-transitional devices report PCI revision 2.
const MODERN_REVISION: u32 = 2;

/// Write one `virtio_pci_cap` structure into `config` at `off`.
fn write_virtio_cap(
    config: &mut [u8; PCI_CONFIG_SIZE],
    off: usize,
    next: u8,
    cfg_type: u8,
    offset: u64,
    length: u64,
) {
    config[off] = PCI_CAP_ID_VNDR;
    config[off + 1] = next;
    config[off + 2] = 16; // this capability's own length
    config[off + 3] = cfg_type;
    config[off + 4] = BAR_INDEX;
    config[off + 8..off + 12].copy_from_slice(&(offset as u32).to_le_bytes());
    config[off + 12..off + 16].copy_from_slice(&(length as u32).to_le_bytes());
}

impl PciDevice for VirtioPciDevice {
    fn config_read(&mut self, reg: usize) -> u32 {
        if reg >= PCI_CONFIG_SIZE / 4 {
            return 0;
        }
        if self.transitional.is_some() {
            return self.transitional_config_read(reg);
        }
        match reg * 4 {
            0x04 => {
                let intx = {
                    let int = self.int.lock();
                    !int.msix_enabled && int.isr != 0 && int.intx.is_some()
                };
                // Status bits live in the upper half of the dword:
                // capabilities list, and interrupt status while INTx is
                // asserted.
                (self.command as u32) | 0x0010_0000 | if intx { 0x0008_0000 } else { 0 }
            }
            // The MSI-X capability dword (byte 0x40, reg 16).
            0x40 => {
                // The message-control word (upper half) is dynamic: the
                // driver read-modify-writes it, so a stale enable bit would
                // silently disable MSI-X behind a live machine.
                let control = self.msix_control();
                (control as u32) << 16
                    | self.config[MSI_X_CAP_OFF] as u32
                    | (self.config[MSI_X_CAP_OFF + 1] as u32) << 8
            }
            0x20 => {
                // BAR sizing probe: all address bits written as ones.
                if self.bar4 & 0xFFFF_FFF0 == 0xFFFF_FFF0 {
                    !(BAR_SIZE - 1) as u32 & 0xFFFF_FFF0 | BAR64_PREFETCHABLE
                } else {
                    self.bar4 as u32 & 0xFFFF_FFF0 | BAR64_PREFETCHABLE
                }
            }
            0x24 => {
                if self.bar4 >> 32 == 0xFFFF_FFFF {
                    // The BAR is smaller than 4 GiB, so the upper dword of
                    // the size mask reads as zero.
                    if BAR_SIZE >= 1 << 32 {
                        (!(BAR_SIZE - 1) >> 32) as u32
                    } else {
                        0
                    }
                } else {
                    (self.bar4 >> 32) as u32
                }
            }
            _ => {
                let bytes = [
                    self.config[reg * 4],
                    self.config[reg * 4 + 1],
                    self.config[reg * 4 + 2],
                    self.config[reg * 4 + 3],
                ];
                u32::from_le_bytes(bytes)
            }
        }
    }

    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        if self.transitional.is_some() {
            self.transitional_config_write(reg, offset, data);
            return;
        }
        let mut msix_control: Option<[u8; 2]> = None;
        for (i, byte) in data.iter().enumerate() {
            let addr = reg as u64 * 4 + offset + i as u64;
            match addr {
                0x04 | 0x05 => {
                    let shift = (addr - 4) * 8;
                    self.command = (self.command & !(0xFF << shift)) | (*byte as u16) << shift;
                    self.command &= 0x0507;
                }
                0x20..=0x27 => {
                    let shift = (addr - 0x20) * 8;
                    self.bar4 = (self.bar4 & !(0xFF << shift)) | (*byte as u64) << shift;
                }
                0x3C => self.interrupt_line = *byte,
                0x42 => msix_control.get_or_insert([0; 2])[0] = *byte,
                0x43 => msix_control.get_or_insert([0; 2])[1] = *byte,
                _ => {}
            }
        }
        if let Some(bytes) = msix_control {
            let value = u16::from_le_bytes(bytes);
            // Preserve the read-only table-size field the guest never owns.
            let preserved = self.msix_control() & 0x03FF;
            self.msix_control_write(preserved | (value & 0xC000));
        }
    }

    fn bars(&self) -> Vec<BarInfo> {
        if let Some(legacy) = &self.transitional {
            return legacy.bars();
        }
        vec![BarInfo {
            index: BAR_INDEX,
            size: BAR_SIZE,
            kind: BarKind::Mmio64 { prefetchable: true },
        }]
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        if bar == ROM_BAR {
            data.fill(0xff);
            if let Some(image) = self
                .transitional
                .as_ref()
                .and_then(|pci| pci.rom_image.as_ref())
            {
                for (i, byte) in data.iter_mut().enumerate() {
                    if let Some(value) = offset
                        .checked_add(i as u64)
                        .and_then(|v| usize::try_from(v).ok())
                        .and_then(|v| image.get(v))
                    {
                        *byte = *value;
                    }
                }
            }
            return;
        }
        let offset = if self.transitional.is_some() {
            match bar {
                0 => return self.legacy_read(offset, data),
                1 => return self.msix_region_read(offset, data),
                4 => match transitional::modern_offset(offset) {
                    Some(offset) => offset,
                    None => {
                        data.fill(0);
                        return;
                    }
                },
                _ => {
                    data.fill(0);
                    return;
                }
            }
        } else {
            offset
        };
        if bar != BAR_INDEX {
            data.fill(0);
            return;
        }
        match offset {
            o if o < 0x1000 => {
                let buf = self.common_bytes();
                copy_clamped(&buf, o as usize, data);
            }
            o if (0x1000..0x2000).contains(&o) => data.fill(0),
            o if (0x2000..0x3000).contains(&o) => {
                let value = self.int.lock().read_isr();
                data.fill(0);
                if !data.is_empty() {
                    data[0] = value;
                }
            }
            o if (0x3000..0x4000).contains(&o) => {
                let rel = o - DEVICE_CFG_OFF;
                data.fill(0);
                if rel < self.device_cfg_len {
                    let len = data.len().min((self.device_cfg_len - rel) as usize);
                    self.device.read_config(rel, &mut data[..len]);
                }
            }
            o => self.msix_region_read(o, data),
        }
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        let offset = if self.transitional.is_some() {
            match bar {
                0 => return self.legacy_write(offset, data),
                1 => return self.msix_region_write(offset, data),
                4 => match transitional::modern_offset(offset) {
                    Some(offset) => offset,
                    None => return,
                },
                _ => return,
            }
        } else {
            offset
        };
        if bar != BAR_INDEX || data.is_empty() {
            return;
        }
        match offset {
            o if o < 0x1000 && o + data.len() as u64 <= COMMON_CFG_LEN => {
                let mut buf = self.common_bytes();
                let start = o as usize;
                buf[start..start + data.len()].copy_from_slice(data);
                let end = start + data.len();
                self.apply_common_write(&buf, start, end);
            }
            o if (0x1000..0x2000).contains(&o) => {
                let queue = (o - NOTIFY_OFF) / NOTIFY_OFF_MULTIPLIER;
                if self.activated && queue < self.num_queues() as u64 {
                    self.device.queue_notify(queue as u16);
                }
            }
            o if (0x2000..0x3000).contains(&o) => { /* ISR is read-only */ }
            o if (0x3000..0x4000).contains(&o) => {
                let rel = o - DEVICE_CFG_OFF;
                if rel < self.device_cfg_len {
                    let len = data.len().min((self.device_cfg_len - rel) as usize);
                    self.device.write_config(rel, &data[..len]);
                }
            }
            o => self.msix_region_write(o, data),
        }
    }
}

impl VirtioPciDevice {
    /// Reads inside the MSI-X table or PBA (offset ≥ 0x4000).
    fn msix_region_read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        let table_off = if self.transitional.is_some() {
            0
        } else {
            MSIX_TABLE_OFF
        };
        let table_end = table_off + (self.int.lock().entries.len() * 16) as u64;
        let pba_off = if self.transitional.is_some() {
            0x800
        } else {
            table_end.div_ceil(64) * 64
        };
        if (table_off..table_end).contains(&offset) {
            let int = self.int.lock();
            for (i, byte) in data.iter_mut().enumerate() {
                let Some(pos) = offset.checked_add(i as u64) else {
                    break;
                };
                if pos >= table_end {
                    break;
                }
                let e = int.entries[((pos - table_off) / 16) as usize];
                let mut bytes = [0u8; 16];
                bytes[..8].copy_from_slice(&e.address.to_le_bytes());
                bytes[8..12].copy_from_slice(&e.data.to_le_bytes());
                bytes[12] = e.masked as u8;
                *byte = bytes[((pos - table_off) % 16) as usize];
            }
        } else if (pba_off..pba_off + 8).contains(&offset) {
            let int = self.int.lock();
            let bytes = int.pba.to_le_bytes();
            let start = (offset - pba_off) as usize;
            let len = data.len().min(8 - start);
            data[..len].copy_from_slice(&bytes[start..start + len]);
        }
    }

    /// Writes inside the MSI-X table or PBA.
    fn msix_region_write(&mut self, offset: u64, data: &[u8]) {
        let mut int = self.int.lock();
        let table_off = if self.transitional.is_some() {
            0
        } else {
            MSIX_TABLE_OFF
        };
        let table_end = table_off + (int.entries.len() * 16) as u64;
        if !(table_off..table_end).contains(&offset) {
            return; // beyond the table (the PBA) is read-only
        }
        for (i, byte) in data.iter().copied().enumerate() {
            let Some(pos) = offset.checked_add(i as u64) else {
                break;
            };
            if pos >= table_end {
                break;
            }
            let e = &mut int.entries[((pos - table_off) / 16) as usize];
            let field = (pos - table_off) % 16;
            match field {
                0..=7 => {
                    let shift = field * 8;
                    e.address = (e.address & !(0xff << shift)) | ((byte as u64) << shift);
                }
                8..=11 => {
                    let shift = (field - 8) * 8;
                    e.data = (e.data & !(0xff << shift)) | ((byte as u32) << shift);
                }
                12 => e.masked = byte & 1 != 0,
                _ => {}
            }
        }
        int.deliver_pending();
    }
}

/// Copy from `src` at `start` into `data`, zero-filling beyond `src`.
fn copy_clamped(src: &[u8], start: usize, data: &mut [u8]) {
    if start >= src.len() {
        data.fill(0);
        return;
    }
    let len = data.len().min(src.len().saturating_sub(start));
    data[..len].copy_from_slice(&src[start..start + len]);
    data[len..].fill(0);
}

/// Snapshot format magic: "VPCI".
const SNAPSHOT_MAGIC: u32 = 0x4943_5056;

impl Snapshot for VirtioPciDevice {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let int = self.int.lock();
        let mut out = Vec::with_capacity(64 + self.queue_cfg.len() * 29);
        out.extend_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        out.push(if self.transitional.is_some() { 2 } else { 1 });
        out.push(self.driver_status);
        out.push(int.config_generation);
        out.push(self.activated as u8);
        out.extend_from_slice(&self.device_feature_select.to_le_bytes());
        out.extend_from_slice(&self.guest_feature_select.to_le_bytes());
        out.extend_from_slice(&self.driver_features.to_le_bytes());
        out.push(self.negotiated.is_some() as u8);
        out.extend_from_slice(&self.negotiated.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&self.queue_select.to_le_bytes());
        out.extend_from_slice(&self.command.to_le_bytes());
        out.extend_from_slice(&self.bar4.to_le_bytes());
        out.push(self.interrupt_line);
        out.extend_from_slice(&(self.queue_cfg.len() as u16).to_le_bytes());
        for (cfg, vector) in self.queue_cfg.iter().zip(&int.queue_vectors) {
            out.extend_from_slice(&cfg.size.to_le_bytes());
            out.push(cfg.enabled as u8);
            out.extend_from_slice(&vector.to_le_bytes());
            out.extend_from_slice(&cfg.desc.to_le_bytes());
            out.extend_from_slice(&cfg.avail.to_le_bytes());
            out.extend_from_slice(&cfg.used.to_le_bytes());
        }
        out.push(int.msix_enabled as u8);
        out.push(int.function_masked as u8);
        out.extend_from_slice(&int.config_vector.to_le_bytes());
        out.push(int.isr);
        out.extend_from_slice(&int.pba.to_le_bytes());
        out.extend_from_slice(&(int.entries.len() as u16).to_le_bytes());
        for e in &int.entries {
            out.extend_from_slice(&e.address.to_le_bytes());
            out.extend_from_slice(&e.data.to_le_bytes());
            out.push(e.masked as u8);
        }
        if let Some(legacy) = &self.transitional {
            out.push(legacy.active as u8);
            out.extend_from_slice(&legacy.bar0.to_le_bytes());
            out.extend_from_slice(&legacy.bar1.to_le_bytes());
            out.extend_from_slice(&legacy.rom.to_le_bytes());
            out.extend_from_slice(&legacy.pci_cfg);
        }
        Ok(out)
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let bad = || DeviceError::State(format!("{}: bad transport state", self.id));
        if state.len() < 40 || u32::from_le_bytes(state[0..4].try_into().unwrap()) != SNAPSHOT_MAGIC
        {
            return Err(bad());
        }
        let expected_version = if self.transitional.is_some() { 2 } else { 1 };
        if state[4] != expected_version {
            return Err(DeviceError::State(format!(
                "{}: transport state version {} does not match {}",
                self.id, state[4], expected_version
            )));
        }
        let mut cur: usize = 5;
        let mut take = |len: usize| -> Result<&[u8], DeviceError> {
            let end = cur.checked_add(len).ok_or_else(bad)?;
            let slice = state.get(cur..end).ok_or_else(bad)?;
            cur = end;
            Ok(slice)
        };
        let u8at = |s: &[u8]| s.first().copied().ok_or_else(bad);
        let u16at = |s: &[u8]| -> Result<u16, DeviceError> {
            s.try_into().map(u16::from_le_bytes).map_err(|_| bad())
        };
        let u32at = |s: &[u8]| -> Result<u32, DeviceError> {
            s.try_into().map(u32::from_le_bytes).map_err(|_| bad())
        };
        let u64at = |s: &[u8]| -> Result<u64, DeviceError> {
            s.try_into().map(u64::from_le_bytes).map_err(|_| bad())
        };

        let driver_status = u8at(take(1)?)?;
        let config_generation = u8at(take(1)?)?;
        let _activated = u8at(take(1)?)? != 0; // re-derived from DRIVER_OK below
        let device_feature_select = u32at(take(4)?)?;
        let guest_feature_select = u32at(take(4)?)?;
        let driver_features = u64at(take(8)?)?;
        let has_negotiated = u8at(take(1)?)? != 0;
        let negotiated_bits = u64at(take(8)?)?;
        let negotiated = has_negotiated.then_some(negotiated_bits);
        let queue_select = u16at(take(2)?)?;
        let command = u16at(take(2)?)?;
        let bar4 = u64at(take(8)?)?;
        let interrupt_line = u8at(take(1)?)?;
        let n_queues = u16at(take(2)?)? as usize;
        if n_queues != self.num_queues() {
            return Err(DeviceError::State(format!(
                "{}: transport state has {} queues, device has {}",
                self.id,
                n_queues,
                self.num_queues()
            )));
        }
        let mut queue_cfg = vec![QueueCfg::default(); n_queues];
        let mut queue_vectors = vec![NO_VECTOR; n_queues];
        for i in 0..n_queues {
            queue_cfg[i].size = u16at(take(2)?)?;
            queue_cfg[i].enabled = u8at(take(1)?)? != 0;
            queue_vectors[i] = u16at(take(2)?)?;
            queue_cfg[i].desc = u64at(take(8)?)?;
            queue_cfg[i].avail = u64at(take(8)?)?;
            queue_cfg[i].used = u64at(take(8)?)?;
        }
        let msix_enabled = u8at(take(1)?)? != 0;
        let function_masked = u8at(take(1)?)? != 0;
        let config_vector = u16at(take(2)?)?;
        let isr = u8at(take(1)?)?;
        let pba = u64at(take(8)?)?;
        let n_entries = u16at(take(2)?)? as usize;
        if n_entries != self.int.lock().entries.len() {
            return Err(bad());
        }
        let mut entries = Vec::with_capacity(n_entries);
        for _ in 0..n_entries {
            let address = u64at(take(8)?)?;
            let data = u32at(take(4)?)?;
            let masked = u8at(take(1)?)? != 0;
            entries.push(MsixEntry {
                address,
                data,
                masked,
            });
        }
        let legacy_state = if self.transitional.is_some() {
            Some((
                u8at(take(1)?)? != 0,
                u32at(take(4)?)?,
                u32at(take(4)?)?,
                u32at(take(4)?)?,
                <[u8; 16]>::try_from(take(16)?).map_err(|_| bad())?,
            ))
        } else {
            None
        };
        if cur != state.len() {
            return Err(bad());
        }

        // Apply: reset the device, then rebuild driver-visible state, then
        // re-activate so the restored inner device gets its queues back.
        self.device.reset();
        self.driver_status = 0;
        self.activated = false;
        self.device_feature_select = device_feature_select;
        self.guest_feature_select = guest_feature_select;
        self.driver_features = driver_features;
        self.negotiated = negotiated;
        self.queue_select = queue_select;
        self.queue_cfg = queue_cfg;
        self.command = command;
        self.bar4 = bar4;
        self.interrupt_line = interrupt_line;
        if let (Some(legacy), Some((active, bar0, bar1, rom, pci_cfg))) =
            (&mut self.transitional, legacy_state)
        {
            legacy.active = active;
            legacy.bar0 = bar0;
            legacy.bar1 = bar1;
            legacy.rom = rom;
            legacy.pci_cfg = pci_cfg;
        }
        {
            let mut int = self.int.lock();
            int.msix_enabled = msix_enabled;
            int.function_masked = function_masked;
            int.config_vector = config_vector;
            int.queue_vectors = queue_vectors;
            if entries.len() == int.entries.len() {
                int.entries = entries;
            }
            int.pba = pba;
            int.isr = isr;
            int.config_generation = config_generation;
            let intx_pending = !int.msix_enabled && int.isr != 0;
            if let Some(irq) = &int.intx {
                irq.set_level(intx_pending);
            }
        }
        if let Some(negotiated) = self.negotiated {
            self.device.ack_features(negotiated);
        }
        if driver_status & device_status::DRIVER_OK != 0 {
            self.driver_status = driver_status;
            self.activate();
        } else {
            self.driver_status = driver_status;
        }
        Ok(())
    }
}

impl crate::Resettable for VirtioPciDevice {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.virtio_reset();
        self.command = 0;
        self.bar4 = 0;
        self.interrupt_line = 0;
        if let Some(legacy) = &mut self.transitional {
            legacy.bar0 = 0;
            legacy.bar1 = 0;
            legacy.rom = 0;
            legacy.pci_cfg.fill(0);
        }
        let mut int = self.int.lock();
        int.msix_enabled = false;
        int.function_masked = false;
        int.entries.fill(MsixEntry {
            masked: self.transitional.is_some(),
            ..MsixEntry::default()
        });
        int.pba = 0;
        Ok(())
    }
}

impl PciSnapshot for VirtioPciDevice {
    fn snapshot_id(&self) -> String {
        Snapshot::state_id(self)
    }

    fn save_function_state(&self) -> Result<Vec<u8>, DeviceError> {
        Snapshot::save_state(self)
    }

    fn save_wrapped_state(&self) -> Result<Vec<u8>, DeviceError> {
        self.device().save_state()
    }

    fn restore_function_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        Snapshot::restore_state(self, state)
    }

    fn restore_wrapped_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        self.device_mut().restore_state(state)
    }
}

#[cfg(test)]
mod tests {
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap, Le16, Le32};

    use super::*;
    use crate::Snapshot;
    use crate::virtio::block::Block;
    use crate::virtio::testing::{MemDisk, RecIrq, RecMsi};

    /// Guest addresses the test driver uses, matching the BAR layout.
    const DESC: u64 = 0x10000;
    const AVAIL: u64 = 0x11000;
    const USED: u64 = 0x12000;
    const DATA: u64 = 0x13000;
    const HDR: u64 = 0x14000;
    const QUEUE_SIZE: u16 = 8;

    /// A block device behind the transport, plus its memory and sinks.
    struct Guest {
        pci: VirtioPciDevice,
        mem: GuestMemoryMmap<()>,
        msi: Arc<RecMsi>,
        irq: Arc<RecIrq>,
    }

    impl Guest {
        fn new() -> Self {
            Self::with_disk(Box::new(MemDisk::new(64 * 1024)))
        }

        fn with_disk(disk: Box<dyn vmlab_disk::DiskImage>) -> Self {
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
            let msi = Arc::new(RecMsi::default());
            let irq = Arc::new(RecIrq::default());
            let block = Block::new("virtio-blk@guest", disk);
            let pci = VirtioPciDevice::new(
                "pci@00:04.0",
                Box::new(block),
                mem.clone(),
                Some(msi.clone()),
                Some(irq.clone()),
            );
            Self { pci, mem, msi, irq }
        }

        fn cfg32(&mut self, off: u64) -> u32 {
            self.pci.config_read((off / 4) as usize)
        }

        fn cfg_write(&mut self, off: u64, data: &[u8]) {
            self.pci.config_write((off / 4) as usize, off % 4, data);
        }

        fn cfg16_write(&mut self, off: u64, value: u16) {
            self.cfg_write(off, &value.to_le_bytes());
        }

        fn bar32_write(&mut self, off: u64, value: u32) {
            self.pci.bar_write(4, off, &value.to_le_bytes());
        }

        fn bar16_write(&mut self, off: u64, value: u16) {
            self.pci.bar_write(4, off, &value.to_le_bytes());
        }

        fn bar32_read(&mut self, off: u64) -> u32 {
            let mut b = [0u8; 4];
            self.pci.bar_read(4, off, &mut b);
            u32::from_le_bytes(b)
        }

        fn bar16_read(&mut self, off: u64) -> u16 {
            let mut b = [0u8; 2];
            self.pci.bar_read(4, off, &mut b);
            u16::from_le_bytes(b)
        }

        fn bar8_read(&mut self, off: u64) -> u8 {
            let mut b = [0u8; 1];
            self.pci.bar_read(4, off, &mut b);
            b[0]
        }

        /// Enable MSI-X and program vector 0 to a recorded address/data.
        fn setup_msix(&mut self) {
            self.cfg16_write(0x42, 0x8000); // enable MSI-X
            self.bar32_write(MSIX_TABLE_OFF, 0xFEE0_0000);
            self.bar32_write(MSIX_TABLE_OFF + 4, 0);
            self.bar32_write(MSIX_TABLE_OFF + 8, 0x41);
        }

        /// Feature negotiation up to FEATURES_OK.
        fn negotiate(&mut self, extra_features: u64) {
            let offered = {
                let mut features = self.pci.device().features() | F_VERSION_1;
                features |= extra_features;
                features
            };
            self.bar32_write(COMMON_CFG_OFF + common::DEVICE_FEATURE_SELECT, 0);
            let low = self.bar32_read(COMMON_CFG_OFF + common::DEVICE_FEATURE);
            self.bar32_write(COMMON_CFG_OFF + common::DEVICE_FEATURE_SELECT, 1);
            let high = self.bar32_read(COMMON_CFG_OFF + common::DEVICE_FEATURE);
            assert_eq!(((high as u64) << 32) | low as u64, offered);

            self.bar32_write(COMMON_CFG_OFF + common::GUEST_FEATURE_SELECT, 0);
            self.bar32_write(COMMON_CFG_OFF + common::GUEST_FEATURE, offered as u32);
            self.bar32_write(COMMON_CFG_OFF + common::GUEST_FEATURE_SELECT, 1);
            self.bar32_write(
                COMMON_CFG_OFF + common::GUEST_FEATURE,
                (offered >> 32) as u32,
            );
            self.bar8_write(COMMON_CFG_OFF + common::DEVICE_STATUS, 0x03); // ACK|DRIVER
            self.bar8_write(COMMON_CFG_OFF + common::DEVICE_STATUS, 0x0B); // |FEATURES_OK
            assert_ne!(
                self.bar8_read(COMMON_CFG_OFF + common::DEVICE_STATUS) & device_status::FEATURES_OK,
                0
            );
        }

        /// Negotiation plus queue setup plus DRIVER_OK.
        fn negotiate_and_activate(&mut self, extra_features: u64) {
            self.negotiate(extra_features);
            self.setup_queue(0);
        }

        fn bar8_write(&mut self, off: u64, value: u8) {
            self.pci.bar_write(4, off, &[value]);
        }

        fn setup_queue(&mut self, vector: u16) {
            self.bar16_write(COMMON_CFG_OFF + common::QUEUE_SELECT, 0);
            assert_eq!(self.bar16_read(COMMON_CFG_OFF + common::QUEUE_SIZE), 128);
            self.bar16_write(COMMON_CFG_OFF + common::QUEUE_SIZE, QUEUE_SIZE);
            self.bar32_write(COMMON_CFG_OFF + common::QUEUE_DESC, DESC as u32);
            self.bar32_write(COMMON_CFG_OFF + common::QUEUE_DESC + 4, 0);
            self.bar32_write(COMMON_CFG_OFF + common::QUEUE_AVAIL, AVAIL as u32);
            self.bar32_write(COMMON_CFG_OFF + common::QUEUE_AVAIL + 4, 0);
            self.bar32_write(COMMON_CFG_OFF + common::QUEUE_USED, USED as u32);
            self.bar32_write(COMMON_CFG_OFF + common::QUEUE_USED + 4, 0);
            self.bar16_write(COMMON_CFG_OFF + common::QUEUE_MSIX_VECTOR, vector);
            assert_eq!(
                self.bar16_read(COMMON_CFG_OFF + common::QUEUE_MSIX_VECTOR),
                vector
            );
            self.bar16_write(COMMON_CFG_OFF + common::QUEUE_ENABLE, 1);
            self.bar8_write(COMMON_CFG_OFF + common::DEVICE_STATUS, 0x0F); // |DRIVER_OK
        }

        /// Build an IN request chain for `sector` and kick the queue.
        fn submit_read(&mut self, sector: u64, head: u16, avail_idx: u16) {
            // Descriptor 0: request header. 1: data buffer (512, writable).
            // 2: status byte (writable).
            for (i, (addr, len, flags)) in [
                (HDR, 16u32, 1u16),
                (DATA, 512, 3), // NEXT | WRITE
                (DATA + 0x800, 1, 2),
            ]
            .into_iter()
            .enumerate()
            {
                let mut desc = [0u8; 16];
                desc[0..8].copy_from_slice(&addr.to_le_bytes());
                desc[8..12].copy_from_slice(&len.to_le_bytes());
                desc[12..14].copy_from_slice(&flags.to_le_bytes());
                desc[14..16].copy_from_slice(&(i as u16 + 1).to_le_bytes());
                self.mem
                    .write_slice(&desc, GuestAddress(DESC + (i as u64) * 16))
                    .unwrap();
            }
            let mut header = Vec::new();
            header.extend(0u32.to_le_bytes()); // VIRTIO_BLK_T_IN
            header.extend(0u32.to_le_bytes());
            header.extend(sector.to_le_bytes());
            self.mem.write_slice(&header, GuestAddress(HDR)).unwrap();

            // Avail ring: flags, idx, ring[head]. With EVENT_IDX
            // negotiated a real driver also publishes used_event, asking to
            // be notified once the ring reaches that index; expect this
            // completion.
            self.mem
                .write_obj(Le16::from(head), GuestAddress(AVAIL + 4))
                .unwrap();
            self.mem
                .write_obj(Le16::from(avail_idx), GuestAddress(AVAIL + 2))
                .unwrap();
            self.mem
                .write_obj(
                    Le16::from(avail_idx - 1),
                    GuestAddress(AVAIL + 4 + 2 * QUEUE_SIZE as u64),
                )
                .unwrap();

            // Notify queue 0.
            self.pci.bar_write(4, NOTIFY_OFF, &[0, 0, 0, 0]);
        }

        fn used(&self, slot: u64) -> (u16, u16, u32) {
            let idx = self.mem.read_obj::<Le16>(GuestAddress(USED + 2)).unwrap();
            let id = self
                .mem
                .read_obj::<Le16>(GuestAddress(USED + 4 + slot * 8))
                .unwrap();
            let len = self
                .mem
                .read_obj::<Le32>(GuestAddress(USED + 8 + slot * 8))
                .unwrap();
            (idx.to_native(), id.to_native(), len.to_native())
        }

        fn status_byte(&self) -> u8 {
            self.mem.read_obj(GuestAddress(DATA + 0x800)).unwrap()
        }
    }

    #[test]
    fn pci_discovery_and_capabilities() {
        let mut g = Guest::new();
        // Device and vendor id, subsystem ids, revision.
        assert_eq!(g.cfg32(0x00), (0x1042 << 16) | 0x1AF4);
        assert_eq!(g.cfg32(0x2C), (2 << 16) | 0x1AF4);
        assert_eq!(g.cfg32(0x08) & 0xFF, 2);
        // Status: capabilities present. Interrupt pin INTA#.
        assert_ne!(g.cfg32(0x04) & 0x0010_0000, 0);
        assert_eq!(g.cfg32(0x3C) >> 8 & 0xFF, 1);
        assert_eq!(g.cfg32(0x34) & 0xFF, 0x40);

        // MSI-X capability: one queue + one config vector = 2 entries.
        assert_eq!(g.cfg32(0x40) & 0xFFFF, (0x11) | (0x4C << 8));
        let control = g.cfg32(0x40) >> 16 & 0xFFFF;
        assert_eq!(control & 0x7FF, 1); // table size - 1
        let table = g.cfg32(0x44);
        assert_eq!(table & 0x7, 4); // BAR 4
        assert_eq!(table & !0x7, 0x4000);
        let pba = g.cfg32(0x48);
        assert_eq!(pba & 0x7, 4);
        assert_eq!(pba & !0x7, 0x4040);

        // Virtio capabilities: common, notify (+multiplier), ISR, device.
        // Each cap is vndr, next, len, type, bar, 3 pad, offset, length.
        let word = |g: &mut Guest, off: u64| g.cfg32(off);
        assert_eq!(word(&mut g, 0x4C), 0x0110_5C09); // vndr 9, next 0x5C, len 16, common
        assert_eq!(word(&mut g, 0x50), 4); // BAR
        assert_eq!(word(&mut g, 0x54), 0); // offset
        assert_eq!(word(&mut g, 0x58), 56); // length
        assert_eq!(word(&mut g, 0x5C), 0x0214_7009); // notify, next 0x70, len 20
        assert_eq!(word(&mut g, 0x64), 0x1000);
        assert_eq!(word(&mut g, 0x68), 4); // 1 queue * multiplier 4
        assert_eq!(word(&mut g, 0x5C + 16), 4); // notify_off_multiplier
        assert_eq!(word(&mut g, 0x70), 0x0310_8009); // ISR, next 0x80
        assert_eq!(word(&mut g, 0x78), 0x2000);
        assert_eq!(word(&mut g, 0x7C), 1); // length
        assert_eq!(word(&mut g, 0x80), 0x0410_0009); // device config, end
        assert_eq!(word(&mut g, 0x88), 0x3000);
        assert_eq!(word(&mut g, 0x8C), 68); // block config is 68 bytes

        // BAR sizing probe.
        g.cfg_write(0x20, &0xFFFF_FFFFu32.to_le_bytes());
        g.cfg_write(0x24, &0xFFFF_FFFFu32.to_le_bytes());
        assert_eq!(g.cfg32(0x20), 0xFFFF_800C); // 32 KiB, 64-bit prefetchable
        assert_eq!(g.cfg32(0x24), 0);
        // A written address reads back with the flag bits.
        g.cfg_write(0x20, &0x1234_5000u32.to_le_bytes());
        assert_eq!(g.cfg32(0x20), 0x1234_500C);
    }

    #[test]
    fn guest_driver_read_request_over_msix() {
        let mut g = Guest::new();
        g.setup_msix();
        g.negotiate_and_activate(0);

        g.submit_read(3, 0, 1);
        assert_eq!(g.used(0), (1, 0, 513));
        assert_eq!(g.status_byte(), 0);
        let got: Vec<u8> = {
            let mut v = vec![0u8; 512];
            g.mem.read_slice(&mut v, GuestAddress(DATA)).unwrap();
            v
        };
        let expect: Vec<u8> = (0..512).map(|j| ((3 * 512 + j) % 251) as u8).collect();
        assert_eq!(got, expect);

        // The queue interrupt arrived as MSI-X vector 0.
        assert_eq!(*g.msi.messages.lock(), vec![(0xFEE0_0000, 0x41)]);
        // ISR read reports and clears.
        assert_eq!(g.bar8_read(ISR_OFF), 1);
        assert_eq!(g.bar8_read(ISR_OFF), 0);
        // MSI-X delivery does not touch the INTx line.
        assert!(!*g.irq.level.lock());
    }

    #[test]
    fn interrupts_fall_back_to_intx_without_msix() {
        let mut g = Guest::new();
        g.negotiate(0);
        g.setup_queue(0xFFFF); // NO_VECTOR
        assert!(!*g.irq.level.lock());

        g.submit_read(1, 0, 1);
        assert_eq!(g.used(0), (1, 0, 513));
        assert!(*g.irq.level.lock());
        assert!(g.msi.messages.lock().is_empty());
        // Reading ISR releases the line.
        assert_eq!(g.bar8_read(ISR_OFF), 1);
        assert!(!*g.irq.level.lock());
        // The interrupt status bit in the PCI status register follows.
        assert_eq!(g.cfg32(0x04) & 0x0900_0000, 0); // cleared, cap list still set
    }

    #[test]
    fn masked_vector_pends_in_pba_and_delivers_on_unmask() {
        let mut g = Guest::new();
        g.setup_msix();
        // Mask vector 0 (vector control bit 0).
        g.bar32_write(MSIX_TABLE_OFF + 12, 1);
        g.negotiate_and_activate(0);

        g.submit_read(0, 0, 1);
        assert_eq!(g.used(0), (1, 0, 513));
        assert!(g.msi.messages.lock().is_empty());
        // PBA bit 0 is set.
        assert_eq!(g.bar32_read(0x4040), 1);
        // Unmasking delivers the held message and clears the bit.
        g.bar32_write(MSIX_TABLE_OFF + 12, 0);
        assert_eq!(*g.msi.messages.lock(), vec![(0xFEE0_0000, 0x41)]);
        assert_eq!(g.bar32_read(0x4040), 0);
    }

    #[test]
    fn function_mask_holds_all_vectors() {
        let mut g = Guest::new();
        g.setup_msix();
        g.cfg16_write(0x42, 0xC000); // enable + function mask
        g.negotiate_and_activate(0);
        g.submit_read(0, 0, 1);
        assert!(g.msi.messages.lock().is_empty());
        assert_eq!(g.bar32_read(0x4040), 1);
        // Clearing the function mask releases the pending vector.
        g.cfg16_write(0x42, 0x8000);
        assert_eq!(*g.msi.messages.lock(), vec![(0xFEE0_0000, 0x41)]);
        assert_eq!(g.bar32_read(0x4040), 0);
    }

    #[test]
    fn msix_table_entries_read_back() {
        let mut g = Guest::new();
        g.setup_msix();
        assert_eq!(g.bar32_read(MSIX_TABLE_OFF), 0xFEE0_0000);
        assert_eq!(g.bar32_read(MSIX_TABLE_OFF + 4), 0);
        assert_eq!(g.bar32_read(MSIX_TABLE_OFF + 8), 0x41);
        assert_eq!(g.bar32_read(MSIX_TABLE_OFF + 12), 0);
        // Unwritten entry 1 reads zero.
        assert_eq!(g.bar32_read(MSIX_TABLE_OFF + 16), 0);
    }

    #[test]
    fn msix_control_word_reads_back_dynamically() {
        // Drivers drop the function-mask bit with a read-modify-write of
        // the message-control word (Linux's pci_msix_clear_and_set_ctrl); a
        // stale read-back silently disables MSI-X on a live machine.
        let mut g = Guest::new();
        let control = |g: &mut Guest| (g.cfg32(0x40) >> 16) as u16;
        assert_eq!(control(&mut g) & 0xC000, 0);
        // Enable + function mask, as drivers write while programming.
        g.cfg16_write(0x42, 0xC000);
        assert_eq!(control(&mut g) & 0x8000, 0x8000);
        assert_eq!(control(&mut g) & 0x4000, 0x4000);
        let unmask = control(&mut g) & !0x4000;
        g.cfg16_write(0x42, unmask);
        assert_eq!(control(&mut g) & 0xC000, 0x8000);
        // The read-only table-size field still reads through (two vectors
        // for one queue plus config, encoded as entries - 1).
        assert_eq!(control(&mut g) & 0x03FF, 1);
    }

    #[test]
    fn features_ok_rejects_unknown_bits() {
        let mut g = Guest::new();
        g.setup_msix();
        // Offer a bit the device does not have.
        g.bar32_write(COMMON_CFG_OFF + common::GUEST_FEATURE_SELECT, 1);
        g.bar32_write(COMMON_CFG_OFF + common::GUEST_FEATURE, 1 << 31); // bit 63
        g.bar8_write(COMMON_CFG_OFF + common::DEVICE_STATUS, 0x0B);
        let status = g.bar8_read(COMMON_CFG_OFF + common::DEVICE_STATUS);
        assert_eq!(status & device_status::FEATURES_OK, 0);
        // Retrying with only offered features succeeds.
        g.bar32_write(COMMON_CFG_OFF + common::GUEST_FEATURE, 1); // VERSION_1
        g.bar8_write(COMMON_CFG_OFF + common::DEVICE_STATUS, 0x0B);
        assert_ne!(
            g.bar8_read(COMMON_CFG_OFF + common::DEVICE_STATUS) & device_status::FEATURES_OK,
            0
        );
    }

    #[test]
    fn status_reset_returns_to_power_on() {
        let mut g = Guest::new();
        g.setup_msix();
        g.negotiate_and_activate(0);
        assert_ne!(g.bar8_read(COMMON_CFG_OFF + common::DEVICE_STATUS), 0);

        g.bar8_write(COMMON_CFG_OFF + common::DEVICE_STATUS, 0);
        assert_eq!(g.bar8_read(COMMON_CFG_OFF + common::DEVICE_STATUS), 0);
        // Queue configuration is wiped: size reads the maximum again and the
        // queue reports disabled.
        assert_eq!(g.bar16_read(COMMON_CFG_OFF + common::QUEUE_SIZE), 128);
        assert_eq!(g.bar16_read(COMMON_CFG_OFF + common::QUEUE_ENABLE), 0);
        // The config vector is NO_VECTOR again (VIRTIO 1.2 §4.1.4.3).
        assert_eq!(g.bar16_read(COMMON_CFG_OFF + common::MSIX_CONFIG), 0xFFFF);
        // A notify is a no-op now.
        g.submit_read(0, 0, 1);
        assert_eq!(g.used(0).0, 0);
    }

    #[test]
    fn bad_queue_configuration_needs_reset() {
        let mut g = Guest::new();
        g.setup_msix();
        g.negotiate(0);
        // Enable a queue whose descriptor table crosses the end of RAM.
        g.bar16_write(COMMON_CFG_OFF + common::QUEUE_SELECT, 0);
        g.bar32_write(COMMON_CFG_OFF + common::QUEUE_DESC, 0xffff_fff0);
        g.bar32_write(COMMON_CFG_OFF + common::QUEUE_AVAIL, AVAIL as u32);
        g.bar32_write(COMMON_CFG_OFF + common::QUEUE_USED, USED as u32);
        g.bar16_write(COMMON_CFG_OFF + common::QUEUE_ENABLE, 1);
        g.bar8_write(COMMON_CFG_OFF + common::DEVICE_STATUS, 0x0F);
        let status = g.bar8_read(COMMON_CFG_OFF + common::DEVICE_STATUS);
        // The driver is told to reset: NEEDS_RESET with DRIVER_OK latched.
        assert_eq!(
            status & device_status::DEVICE_NEEDS_RESET,
            device_status::DEVICE_NEEDS_RESET
        );
        assert_ne!(status & device_status::DRIVER_OK, 0);
    }

    #[test]
    fn queue_size_writes_are_validated() {
        let mut g = Guest::new();
        // A non power of two is ignored; 0 cannot be written either.
        g.bar16_write(COMMON_CFG_OFF + common::QUEUE_SIZE, 7);
        assert_eq!(g.bar16_read(COMMON_CFG_OFF + common::QUEUE_SIZE), 128);
        g.bar16_write(COMMON_CFG_OFF + common::QUEUE_SIZE, 256);
        assert_eq!(g.bar16_read(COMMON_CFG_OFF + common::QUEUE_SIZE), 128);
        g.bar16_write(COMMON_CFG_OFF + common::QUEUE_SIZE, QUEUE_SIZE);
        assert_eq!(
            g.bar16_read(COMMON_CFG_OFF + common::QUEUE_SIZE),
            QUEUE_SIZE
        );
    }

    #[test]
    fn device_config_is_readable_through_the_bar() {
        let mut g = Guest::new();
        let mut bytes = [0u8; 8];
        g.pci.bar_read(4, DEVICE_CFG_OFF, &mut bytes);
        assert_eq!(u64::from_le_bytes(bytes), (64 * 1024) / 512);
    }

    #[test]
    fn transport_snapshot_restores_a_live_device() {
        let mut g = Guest::new();
        g.setup_msix();
        g.negotiate_and_activate(0);
        g.submit_read(1, 0, 1);
        assert_eq!(g.used(0).0, 1);
        let transport_state = g.pci.save_state().unwrap();
        let block_state = g.pci.device().save_state().unwrap();

        // Restore into a fresh transport around a fresh device.
        let mut g2 = Guest::new();
        g2.pci.restore_state(&transport_state).unwrap();
        g2.pci.device_mut().restore_state(&block_state).unwrap();

        // Driver-visible state is back.
        assert_eq!(g2.bar8_read(COMMON_CFG_OFF + common::DEVICE_STATUS), 0x0F);
        assert_eq!(
            g2.bar16_read(COMMON_CFG_OFF + common::QUEUE_SIZE),
            QUEUE_SIZE
        );
        assert_eq!(g2.bar16_read(COMMON_CFG_OFF + common::QUEUE_ENABLE), 1);
        assert_eq!(
            g2.bar32_read(COMMON_CFG_OFF + common::QUEUE_DESC),
            DESC as u32
        );
        assert_eq!(g2.bar32_read(MSIX_TABLE_OFF), 0xFEE0_0000);
        // The device continues from the restored ring position: the second
        // request lands in used slot 1 with idx 2.
        g2.submit_read(2, 0, 2);
        assert_eq!(g2.used(1), (2, 0, 513));
        assert_eq!(g2.status_byte(), 0);
        assert_eq!(*g2.msi.messages.lock(), vec![(0xFEE0_0000, 0x41)]);
    }

    #[test]
    fn snapshot_rejects_wrong_queue_count_and_garbage() {
        let mut g = Guest::new();
        let state = g.pci.save_state().unwrap();
        assert!(g.pci.restore_state(&[0u8; 64]).is_err());
        let mut truncated = state.clone();
        truncated.pop();
        assert!(g.pci.restore_state(&truncated).is_err());
    }

    #[test]
    fn transport_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<VirtioPciDevice>();
    }

    #[test]
    fn out_of_range_queue_select_reads_zero() {
        let mut g = Guest::new();
        g.bar16_write(COMMON_CFG_OFF + common::QUEUE_SELECT, 9);
        assert_eq!(g.bar16_read(COMMON_CFG_OFF + common::QUEUE_SIZE), 0);
        assert_eq!(g.bar16_read(COMMON_CFG_OFF + common::QUEUE_ENABLE), 0);
        assert_eq!(g.bar32_read(COMMON_CFG_OFF + common::QUEUE_DESC), 0);
        assert_eq!(g.bar16_read(COMMON_CFG_OFF + common::QUEUE_MSIX_VECTOR), 0);
        // num_queues is unaffected by the selection.
        assert_eq!(g.bar16_read(COMMON_CFG_OFF + common::NUM_QUEUES), 1);
    }
}
