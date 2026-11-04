//! The ivshmem-compatible VMLABDSP monitor and its version-one BAR2 layout.

use std::fs::File;
use std::io;
use std::os::fd::FromRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};
use std::time::{Duration, Instant};

use vm_memory::{Bytes, FileOffset, GuestAddress, GuestMemoryBackend};
use vmlab_hv_proto::machine::VirtualDisplay;
use vmm_sys_util::eventfd::EventFd;

use crate::{BarInfo, BarKind, DeviceError, GuestMemory, PciDevice, Resettable, Snapshot};

pub const HEADER_SIZE: usize = 4096;
pub const CURSOR_OFFSET: u64 = 0x1000;
pub const FRAME_SEQ: u64 = 0x110;
pub const CURSOR_SEQ: u64 = 0x200;
pub const XRGB8888: u32 = 0x34325258;

/// Owns both mappings for the entire lifetime of the KVM memory registration.
pub struct Region {
    memory: GuestMemory,
    file: File,
    size: u64,
    epoch: AtomicU64,
    restore_sequence: AtomicU32,
}

impl Region {
    pub fn new(size: u64) -> io::Result<Arc<Self>> {
        if !VirtualDisplay::valid_memory_bytes(size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "virtual display memory must be a power of two large enough for three 3840x2160 buffers and the cursor",
            ));
        }
        // SAFETY: the name is NUL-terminated and a successful call returns an owned fd.
        let fd = unsafe {
            libc::memfd_create(
                c"vmlab-vdisplay".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: memfd_create returned this new descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(size)?;
        use std::os::fd::AsRawFd;
        // SAFETY: sealing the size retains writable shared mappings.
        if unsafe {
            libc::fcntl(
                file.as_raw_fd(),
                libc::F_ADD_SEALS,
                libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let memory = GuestMemory::from_ranges_with_files([(
            GuestAddress(0),
            size as usize,
            Some(FileOffset::new(file.try_clone()?, 0)),
        )])
        .map_err(io::Error::other)?;
        let region = Arc::new(Self {
            memory,
            file,
            size,
            epoch: AtomicU64::new(0),
            restore_sequence: AtomicU32::new(0),
        });
        region.initialize()?;
        Ok(region)
    }

    pub fn file(&self) -> io::Result<File> {
        self.file.try_clone()
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn host_address(&self) -> u64 {
        self.memory
            .get_host_address(GuestAddress(0))
            .expect("BAR2 mapping") as u64
    }

    fn initialize(&self) -> io::Result<()> {
        self.write(0, &[0; HEADER_SIZE])?;
        self.write(0, b"VMLABDSP")?;
        for (offset, value) in [
            (8, 1),
            (12, HEADER_SIZE as u32),
            (0x18, VirtualDisplay::MAX_WIDTH),
            (0x1c, VirtualDisplay::MAX_HEIGHT),
            (0x20, 3),
            (0x24, VirtualDisplay::BUFFER_STRIDE as u32),
            (0x30, 1),
        ] {
            self.store_u32(offset, value);
        }
        self.write(0x10, &self.size.to_le_bytes())?;
        self.write(0x28, &VirtualDisplay::BUFFERS_OFFSET.to_le_bytes())?;
        self.restore_sequence.store(0, Ordering::Relaxed);
        self.epoch.fetch_add(1, Ordering::Release);
        Ok(())
    }

    pub fn read(&self, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
        self.memory
            .read_slice(bytes, GuestAddress(offset))
            .map_err(io::Error::other)
    }

    pub fn write(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.memory
            .write_slice(bytes, GuestAddress(offset))
            .map_err(io::Error::other)
    }

    pub fn load_u32(&self, offset: u64) -> u32 {
        assert!(offset.is_multiple_of(4) && offset + 4 <= HEADER_SIZE as u64);
        // SAFETY: the mapping is page aligned, live, and the offset is checked above.
        u32::from_le(
            unsafe { &*((self.host_address() + offset) as *const AtomicU32) }
                .load(Ordering::Acquire),
        )
    }

    pub fn store_u32(&self, offset: u64, value: u32) {
        assert!(offset.is_multiple_of(4) && offset + 4 <= HEADER_SIZE as u64);
        // SAFETY: the mapping is page aligned, live, and the offset is checked above.
        unsafe { &*((self.host_address() + offset) as *const AtomicU32) }
            .store(value.to_le(), Ordering::Release);
    }

    fn heartbeat(&self) -> u64 {
        // SAFETY: heartbeat is an aligned u64 inside the live header mapping.
        u64::from_le(
            unsafe { &*((self.host_address() + 0x108) as *const AtomicU64) }
                .load(Ordering::Acquire),
        )
    }

    pub fn request_size(&self, width: u32, height: u32) {
        let width = width.min(VirtualDisplay::MAX_WIDTH);
        let height = height.min(VirtualDisplay::MAX_HEIGHT);
        if (self.load_u32(0x34), self.load_u32(0x38)) != (width, height) {
            self.store_u32(0x34, width);
            self.store_u32(0x38, height);
            self.store_u32(0x3c, self.load_u32(0x3c).wrapping_add(1));
        }
    }
}

pub struct PublishedFrame {
    pub seq: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub damage: Vec<vmlab_hv_display::Rect>,
    pub pixels: Vec<u8>,
}

/// Tracks heartbeat progress using host time, never subtracting guest timestamps.
pub struct Reader {
    region: Arc<Region>,
    heartbeat: Option<(u64, Instant)>,
    epoch: u64,
    wait_after_restore: Option<u32>,
}

impl Reader {
    pub fn new(region: Arc<Region>) -> Self {
        let epoch = region.epoch.load(Ordering::Acquire);
        let wait_after_restore = Some(region.restore_sequence.load(Ordering::Acquire));
        Self {
            region,
            heartbeat: None,
            epoch,
            wait_after_restore,
        }
    }

    pub fn active(&mut self, now: Instant) -> bool {
        let epoch = self.region.epoch.load(Ordering::Acquire);
        if self.epoch != epoch {
            self.epoch = epoch;
            self.wait_after_restore = Some(self.region.restore_sequence.load(Ordering::Acquire));
            self.heartbeat = None;
        }
        if self.region.load_u32(0x100) != 1 {
            self.heartbeat = None;
            return false;
        }
        let value = self.region.heartbeat();
        let (_, changed) = *self.heartbeat.get_or_insert((value, now));
        if self
            .heartbeat
            .is_some_and(|(previous, _)| previous != value)
        {
            self.heartbeat = Some((value, now));
            return true;
        }
        now.saturating_duration_since(changed) < Duration::from_secs(3)
    }

    pub fn frame(&mut self) -> io::Result<Option<PublishedFrame>> {
        self.frame_with_hook(|| {})
    }

    pub fn sequence(&self) -> Option<u32> {
        let seq = self.region.load_u32(FRAME_SEQ);
        (seq != 0 && self.wait_after_restore != Some(seq)).then_some(seq)
    }

    fn frame_with_hook(&mut self, mut sampled: impl FnMut()) -> io::Result<Option<PublishedFrame>> {
        for _ in 0..3 {
            let seq = self.region.load_u32(FRAME_SEQ);
            if seq == 0 || self.wait_after_restore == Some(seq) {
                return Ok(None);
            }
            let buffer = self.region.load_u32(0x114);
            let width = self.region.load_u32(0x118);
            let height = self.region.load_u32(0x11c);
            let stride = self.region.load_u32(0x120);
            let format = self.region.load_u32(0x124);
            let damage = [0x128, 0x12c, 0x130, 0x134].map(|o| self.region.load_u32(o));
            sampled();
            if seq != self.region.load_u32(FRAME_SEQ) {
                continue;
            }
            let length = stride as u64 * height as u64;
            if buffer >= 3
                || width == 0
                || height == 0
                || width > VirtualDisplay::MAX_WIDTH
                || height > VirtualDisplay::MAX_HEIGHT
                || stride < width * 4
                || !stride.is_multiple_of(4)
                || length > VirtualDisplay::BUFFER_STRIDE
                || format != XRGB8888
                || damage[0] as u64 + damage[2] as u64 > width as u64
                || damage[1] as u64 + damage[3] as u64 > height as u64
            {
                return Ok(None);
            }
            let mut pixels = vec![0; length as usize];
            self.region.read(
                VirtualDisplay::BUFFERS_OFFSET + buffer as u64 * VirtualDisplay::BUFFER_STRIDE,
                &mut pixels,
            )?;
            fence(Ordering::Acquire);
            if seq != self.region.load_u32(FRAME_SEQ) {
                continue;
            }
            self.wait_after_restore = None;
            let rect = if damage == [0; 4] {
                [0, 0, width, height]
            } else {
                damage
            };
            return Ok(Some(PublishedFrame {
                seq,
                width,
                height,
                stride,
                damage: vec![vmlab_hv_display::Rect {
                    x: rect[0],
                    y: rect[1],
                    width: rect[2],
                    height: rect[3],
                }],
                pixels,
            }));
        }
        Ok(None)
    }

    pub fn cursor(&self) -> io::Result<Option<vmlab_hv_display::Cursor>> {
        for _ in 0..3 {
            let seq = self.region.load_u32(CURSOR_SEQ);
            let width = self.region.load_u32(0x214);
            let height = self.region.load_u32(0x218);
            let hot_x = self.region.load_u32(0x21c);
            let hot_y = self.region.load_u32(0x220);
            if width > 256
                || height > 256
                || (width != 0 && hot_x >= width)
                || (height != 0 && hot_y >= height)
            {
                return Ok(None);
            }
            let mut cursor = vmlab_hv_display::Cursor {
                width,
                height,
                hot_x,
                hot_y,
                x: self.region.load_u32(0x204) as i32,
                y: self.region.load_u32(0x208) as i32,
                visible: self.region.load_u32(0x20c) != 0,
                argb: vec![0; width as usize * height as usize * 4],
            };
            self.region.read(CURSOR_OFFSET, &mut cursor.argb)?;
            fence(Ordering::Acquire);
            if seq == self.region.load_u32(CURSOR_SEQ) {
                return Ok(Some(cursor));
            }
        }
        Ok(None)
    }
}

/// The VMM implements host registrations; the device remains independent of KVM.
pub trait Mapper: Send {
    fn set_bar(&mut self, bar: u8, base: Option<u64>) -> io::Result<()>;
}

pub struct Device {
    pub region: Arc<Region>,
    pub doorbell: Arc<EventFd>,
    command: u16,
    intr_mask: u32,
    intr_status: u32,
    bases: [Option<u64>; 2],
    mapper: Option<Box<dyn Mapper>>,
    error: Option<String>,
}

impl Device {
    pub fn new(size: u64) -> io::Result<Self> {
        Ok(Self {
            region: Region::new(size)?,
            doorbell: Arc::new(EventFd::new(libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)?),
            command: 0,
            intr_mask: 0,
            intr_status: 0,
            bases: [None; 2],
            mapper: None,
            error: None,
        })
    }
    pub fn set_mapper(&mut self, mapper: Box<dyn Mapper>) {
        self.mapper = Some(mapper);
    }
    fn remap(&mut self) {
        if let Some(mapper) = &mut self.mapper {
            for (index, bar) in [0, 2].into_iter().enumerate() {
                if let Err(error) =
                    mapper.set_bar(bar, self.bases[index].filter(|_| self.command & 2 != 0))
                {
                    self.error = Some(error.to_string());
                }
            }
        }
    }
}

impl PciDevice for Device {
    fn config_read(&mut self, reg: usize) -> u32 {
        match reg {
            0 => 0x1110_1af4,
            1 => self.command as u32,
            2 => 0x0500_0001,
            6 => 0xc,
            11 => 0x1100_1af4,
            _ => 0,
        }
    }
    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        if reg == 1 {
            let mut value = (self.command as u32).to_le_bytes();
            if let Some(bytes) = value.get_mut(offset as usize..offset as usize + data.len()) {
                bytes.copy_from_slice(data);
            }
            self.command = u32::from_le_bytes(value) as u16 & 7;
            self.remap();
        }
    }
    fn bars(&self) -> Vec<BarInfo> {
        vec![
            BarInfo {
                index: 0,
                size: 256,
                kind: BarKind::Mmio32,
            },
            BarInfo {
                index: 2,
                size: self.region.size(),
                kind: BarKind::Mmio64 { prefetchable: true },
            },
        ]
    }
    fn set_bar_address(&mut self, bar: u8, base: Option<u64>) {
        if let Some(index) = [0, 2].iter().position(|b| *b == bar) {
            self.bases[index] = base;
            self.remap();
        }
    }
    fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }
    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if bar == 2 {
            let _ = self.region.read(offset, data);
        }
        if bar == 0 && offset < 16 {
            let mut registers = [0; 16];
            registers[..4].copy_from_slice(&self.intr_mask.to_le_bytes());
            registers[4..8].copy_from_slice(&self.intr_status.to_le_bytes());
            if let Some(bytes) = registers.get(offset as usize..offset as usize + data.len()) {
                data.copy_from_slice(bytes);
            }
            if offset == 4 {
                self.intr_status = 0;
            }
        }
    }
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if bar == 2 {
            let _ = self.region.write(offset, data);
        }
        if bar == 0 && data.len() == 4 {
            let value = u32::from_le_bytes(data.try_into().unwrap());
            match offset {
                0 => self.intr_mask = value,
                4 => self.intr_status = value,
                12 => {
                    let _ = self.doorbell.write(1);
                }
                _ => {}
            }
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct State {
    version: u32,
    command: u16,
    intr_mask: u32,
    intr_status: u32,
    bases: [Option<u64>; 2],
    header: Vec<u8>,
}

impl Snapshot for Device {
    fn state_id(&self) -> String {
        "vdisplay".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut header = vec![0; HEADER_SIZE];
        self.region.read(0, &mut header)?;
        serde_json::to_vec(&State {
            version: 1,
            command: self.command,
            intr_mask: self.intr_mask,
            intr_status: self.intr_status,
            bases: self.bases,
            header,
        })
        .map_err(|e| DeviceError::State(e.to_string()))
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let state: State =
            serde_json::from_slice(state).map_err(|e| DeviceError::State(e.to_string()))?;
        if state.version != 1 || state.header.len() != HEADER_SIZE {
            return Err(DeviceError::State(
                "invalid virtual display snapshot".into(),
            ));
        }
        self.region.write(0, &state.header)?;
        self.region
            .restore_sequence
            .store(self.region.load_u32(FRAME_SEQ), Ordering::Relaxed);
        self.region.epoch.fetch_add(1, Ordering::Release);
        self.command = state.command;
        self.intr_mask = state.intr_mask;
        self.intr_status = state.intr_status;
        self.bases = state.bases;
        self.remap();
        Ok(())
    }
}
impl Resettable for Device {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.command = 0;
        self.intr_mask = 0;
        self.intr_status = 0;
        self.region.initialize()?;
        self.remap();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish(region: &Region, seq: u32) {
        for (offset, value) in [
            (0x100, 1),
            (0x114, 0),
            (0x118, 2),
            (0x11c, 1),
            (0x120, 8),
            (0x124, XRGB8888),
        ] {
            region.store_u32(offset, value);
        }
        region
            .write(VirtualDisplay::BUFFERS_OFFSET, &[1, 2, 3, 0, 4, 5, 6, 0])
            .unwrap();
        region.store_u32(FRAME_SEQ, seq);
    }

    #[test]
    fn guest_mapping_torn_retry_heartbeat_mode_and_header_snapshot() {
        let mut device = Device::new(VirtualDisplay::default_memory_bytes()).unwrap();
        let region = device.region.clone();
        let guest = GuestMemory::from_ranges_with_files([(
            GuestAddress(0),
            region.size() as usize,
            Some(FileOffset::new(region.file().unwrap(), 0)),
        )])
        .unwrap();
        guest
            .write_slice(&42u64.to_le_bytes(), GuestAddress(0x108))
            .unwrap();
        publish(&region, 1);
        let mut reader = Reader::new(region.clone());
        let now = Instant::now();
        assert!(reader.active(now));
        let mut reads = 0;
        let frame = reader
            .frame_with_hook(|| {
                reads += 1;
                if reads == 1 {
                    region.store_u32(FRAME_SEQ, 2);
                }
            })
            .unwrap()
            .unwrap();
        assert_eq!(reads, 2);
        assert_eq!(frame.seq, 2);
        assert_eq!(frame.pixels, [1, 2, 3, 0, 4, 5, 6, 0]);
        assert!(!reader.active(now + Duration::from_secs(3)));
        guest
            .write_slice(&43u64.to_le_bytes(), GuestAddress(0x108))
            .unwrap();
        assert!(reader.active(now + Duration::from_secs(4)));
        region.request_size(9000, 1000);
        assert_eq!(
            (
                region.load_u32(0x34),
                region.load_u32(0x38),
                region.load_u32(0x3c)
            ),
            (3840, 1000, 1)
        );
        region.request_size(9000, 1000);
        assert_eq!(region.load_u32(0x3c), 1);
        let saved = device.save_state().unwrap();
        assert!(saved.len() < 20000);
        region.store_u32(0x104, 77);
        device.restore_state(&saved).unwrap();
        assert!(reader.active(now + Duration::from_secs(5)));
        assert!(reader.frame().unwrap().is_none());
        assert_eq!(region.load_u32(0x104), 0);
        publish(&region, 3);
        assert!(reader.frame().unwrap().is_some());
        device.restore_state(&saved).unwrap();
        publish(&region, 4);
        assert!(reader.active(now + Duration::from_secs(6)));
        assert_eq!(reader.frame().unwrap().unwrap().seq, 4);
        device.bar_write(0, 12, &0u32.to_le_bytes());
        assert_eq!(device.doorbell.read().unwrap(), 1);
        assert_eq!(device.config_read(0), 0x1110_1af4);
        region.store_u32(0x118, u32::MAX);
        assert!(reader.frame().unwrap().is_none());
        device.reset().unwrap();
        assert!(!reader.active(now));
    }
}
