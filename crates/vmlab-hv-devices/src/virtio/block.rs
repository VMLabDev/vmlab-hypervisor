//! virtio-blk over the disk-format seam (VIRTIO 1.2 §5.2).
//!
//! [`Block`] runs any `Box<dyn vmlab_disk::DiskImage>` behind the virtio-pci
//! transport. Its queue handler uses the blocking disk seam. The binary wraps
//! it in [`super::worker::QueueWorker`] so disk I/O does not block a vCPU.
//! Read-only images advertise `VIRTIO_BLK_F_RO` and fail
//! writes, discards included, as the spec requires.

use std::cell::Cell;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::sync::Arc;

use virtio_queue::desc::split::Descriptor;
use virtio_queue::{DescriptorChain, Queue, QueueT};
use vm_memory::bitmap::Bitmap;
use vm_memory::{
    Address, ByteValued, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryRegion,
    GuestRegionMmap, Le32, Le64, VolatileSlice,
};

use crate::{DeviceError, GuestMemory, Snapshot, VirtioDevice, VirtioInterrupt};

/// Virtio device id for a block device.
pub const VIRTIO_ID_BLOCK: u32 = 2;

/// Logical sector size; virtio-blk capacity is in these units.
const SECTOR_SIZE: u64 = 512;
/// Maximum virtqueue size offered to the driver.
const QUEUE_MAX_SIZE: u16 = 256;

// Device feature bits (VIRTIO 1.2 §5.2.3). These are low-word device
// features — bits 5, 9 and 13 — NOT transport features: shifting them into
// the high word makes F_FLUSH land on bit 41, which is VIRTIO_F_ADMIN_VQ,
// and the Linux driver then demands the 1.2-sized common config (the real
// guest rejects the device with -EINVAL exactly there).
const F_RO: u64 = 1 << 5;
const F_SEG_MAX: u64 = 1 << 2;
const F_FLUSH: u64 = 1 << 9;
const F_DISCARD: u64 = 1 << 13;
// Transport features the device opts into (VIRTIO 1.2 §6).
const F_RING_EVENT_IDX: u64 = 1 << 29;
const F_RING_INDIRECT_DESC: u64 = 1 << 28;
/// Legacy split-ring flag suppressing completion interrupts.
const AVAIL_F_NO_INTERRUPT: u16 = 1;

// Request types (VIRTIO 1.2 §5.2.5).
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const T_DISCARD: u32 = 11;
/// Legacy barrier flag some older drivers set on IN/OUT; ignored.
const T_BARRIER: u32 = 0x8000_0000;

// Request completion statuses (VIRTIO 1.2 §5.2.5).
const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

/// Bytes of device-specific configuration exposed at offset 0 (the
/// `virtio_blk_config` layout of VIRTIO 1.2 §5.2.4 through the discard
/// fields).
const CONFIG_LEN: usize = 68;
/// Offset of `capacity` within the configuration.
const OFF_CAPACITY: usize = 0;
/// Offset of `writeback` (the `wce` field) within the configuration.
const OFF_WRITEBACK: usize = 32;
/// Offset of `max_discard_sectors` within the configuration, followed by
/// `max_discard_seg`.
const OFF_DISCARD: usize = 36;
/// Length of the device serial returned by `VIRTIO_BLK_T_GET_ID`.
const SERIAL_LEN: usize = 20;

/// Request header the driver places at the head of each descriptor chain.
#[repr(C)]
#[derive(Clone, Copy)]
struct ReqHeader {
    kind: Le32,
    reserved: Le32,
    sector: Le64,
}

/// SAFETY: `ReqHeader` contains only `Le32`/`Le64` plain-old-data fields and
/// no padding, so any byte pattern is a valid value.
unsafe impl ByteValued for ReqHeader {}

/// One `virtio_blk_discard_write_zeroes` range.
#[repr(C)]
#[derive(Clone, Copy)]
struct DiscardRange {
    sector: Le64,
    num_sectors: Le32,
    flags: Le32,
}

/// SAFETY: `DiscardRange` contains only `Le32`/`Le64` plain-old-data fields
/// and no padding, so any byte pattern is a valid value.
unsafe impl ByteValued for DiscardRange {}

/// Bounded split-ring walk (VIRTIO 1.2 sections 2.7.5 and 2.7.5.3).
/// Metadata usually fits in one region. Resolve it directly rather than
/// constructing a guest-memory slice iterator for each 16-byte descriptor.
struct ReadDescriptors<'a> {
    access: GuestAccess<'a>,
    table: GuestAddress,
    table_slice: Option<VolatileSlice<'a>>,
    entries: u16,
    index: u16,
    remaining: u16,
    bytes: u32,
    indirect_allowed: bool,
    indirect: bool,
    finished: bool,
}

/// Cache a region only for this request. The borrowed memory map cannot
/// change, and every access still checks its address and length.
struct GuestAccess<'a> {
    mem: &'a GuestMemory,
    region: Cell<Option<&'a GuestRegionMmap>>,
}

impl<'a> GuestAccess<'a> {
    fn region(
        &self,
        address: GuestAddress,
    ) -> Option<(&'a GuestRegionMmap, vm_memory::MemoryRegionAddress)> {
        let (region, offset) = self
            .region
            .get()
            .and_then(|region| {
                region
                    .to_region_addr(address)
                    .map(|offset| (region, offset))
            })
            .or_else(|| {
                let region = self.mem.find_region(address)?;
                Some((region, region.to_region_addr(address)?))
            })?;
        self.region.set(Some(region));
        Some((region, offset))
    }

    fn slice(&self, address: GuestAddress, len: usize) -> Option<VolatileSlice<'a>> {
        let (region, offset) = self.region(address)?;
        region.get_slice(offset, len).ok()
    }

    fn span(&self, address: GuestAddress, max_len: usize) -> Option<VolatileSlice<'a>> {
        let (region, offset) = self.region(address)?;
        let len = max_len.min((region.len() - offset.raw_value()) as usize);
        region.get_slice(offset, len).ok()
    }

    fn object<T: ByteValued>(&self, address: GuestAddress) -> Option<T> {
        match self.slice(address, size_of::<T>()) {
            Some(slice) => slice.read_obj(0).ok(),
            None => self.mem.read_obj(address).ok(),
        }
    }
}

struct GuestReadTarget<'a, 'b> {
    access: GuestAccess<'a>,
    ranges: &'b [(GuestAddress, usize)],
    index: usize,
    offset: usize,
    remaining: usize,
}

impl<'a> GuestReadTarget<'a, '_> {
    fn next_span(&mut self, len: usize) -> vmlab_disk::Result<VolatileSlice<'a>> {
        let invalid = || vmlab_disk::Error::Unsupported("block read target is out of range".into());
        if len == 0 || self.remaining == 0 {
            return Err(invalid());
        }
        let &(base, count) = self.ranges.get(self.index).ok_or_else(invalid)?;
        let address = base.checked_add(self.offset as u64).ok_or_else(invalid)?;
        let slice = self
            .access
            .span(address, len.min(self.remaining).min(count - self.offset))
            .ok_or_else(invalid)?;
        self.offset += slice.len();
        self.remaining -= slice.len();
        if self.offset == count {
            self.index += 1;
            self.offset = 0;
        }
        Ok(slice)
    }
}

impl vmlab_disk::ReadTarget for GuestReadTarget<'_, '_> {
    fn zero(&mut self, mut len: usize) -> vmlab_disk::Result<()> {
        while len != 0 {
            let slice = self.next_span(len)?;
            let guard = slice.ptr_guard_mut();
            // SAFETY: the guard retains a validated writable guest mapping
            // for slice.len() bytes. No Rust reference aliases those bytes.
            unsafe { std::ptr::write_bytes(guard.as_ptr(), 0, slice.len()) };
            slice.bitmap().mark_dirty(0, slice.len());
            len -= slice.len();
        }
        Ok(())
    }

    fn write_all(&mut self, mut bytes: &[u8]) -> vmlab_disk::Result<()> {
        while !bytes.is_empty() {
            let slice = self.next_span(bytes.len())?;
            slice.copy_from(&bytes[..slice.len()]);
            bytes = &bytes[slice.len()..];
        }
        Ok(())
    }

    fn read_file(
        &mut self,
        file: &std::fs::File,
        mut offset: u64,
        mut len: usize,
    ) -> vmlab_disk::Result<()> {
        // A fixed batch stays below Linux's IOV_MAX without a per-request
        // allocation. Guards retain every mapping throughout all short reads.
        const BATCH: usize = 256;
        while len != 0 {
            let mut slices: [Option<VolatileSlice<'_>>; BATCH] = std::array::from_fn(|_| None);
            let mut count = 0;
            let mut bytes = 0;
            while count < BATCH && bytes < len {
                let slice = self.next_span(len - bytes)?;
                slice.bitmap().mark_dirty(0, slice.len());
                bytes += slice.len();
                slices[count] = Some(slice);
                count += 1;
            }
            let guards: [_; BATCH] =
                std::array::from_fn(|i| slices[i].as_ref().map(|slice| slice.ptr_guard_mut()));
            let mut vectors: [_; BATCH] = std::array::from_fn(|i| libc::iovec {
                iov_base: guards[i]
                    .as_ref()
                    .map_or(std::ptr::null_mut(), |guard| guard.as_ptr().cast()),
                iov_len: slices[i].as_ref().map_or(0, VolatileSlice::len),
            });
            let mut first = 0;
            while first < count {
                let position: libc::off_t = offset.try_into().map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "disk offset exceeds off_t",
                    )
                })?;
                // SAFETY: each vector names a validated writable volatile
                // slice retained by slices and guards until the read finishes.
                // No Rust reference to guest bytes is created.
                let read = unsafe {
                    libc::preadv(
                        file.as_raw_fd(),
                        vectors[first..count].as_ptr(),
                        (count - first) as i32,
                        position,
                    )
                };
                if read < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error.into());
                }
                if read == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "short block file read",
                    )
                    .into());
                }
                offset += read as u64;
                let mut consumed = read as usize;
                while consumed != 0 {
                    let vector = &mut vectors[first];
                    let used = consumed.min(vector.iov_len);
                    vector.iov_base = vector.iov_base.cast::<u8>().wrapping_add(used).cast();
                    vector.iov_len -= used;
                    consumed -= used;
                    if vector.iov_len == 0 {
                        first += 1;
                    }
                }
            }
            len -= bytes;
        }
        Ok(())
    }
}

impl ReadDescriptors<'_> {
    fn next(&mut self) -> Result<Option<Descriptor>, ()> {
        loop {
            if self.finished {
                return Ok(None);
            }
            if self.remaining == 0 || self.index >= self.entries {
                return Err(());
            }
            let address = self
                .table
                .checked_add(u64::from(self.index) * size_of::<Descriptor>() as u64)
                .ok_or(())?;
            let desc: Descriptor = match &self.table_slice {
                Some(slice) => slice
                    .read_obj(usize::from(self.index) * size_of::<Descriptor>())
                    .map_err(|_| ())?,
                None => self.access.object(address).ok_or(())?,
            };
            if desc.refers_to_indirect_table() {
                let len = desc.len() as usize;
                if !self.indirect_allowed
                    || self.indirect
                    || desc.has_next()
                    || len == 0
                    || !len.is_multiple_of(size_of::<Descriptor>())
                {
                    return Err(());
                }
                self.entries = (len / size_of::<Descriptor>()).try_into().map_err(|_| ())?;
                self.table = desc.addr();
                self.table_slice = self.access.slice(self.table, len);
                self.index = 0;
                self.indirect = true;
                continue;
            }
            self.bytes = self.bytes.checked_add(desc.len()).ok_or(())?;
            self.remaining -= 1;
            self.finished = !desc.has_next();
            self.index = desc.next();
            return Ok(Some(desc));
        }
    }
}

/// Snapshot format magic: "VBLK".
const SNAPSHOT_MAGIC: u32 = 0x4B4C_4256;
/// Serialized bytes per queue state (see [`virtio_queue::QueueState`]).
const QUEUE_STATE_BYTES: usize = 2 + 2 + 2 + 1 + 2 + 1 + 8 + 8 + 8;

/// A virtio-blk device over one disk image.
pub struct Block {
    id: String,
    disk: Box<dyn vmlab_disk::DiskImage>,
    /// Guest-visible device-specific configuration (capacity, discard limits).
    config: [u8; CONFIG_LEN],
    features: u64,
    acked_features: u64,
    /// Present once `activate` ran; cleared by `reset`.
    mem: Option<GuestMemory>,
    interrupt: Option<Arc<dyn VirtioInterrupt>>,
    queues: Vec<Queue>,
    queue_sizes: Vec<u16>,
    /// Queue states restored before activation, applied by `activate`.
    pending_queues: Option<Vec<virtio_queue::QueueState>>,
    /// Reused scratch for buffered requests; never snapshotted.
    io_buffer: Vec<u8>,
    /// Coalesced guest ranges for reads, rebuilt without retaining guest pointers.
    read_ranges: Vec<(GuestAddress, usize)>,
}

impl Block {
    /// New block device over `disk`. A read-only image yields a read-only
    /// device; a writable image also advertises DISCARD, which the image
    /// implementation is free to treat as a hint.
    pub fn new(id: impl Into<String>, disk: Box<dyn vmlab_disk::DiskImage>) -> Self {
        let read_only = disk.read_only();
        let capacity = disk.size() / SECTOR_SIZE;
        let mut features = F_RING_EVENT_IDX | F_RING_INDIRECT_DESC | F_SEG_MAX | F_FLUSH;
        if read_only {
            features |= F_RO;
        } else {
            features |= F_DISCARD;
        }

        let mut config = [0u8; CONFIG_LEN];
        config[OFF_CAPACITY..OFF_CAPACITY + 8].copy_from_slice(&capacity.to_le_bytes());
        // Reserve a descriptor for the header and one for the status.
        config[12..16].copy_from_slice(&u32::from(QUEUE_MAX_SIZE - 2).to_le_bytes());
        config[20..24].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
        config[OFF_WRITEBACK] = 1; // writes are flushed through the seam
        if !read_only {
            config[OFF_DISCARD..OFF_DISCARD + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            config[OFF_DISCARD + 4..OFF_DISCARD + 8].copy_from_slice(&1u32.to_le_bytes());
            // discard_sector_alignment 0; write-zeroes fields stay zero (the
            // feature is not advertised).
        }

        Self {
            id: id.into(),
            disk,
            config,
            features,
            acked_features: 0,
            mem: None,
            interrupt: None,
            queues: Vec::new(),
            queue_sizes: vec![QUEUE_MAX_SIZE],
            pending_queues: None,
            io_buffer: Vec::new(),
            read_ranges: Vec::new(),
        }
    }

    /// Configure the request queues before transport construction.
    pub fn with_num_queues(mut self, count: u16) -> Result<Self, DeviceError> {
        if count == 0 || count > 256 || self.mem.is_some() {
            return Err(DeviceError::State(
                "invalid block queue count or active device".into(),
            ));
        }
        self.queue_sizes = vec![QUEUE_MAX_SIZE; count as usize];
        self.config[34..36].copy_from_slice(&count.to_le_bytes());
        if count > 1 {
            self.features |= 1 << 12;
        } else {
            self.features &= !(1 << 12);
        }
        Ok(self)
    }

    /// Process every available request on queue `index`, publish them on the
    /// used ring and raise the queue interrupt once.
    fn process_queue(&mut self, index: usize) {
        let Some(mem) = self.mem.clone() else { return };
        let Some(interrupt) = self.interrupt.clone() else {
            return;
        };
        if !self.queues.get(index).is_some_and(|q| q.ready()) {
            return;
        }

        // With VIRTIO_F_EVENT_IDX negotiated, driver kicks are suppressed
        // once the device has consumed buffers, until the device publishes
        // the new avail_event (`enable_notification`). Drain, re-enable,
        // and drain again if buffers arrived while the event was being
        // published — the loop the spec's suppression mechanism expects.
        let mut used_any = false;
        loop {
            while let Some(chain) = self
                .queues
                .get_mut(index)
                .and_then(|q| q.pop_descriptor_chain(&mem))
            {
                let head = chain.head_index();
                let table = self.queues[index].desc_table();
                let size = self.queues[index].size();
                let written = self.handle_chain(&mem, chain, table, size);
                // The used ring lives in guest memory validated at
                // activation; failing here means the guest corrupted it,
                // and the only recourse is to drop the request.
                match self
                    .queues
                    .get_mut(index)
                    .and_then(|q| q.add_used(&mem, head, written as u32).ok())
                {
                    Some(()) => used_any = true,
                    None => break,
                }
            }
            match self
                .queues
                .get_mut(index)
                .and_then(|q| q.enable_notification(&mem).ok())
            {
                // Buffers arrived while publishing the event: drain them.
                Some(true) => continue,
                _ => break,
            }
        }
        let notified = self.queues.get_mut(index).is_some_and(|q| {
            q.needs_notification(&mem).unwrap_or(false)
                && (q.event_idx_enabled()
                    || mem
                        .load::<u16>(
                            GuestAddress(q.avail_ring()),
                            std::sync::atomic::Ordering::Relaxed,
                        )
                        .is_ok_and(|flags| u16::from_le(flags) & AVAIL_F_NO_INTERRUPT == 0))
        });
        if used_any && notified {
            interrupt.notify_queue(index as u16);
        }
    }

    /// Serve one descriptor chain and return the number of bytes written to
    /// the driver-writable descriptors (data plus status byte).
    fn handle_chain(
        &mut self,
        mem: &GuestMemory,
        chain: DescriptorChain<&GuestMemory>,
        table: u64,
        size: u16,
    ) -> usize {
        if let Some(written) = self.read_chain(mem, table, size, chain.head_index()) {
            return written;
        }
        let mut reader = match chain.clone().reader(mem) {
            Ok(r) => r,
            Err(_) => return 0,
        };
        let header: ReqHeader = match reader.read_obj() {
            Ok(h) => h,
            Err(_) => return 0, // not even a header: nothing to answer on
        };
        let mut writer = match chain.writer(mem) {
            Ok(w) => w,
            Err(_) => return 0,
        };

        let kind = header.kind.to_native() & !T_BARRIER;
        let sector = header.sector.to_native();
        let status = match kind {
            T_IN => self.read(sector, &mut writer),
            T_OUT => self.write(sector, &mut reader, &mut writer),
            T_FLUSH => match self.disk.flush() {
                Ok(()) => S_OK,
                Err(_) => S_IOERR,
            },
            T_GET_ID => {
                let serial: [u8; SERIAL_LEN] = *b"vmlab-hv-blk-0000000";
                match writer.write_all(&serial) {
                    Ok(()) => S_OK,
                    Err(_) => S_IOERR,
                }
            }
            T_DISCARD => self.discard(&mut reader),
            _ => S_UNSUPP,
        };
        // The status byte terminates the chain: it belongs at the very end
        // of the writable descriptors. When the request produced no data
        // (a failure, or a command with no payload) the writer has not
        // consumed the data descriptors, so split the tail off and write
        // there instead of at the front.
        let remaining = writer.available_bytes();
        if remaining > 1 {
            if let Ok(mut tail) = writer.split_at(remaining - 1) {
                let _ = tail.write_all(&[status]);
                return writer.bytes_written() + tail.bytes_written();
            }
            return writer.bytes_written();
        }
        let _ = writer.write_all(&[status]);
        writer.bytes_written()
    }

    /// Read requests normally have one header followed by writable buffers.
    /// Walk that layout once and combine adjacent guest ranges before address
    /// translation. Other layouts retain the scatter-gather reader/writer path.
    fn read_chain(&mut self, mem: &GuestMemory, table: u64, size: u16, head: u16) -> Option<usize> {
        let access = GuestAccess {
            mem,
            region: Cell::new(None),
        };
        let table_slice = access.slice(
            GuestAddress(table),
            usize::from(size) * size_of::<Descriptor>(),
        );
        let mut descriptors = ReadDescriptors {
            access,
            table: GuestAddress(table),
            table_slice,
            entries: size,
            index: head,
            remaining: size,
            bytes: 0,
            indirect_allowed: self.acked_features & F_RING_INDIRECT_DESC != 0,
            indirect: false,
            finished: false,
        };
        let first = match descriptors.next() {
            Ok(first) => first?,
            Err(()) => return Some(0),
        };
        if first.is_write_only() || first.len() as usize != size_of::<ReqHeader>() {
            return None;
        }
        let header: ReqHeader = descriptors.access.object(first.addr())?;
        if header.kind.to_native() & !T_BARRIER != T_IN {
            return None;
        }
        self.read_ranges.clear();
        let mut total = 0usize;
        loop {
            let desc = match descriptors.next() {
                Ok(Some(desc)) => desc,
                Ok(None) => break,
                Err(()) => return Some(0),
            };
            if !desc.is_write_only() {
                return None;
            }
            let len = desc.len() as usize;
            total = total.checked_add(len)?;
            if len == 0 {
                continue;
            }
            if let Some((address, count)) = self.read_ranges.last_mut()
                && address.checked_add(*count as u64) == Some(desc.addr())
            {
                *count += len;
            } else {
                self.read_ranges.push((desc.addr(), len));
            }
        }
        // A truncated descriptor walk or an invalid range must not cause disk
        // I/O or a partial guest write. Keep validation ahead of the transfer.
        if self.read_ranges.iter().any(|&(address, len)| {
            descriptors.access.slice(address, len).is_none() && !mem.check_range(address, len)
        }) {
            return Some(0);
        }
        let &(last, len) = self.read_ranges.last()?;
        let status_address = last.checked_add((len - 1) as u64)?;
        let data_len = total - 1;
        let offset = header.sector.to_native().checked_mul(SECTOR_SIZE);
        let valid = data_len != 0
            && offset
                .and_then(|offset| offset.checked_add(data_len as u64))
                .is_some_and(|end| end <= self.disk.size());
        let status = if !valid {
            S_IOERR
        } else {
            let mut target = GuestReadTarget {
                access: descriptors.access,
                ranges: &self.read_ranges,
                index: 0,
                offset: 0,
                remaining: data_len,
            };
            if self
                .disk
                .read_into(offset.unwrap(), data_len, &mut target)
                .is_ok()
            {
                S_OK
            } else {
                S_IOERR
            }
        };
        let written = if status == S_OK { data_len } else { 0 };
        Some(written + usize::from(mem.write_obj(status, status_address).is_ok()))
    }

    /// `VIRTIO_BLK_T_IN`: fill the driver-writable descriptors from the
    /// image.
    fn read(&mut self, sector: u64, writer: &mut virtio_queue::Writer<'_, ()>) -> u8 {
        let Some(offset) = sector.checked_mul(SECTOR_SIZE) else {
            return S_IOERR;
        };
        // One byte at the end of the writable descriptors is the status.
        let data_len = writer.available_bytes().saturating_sub(1);
        if data_len == 0 {
            return S_IOERR;
        }
        let Some(end) = offset.checked_add(data_len as u64) else {
            return S_IOERR;
        };
        if end > self.disk.size() {
            return S_IOERR; // out of range
        }
        if self.io_buffer.len() < data_len {
            self.io_buffer.resize(data_len, 0);
        }
        if self
            .disk
            .read_at(offset, &mut self.io_buffer[..data_len])
            .is_err()
        {
            return S_IOERR;
        }
        match writer.write_all(&self.io_buffer[..data_len]) {
            Ok(()) => S_OK,
            Err(_) => S_IOERR,
        }
    }

    /// `VIRTIO_BLK_T_OUT`: drain the driver-readable descriptors into the
    /// image.
    fn write(
        &mut self,
        sector: u64,
        reader: &mut virtio_queue::Reader<'_, ()>,
        writer: &mut virtio_queue::Writer<'_, ()>,
    ) -> u8 {
        if self.disk.read_only() {
            return S_IOERR;
        }
        let Some(offset) = sector.checked_mul(SECTOR_SIZE) else {
            return S_IOERR;
        };
        let data_len = reader.available_bytes();
        if self.io_buffer.len() < data_len {
            self.io_buffer.resize(data_len, 0);
        }
        if reader.read_exact(&mut self.io_buffer[..data_len]).is_err() {
            return S_IOERR;
        }
        if let Some(end) = offset.checked_add(data_len as u64) {
            if end > self.disk.size() {
                return S_IOERR; // out of range
            }
        } else {
            return S_IOERR;
        }
        if self
            .disk
            .write_at(offset, &self.io_buffer[..data_len])
            .is_err()
        {
            return S_IOERR;
        }
        // The response consumes no writable bytes beyond the status.
        let _ = writer;
        S_OK
    }

    /// `VIRTIO_BLK_T_DISCARD`: one 16-byte range per segment.
    fn discard(&mut self, reader: &mut virtio_queue::Reader<'_, ()>) -> u8 {
        if self.features & self.acked_features & F_DISCARD == 0 {
            return S_UNSUPP;
        }
        while reader.available_bytes() > 0 {
            let range: DiscardRange = match reader.read_obj() {
                Ok(r) => r,
                Err(_) => return S_IOERR,
            };
            let sector = range.sector.to_native();
            let sectors = range.num_sectors.to_native() as u64;
            let (Some(offset), Some(len)) = (
                sector.checked_mul(SECTOR_SIZE),
                sectors.checked_mul(SECTOR_SIZE),
            ) else {
                return S_IOERR;
            };
            if !offset
                .checked_add(len)
                .is_some_and(|end| end <= self.disk.size())
            {
                return S_IOERR;
            }
            if self.disk.discard(offset, len).is_err() {
                return S_IOERR;
            }
        }
        S_OK
    }
}

impl VirtioDevice for Block {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_BLOCK
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn features(&self) -> u64 {
        self.features
    }

    fn ack_features(&mut self, features: u64) {
        // Set semantics: the transport hands over the whole negotiated set.
        self.acked_features = features & self.features;
    }

    fn config_len(&self) -> u64 {
        CONFIG_LEN as u64
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        if offset >= CONFIG_LEN as u64 {
            data.fill(0);
            return;
        }
        let start = offset as usize;
        let end = (start + data.len()).min(CONFIG_LEN);
        data[..end - start].copy_from_slice(&self.config[start..end]);
        data[end - start..].fill(0);
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // The block configuration is read-only; VIRTIO_BLK_F_CONFIG_WCE is
        // not advertised.
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Arc<dyn VirtioInterrupt>,
        mut queues: Vec<Queue>,
    ) -> Result<(), DeviceError> {
        if let Some(states) = self.pending_queues.take() {
            for (queue, state) in queues.iter_mut().zip(states) {
                if let Ok(q) = Queue::try_from(state) {
                    *queue = q;
                }
            }
        }
        self.mem = Some(mem);
        self.interrupt = Some(interrupt);
        self.queues = queues;
        Ok(())
    }

    fn queue_notify(&mut self, index: u16) {
        self.process_queue(index as usize);
    }

    fn reset(&mut self) {
        self.acked_features = 0;
        self.mem = None;
        self.interrupt = None;
        self.queues.clear();
        self.pending_queues = None;
    }
}

/// Snapshot layout: magic, version, acked features, queue count, then one
/// queue-state blob per queue. The transport restores the driver-written
/// configuration; the device restores its live ring positions.
impl Snapshot for Block {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut out = Vec::with_capacity(15 + self.queues.len() * QUEUE_STATE_BYTES);
        out.extend_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        out.push(1); // format version
        out.extend_from_slice(&self.acked_features.to_le_bytes());
        out.extend_from_slice(&(self.queues.len() as u16).to_le_bytes());
        for q in &self.queues {
            let s = q.state();
            out.extend_from_slice(&s.max_size.to_le_bytes());
            out.extend_from_slice(&s.next_avail.to_le_bytes());
            out.extend_from_slice(&s.next_used.to_le_bytes());
            out.extend_from_slice(&(s.event_idx_enabled as u8).to_le_bytes());
            out.extend_from_slice(&s.size.to_le_bytes());
            out.extend_from_slice(&(s.ready as u8).to_le_bytes());
            out.extend_from_slice(&s.desc_table.to_le_bytes());
            out.extend_from_slice(&s.avail_ring.to_le_bytes());
            out.extend_from_slice(&s.used_ring.to_le_bytes());
        }
        Ok(out)
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let bad = || DeviceError::State(format!("{}: bad block state", self.id));
        if state.len() < 15 || u32::from_le_bytes(state[0..4].try_into().unwrap()) != SNAPSHOT_MAGIC
        {
            return Err(bad());
        }
        if state[4] != 1 {
            return Err(DeviceError::State(format!(
                "{}: block state version {} is not 1",
                self.id, state[4]
            )));
        }
        let acked = u64::from_le_bytes(state[5..13].try_into().unwrap());
        let n = u16::from_le_bytes(state[13..15].try_into().unwrap()) as usize;
        if state.len() != 15 + n * QUEUE_STATE_BYTES {
            return Err(bad());
        }
        let mut states = Vec::with_capacity(n);
        for i in 0..n {
            let b = &state[15 + i * QUEUE_STATE_BYTES..];
            let u16at = |k: usize| -> Result<u16, DeviceError> {
                b[k..k + 2]
                    .try_into()
                    .map(u16::from_le_bytes)
                    .map_err(|_| bad())
            };
            let u64at = |k: usize| -> Result<u64, DeviceError> {
                b[k..k + 8]
                    .try_into()
                    .map(u64::from_le_bytes)
                    .map_err(|_| bad())
            };
            states.push(virtio_queue::QueueState {
                max_size: u16at(0)?,
                next_avail: u16at(2)?,
                next_used: u16at(4)?,
                event_idx_enabled: b[6] != 0,
                size: u16at(7)?,
                ready: b[9] != 0,
                desc_table: u64at(10)?,
                avail_ring: u64at(18)?,
                used_ring: u64at(26)?,
            });
        }
        self.acked_features = acked & self.features;
        if self.mem.is_some() {
            for (q, s) in self.queues.iter_mut().zip(states) {
                *q = Queue::try_from(s).map_err(|_| bad())?;
            }
        } else {
            self.pending_queues = Some(states);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap, Le16, Le32, Le64};

    use super::*;
    use crate::virtio::testing::MemDisk;
    use crate::{Snapshot, VirtioDevice, VirtioInterrupt};

    /// A transport stand-in that records queue notifications.
    #[derive(Default)]
    struct RecInterrupt {
        notifies: Mutex<Vec<u16>>,
    }

    impl VirtioInterrupt for RecInterrupt {
        fn notify_queue(&self, queue: u16) {
            self.notifies.lock().push(queue);
        }
        fn notify_config(&self) {}
    }

    /// Driver-side descriptor flags (VIRTIO 1.2 §2.6.5).
    const NEXT: u16 = 1;
    const WRITE: u16 = 2;

    /// One vring descriptor, in wire layout.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Desc {
        addr: Le64,
        len: Le32,
        flags: Le16,
        next: Le16,
    }

    /// SAFETY: the struct is four fixed-width little-endian fields with no
    /// padding, so any byte pattern is a valid value.
    unsafe impl ByteValued for Desc {}

    fn desc(addr: u64, len: u32, flags: u16, next: u16) -> Desc {
        Desc {
            addr: Le64::from(addr),
            len: Le32::from(len),
            flags: Le16::from(flags),
            next: Le16::from(next),
        }
    }

    /// An activated block device with a 16-entry queue at fixed guest
    /// addresses, plus the memory the "driver" builds chains in.
    struct Fixture {
        block: Block,
        mem: GuestMemoryMmap<()>,
        desc: u64,
        avail: u64,
        used: u64,
        buf: u64,
        headers: u64,
        interrupt: Arc<RecInterrupt>,
        submitted: u16,
    }

    impl Fixture {
        fn new(disk: Box<dyn vmlab_disk::DiskImage>) -> Self {
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
            let interrupt = Arc::new(RecInterrupt::default());
            let mut block = Block::new("virtio-blk@test", disk);
            block.ack_features(block.features());
            let mut q = Queue::new(QUEUE_MAX_SIZE).unwrap();
            q.set_size(16);
            q.set_desc_table_address(Some(0x1000), Some(0));
            q.set_avail_ring_address(Some(0x2000), Some(0));
            q.set_used_ring_address(Some(0x3000), Some(0));
            q.set_event_idx(true);
            q.set_ready(true);
            assert!(q.is_valid(&mem));
            block
                .activate(
                    mem.clone(),
                    interrupt.clone() as Arc<dyn VirtioInterrupt>,
                    vec![q],
                )
                .unwrap();
            Self {
                block,
                mem,
                desc: 0x1000,
                avail: 0x2000,
                used: 0x3000,
                buf: 0x4000,
                headers: 0x5000,
                interrupt,
                submitted: 0,
            }
        }

        fn write_desc(&self, i: u16, d: Desc) {
            self.mem
                .write_obj(d, GuestAddress(self.desc + (i as u64) * 16))
                .unwrap();
        }

        fn put_header(&self, kind: u32, sector: u64) {
            self.mem
                .write_obj(
                    ReqHeader {
                        kind: Le32::from(kind),
                        reserved: Le32::from(0),
                        sector: Le64::from(sector),
                    },
                    GuestAddress(self.headers),
                )
                .unwrap();
        }

        /// Header (readable) -> data at `buf` -> status byte at `buf+0x800`.
        fn chain_hdr_data_status(&self, data_len: u32, data_writable: bool) {
            self.write_desc(0, desc(self.headers, 16, NEXT, 1));
            let data_flags = NEXT | if data_writable { WRITE } else { 0 };
            self.write_desc(1, desc(self.buf, data_len, data_flags, 2));
            self.write_desc(2, desc(self.buf + 0x800, 1, WRITE, 0));
        }

        /// Header (readable) -> status byte (writable) only.
        fn chain_hdr_status(&self) {
            self.write_desc(0, desc(self.headers, 16, NEXT, 1));
            self.write_desc(1, desc(self.buf + 0x800, 1, WRITE, 0));
        }

        /// Put `head` in the avail ring and update the index; slot 0.
        fn submit(&mut self, head: u16) {
            self.submitted += 1;
            self.submit_at(head, 0, self.submitted);
        }

        /// Put `head` in avail slot `slot`, set avail idx to `idx`.
        fn submit_at(&mut self, head: u16, slot: u16, idx: u16) {
            self.mem
                .write_obj(
                    Le16::from(head),
                    GuestAddress(self.avail + 4 + (slot as u64) * 2),
                )
                .unwrap();
            self.mem
                .write_obj(Le16::from(idx), GuestAddress(self.avail + 2))
                .unwrap();
            self.block.queue_notify(0);
        }

        /// Used-ring state: (idx, ring[slot].id, ring[slot].len).
        fn used_slot(&self, slot: u64) -> (u16, u16, u32) {
            let idx = self
                .mem
                .read_obj::<Le16>(GuestAddress(self.used + 2))
                .unwrap();
            let id = self
                .mem
                .read_obj::<Le16>(GuestAddress(self.used + 4 + slot * 8))
                .unwrap();
            let len = self
                .mem
                .read_obj::<Le32>(GuestAddress(self.used + 8 + slot * 8))
                .unwrap();
            (idx.to_native(), id.to_native(), len.to_native())
        }

        fn status(&self) -> u8 {
            self.mem.read_obj(GuestAddress(self.buf + 0x800)).unwrap()
        }
    }

    fn sector_content(i: u64) -> Vec<u8> {
        (0..512).map(|j| ((i * 512 + j) % 251) as u8).collect()
    }

    #[test]
    fn file_reads_fill_fragmented_guest_targets_across_batches_and_report_eof() {
        use vmlab_disk::ReadTarget;

        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let ranges: Vec<_> = (0..300)
            .map(|i| (GuestAddress(0x10000 + i * 2048), 512))
            .collect();
        let content: Vec<_> = (0..300 * 512 + 7).map(|i| (i % 251) as u8).collect();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&content).unwrap();
        let mut target = GuestReadTarget {
            access: GuestAccess {
                mem: &mem,
                region: Cell::new(None),
            },
            ranges: &ranges,
            index: 0,
            offset: 0,
            remaining: 300 * 512,
        };
        target.read_file(&file, 7, 300 * 512).unwrap();
        assert_eq!(target.remaining, 0);
        for (i, &(address, len)) in ranges.iter().enumerate() {
            let mut received = vec![0; len];
            mem.read_slice(&mut received, address).unwrap();
            assert_eq!(&received, &content[7 + i * 512..7 + (i + 1) * 512]);
        }
        file.set_len(7).unwrap();
        target.index = 0;
        target.offset = 0;
        target.remaining = 512;
        assert!(target.read_file(&file, 0, 512).is_err());
    }

    #[test]
    fn zero_reads_preserve_unrequested_data_and_the_status_byte() {
        use vmlab_disk::ReadTarget;

        let mem = GuestMemoryMmap::from_ranges(&[
            (GuestAddress(0), 0x10000),
            (GuestAddress(0x10000), 0x10000),
        ])
        .unwrap();
        let ranges = [(GuestAddress(0xff00), 512), (GuestAddress(0x18000), 1025)];
        for &(address, len) in &ranges {
            mem.write_slice(&vec![0xa5; len], address).unwrap();
        }
        let mut target = GuestReadTarget {
            access: GuestAccess {
                mem: &mem,
                region: Cell::new(None),
            },
            ranges: &ranges,
            index: 0,
            offset: 0,
            remaining: 1536,
        };
        target.zero(768).unwrap();
        assert_eq!(target.remaining, 768);
        let mut received = [1; 512];
        mem.read_slice(&mut received, ranges[0].0).unwrap();
        assert_eq!(received, [0; 512]);
        let mut received = [1; 1025];
        mem.read_slice(&mut received, ranges[1].0).unwrap();
        assert_eq!(&received[..256], &[0; 256]);
        assert_eq!(&received[256..], &[0xa5; 769]);
    }

    #[test]
    #[ignore = "diagnostic: compare zero filling over 4 GiB of guest writes per path"]
    fn zero_fill_throughput() {
        use vmlab_disk::ReadTarget;

        struct Buffered<'a>(GuestReadTarget<'a, 'a>);
        impl ReadTarget for Buffered<'_> {
            fn write_all(&mut self, bytes: &[u8]) -> vmlab_disk::Result<()> {
                self.0.write_all(bytes)
            }
        }
        const LEN: usize = 256 << 20;
        const REPEATS: usize = 16;
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), LEN)]).unwrap();
        let ranges = [(GuestAddress(0), LEN)];
        let target = || GuestReadTarget {
            access: GuestAccess {
                mem: &mem,
                region: Cell::new(None),
            },
            ranges: &ranges,
            index: 0,
            offset: 0,
            remaining: LEN,
        };
        target().zero(LEN).unwrap();
        for direct in [false, true] {
            let started = std::time::Instant::now();
            for _ in 0..REPEATS {
                if direct {
                    target().zero(LEN).unwrap();
                } else {
                    Buffered(target()).zero(LEN).unwrap();
                }
            }
            let seconds = started.elapsed().as_secs_f64();
            eprintln!(
                "zero fill direct={direct}: {:.2} MiB/s ({seconds:.6}s)",
                (LEN * REPEATS / 1048576) as f64 / seconds
            );
            let mut check = [1; 65536];
            for offset in (0..LEN).step_by(check.len()) {
                mem.read_slice(&mut check, GuestAddress(offset as u64))
                    .unwrap();
                assert!(check.iter().all(|byte| *byte == 0));
            }
        }
    }

    #[test]
    fn read_walker_rejects_cycles_nested_and_unnegotiated_indirect_tables() {
        for malformed in 0..5 {
            let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
            f.put_header(T_IN, 2);
            f.chain_hdr_data_status(512, true);
            match malformed {
                0 => f.write_desc(1, desc(f.buf, 512, NEXT | WRITE, 1)),
                1 => f.write_desc(1, desc(f.buf, 512, NEXT | WRITE, 16)),
                2 => {
                    f.write_desc(0, desc(0x8000, 16, 4, 0));
                    f.mem
                        .write_obj(desc(0x9000, 16, 4, 0), GuestAddress(0x8000))
                        .unwrap();
                }
                3 => f.write_desc(0, desc(0x8000, 17, 4, 0)),
                4 => {
                    f.block.acked_features &= !F_RING_INDIRECT_DESC;
                    f.write_desc(0, desc(0x8000, 16, 4, 0));
                }
                _ => unreachable!(),
            }
            f.mem
                .write_obj(0xa5u8, GuestAddress(f.buf + 0x800))
                .unwrap();
            f.submit(0);
            assert_eq!(f.used_slot(0), (1, 0, 0));
            assert_eq!(f.status(), 0xa5);
        }
    }

    #[test]
    fn advertises_scatter_gather_requests() {
        let block = Block::new("segments", Box::new(MemDisk::new(64 * 1024)));
        assert_ne!(block.features() & (1 << 2), 0);
        assert_ne!(block.features() & (1 << 28), 0);
        let mut max = [0; 4];
        block.read_config(12, &mut max);
        assert_eq!(u32::from_le_bytes(max), u32::from(QUEUE_MAX_SIZE - 2));
    }

    #[test]
    fn scatter_gather_reads_and_writes_preserve_data_and_status() {
        for kind in [T_IN, T_OUT] {
            let disk = MemDisk::new(64 * 1024);
            let content = disk.content();
            let mut f = Fixture::new(Box::new(disk));
            let payload: Vec<u8> = (0..12288).map(|i| (i % 239) as u8).collect();
            if kind == T_IN {
                content.lock()[1024..1024 + payload.len()].copy_from_slice(&payload);
            } else {
                f.mem
                    .write_slice(&payload[..8192], GuestAddress(0x10000))
                    .unwrap();
                f.mem
                    .write_slice(&payload[8192..], GuestAddress(0x30000))
                    .unwrap();
            }
            f.put_header(kind, 2);
            let flags = NEXT | if kind == T_IN { WRITE } else { 0 };
            f.write_desc(0, desc(f.headers, 16, NEXT, 1));
            f.write_desc(1, desc(0x10000, 8192, flags, 2));
            f.write_desc(2, desc(0x30000, 4096, flags, 3));
            f.write_desc(3, desc(f.buf + 0x800, 1, WRITE, 0));
            f.submit(0);
            assert_eq!(f.status(), S_OK);
            if kind == T_IN {
                let mut received = vec![0; payload.len()];
                f.mem
                    .read_slice(&mut received[..8192], GuestAddress(0x10000))
                    .unwrap();
                f.mem
                    .read_slice(&mut received[8192..], GuestAddress(0x30000))
                    .unwrap();
                assert_eq!(received, payload);
                assert_eq!(f.used_slot(0), (1, 0, 12289));
            } else {
                assert_eq!(&content.lock()[1024..1024 + payload.len()], &payload);
                assert_eq!(f.used_slot(0), (1, 0, 1));
            }
        }
    }

    #[test]
    fn reads_reuse_guest_ranges_without_exposing_previous_data() {
        let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
        f.put_header(T_IN, 2);
        f.chain_hdr_data_status(1024, true);
        f.submit(0);
        let allocation = f.block.read_ranges.as_ptr();
        f.put_header(T_IN, 3);
        f.chain_hdr_data_status(512, true);
        f.submit_at(0, 1, 2);
        assert_eq!(f.status(), S_OK);
        assert_eq!(f.block.read_ranges.as_ptr(), allocation);
        assert!(f.block.io_buffer.is_empty());
        let mut received = [0; 512];
        f.mem
            .read_slice(&mut received, GuestAddress(f.buf))
            .unwrap();
        assert_eq!(&received, sector_content(3).as_slice());
    }

    #[test]
    fn indirect_reads_coalesce_maximum_segments_and_reuse_ranges() {
        let mut f = Fixture::new(Box::new(MemDisk::new(1 << 20)));
        f.block.queues[0].set_size(QUEUE_MAX_SIZE);
        let table = 0x8000;
        let start = 0x10000;
        let payload_len = usize::from(QUEUE_MAX_SIZE - 2) * 512;
        f.put_header(T_IN, 0);
        f.write_desc(0, desc(table, u32::from(QUEUE_MAX_SIZE) * 16, 4, 0));
        f.mem
            .write_obj(desc(f.headers, 16, NEXT, 1), GuestAddress(table))
            .unwrap();
        for i in 1..QUEUE_MAX_SIZE - 1 {
            f.mem
                .write_obj(
                    desc(start + u64::from(i - 1) * 512, 512, NEXT | WRITE, i + 1),
                    GuestAddress(table + u64::from(i) * 16),
                )
                .unwrap();
        }
        f.mem
            .write_obj(
                desc(start + payload_len as u64, 1, WRITE, 0),
                GuestAddress(table + u64::from(QUEUE_MAX_SIZE - 1) * 16),
            )
            .unwrap();
        let mut allocation = None;
        for slot in 0..2 {
            f.submit_at(0, slot, slot + 1);
            assert_eq!(
                f.used_slot(u64::from(slot)),
                (slot + 1, 0, payload_len as u32 + 1)
            );
            assert_eq!(
                f.block.read_ranges,
                [(GuestAddress(start), payload_len + 1)]
            );
            let current = (f.block.io_buffer.as_ptr(), f.block.read_ranges.as_ptr());
            if let Some(previous) = allocation {
                assert_eq!(current, previous);
            }
            allocation = Some(current);
            let mut received = vec![0; payload_len + 1];
            f.mem
                .read_slice(&mut received, GuestAddress(start))
                .unwrap();
            assert_eq!(received[payload_len], S_OK);
            for (i, byte) in received[..payload_len].iter().enumerate() {
                assert_eq!(*byte, (i % 251) as u8);
            }
        }
    }

    #[test]
    fn reads_support_split_headers_and_packed_status() {
        let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
        for split in [false, true] {
            f.put_header(T_IN, 2);
            let data_index = if split {
                f.write_desc(0, desc(f.headers, 8, NEXT, 1));
                f.write_desc(1, desc(f.headers + 8, 8, NEXT, 2));
                2
            } else {
                f.write_desc(0, desc(f.headers, 16, NEXT, 1));
                1
            };
            f.write_desc(data_index, desc(f.buf, 513, WRITE, 0));
            let slot = u16::from(split);
            f.submit_at(0, slot, slot + 1);
            assert_eq!(f.used_slot(u64::from(slot)), (slot + 1, 0, 513));
            let mut received = [0; 513];
            f.mem
                .read_slice(&mut received, GuestAddress(f.buf))
                .unwrap();
            assert_eq!(&received[..512], sector_content(2).as_slice());
            assert_eq!(received[512], S_OK);
        }
    }

    #[test]
    fn coalesced_reads_cross_adjacent_memory_regions_but_reject_holes_and_wraps() {
        for (second_region, address, succeeds) in [
            (0x10000, 0xff00, true),
            (0x20000, 0xff00, false),
            (0x10000, u64::MAX - 255, false),
        ] {
            let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
            f.mem = GuestMemoryMmap::from_ranges(&[
                (GuestAddress(0), 0x10000),
                (GuestAddress(second_region), 0x10000),
            ])
            .unwrap();
            f.block.mem = Some(f.mem.clone());
            f.put_header(T_IN, 2);
            f.write_desc(0, desc(f.headers, 16, NEXT, 1));
            f.write_desc(1, desc(address, 512, NEXT | WRITE, 2));
            f.write_desc(2, desc(f.buf + 0x800, 1, WRITE, 0));
            f.mem
                .write_obj(0xa5u8, GuestAddress(f.buf + 0x800))
                .unwrap();
            f.submit(0);
            if succeeds {
                assert_eq!(f.used_slot(0), (1, 0, 513));
                assert_eq!(f.status(), S_OK);
                let mut received = [0; 512];
                f.mem
                    .read_slice(&mut received, GuestAddress(address))
                    .unwrap();
                assert_eq!(received.as_slice(), sector_content(2));
            } else {
                assert_eq!(f.used_slot(0), (1, 0, 0));
                assert_eq!(f.status(), 0xa5);
                assert!(f.block.io_buffer.is_empty());
            }
        }
    }

    #[test]
    fn polling_legacy_driver_suppresses_completion_interrupts() {
        let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
        f.block.acked_features &= !F_RING_EVENT_IDX;
        f.block.queues[0].set_event_idx(false);
        f.mem.write_obj(1u16, GuestAddress(f.avail)).unwrap();
        f.put_header(T_IN, 2);
        f.chain_hdr_data_status(512, true);
        f.submit(0);
        assert_eq!(f.status(), S_OK);
        assert_eq!(f.used_slot(0), (1, 0, 513));
        assert!(f.interrupt.notifies.lock().is_empty());
        f.mem.write_obj(0u16, GuestAddress(f.avail)).unwrap();
        f.submit(0);
        assert_eq!(*f.interrupt.notifies.lock(), vec![0]);
    }

    #[test]
    fn event_index_takes_precedence_over_the_legacy_interrupt_flag() {
        let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
        f.mem.write_obj(1u16, GuestAddress(f.avail)).unwrap();
        f.put_header(T_IN, 2);
        f.chain_hdr_data_status(512, true);
        f.submit(0);
        assert_eq!(*f.interrupt.notifies.lock(), vec![0]);
    }

    #[test]
    fn reads_fill_the_data_buffer() {
        let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
        f.put_header(T_IN, 2);
        f.chain_hdr_data_status(512, true);
        f.submit(0);
        assert_eq!(f.used_slot(0), (1, 0, 513)); // 512 data + status
        assert_eq!(f.status(), S_OK);
        let mut got = vec![0u8; 512];
        f.mem.read_slice(&mut got, GuestAddress(f.buf)).unwrap();
        assert_eq!(got, sector_content(2));
        assert_eq!(*f.interrupt.notifies.lock(), vec![0]);
    }

    #[test]
    fn writes_reach_the_image() {
        let disk = MemDisk::new(64 * 1024);
        let content = disk.content();
        let mut f = Fixture::new(Box::new(disk));
        f.put_header(T_OUT, 3);
        let payload = [0xA7u8; 512];
        f.mem.write_slice(&payload, GuestAddress(f.buf)).unwrap();
        f.chain_hdr_data_status(512, false);
        f.submit(0);
        assert_eq!(f.status(), S_OK);
        assert_eq!(&content.lock()[3 * 512..4 * 512], &payload);
    }

    #[test]
    fn multiqueue_processes_second_ring_and_restores_its_position() {
        let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
        let queue = f.block.queues.remove(0);
        f.block.reset();
        f.block = f.block.with_num_queues(2).unwrap();
        assert_eq!(f.block.queue_max_sizes(), &[QUEUE_MAX_SIZE; 2]);
        assert_ne!(f.block.features() & (1 << 12), 0);
        let mut count = [0; 2];
        f.block.read_config(34, &mut count);
        assert_eq!(u16::from_le_bytes(count), 2);
        f.block.ack_features(f.block.features());
        f.block
            .activate(
                f.mem.clone(),
                f.interrupt.clone(),
                vec![Queue::new(QUEUE_MAX_SIZE).unwrap(), queue],
            )
            .unwrap();
        f.put_header(T_FLUSH, 0);
        f.chain_hdr_status();
        f.submit(0);
        assert_eq!(f.used_slot(0).0, 0);
        f.block.queue_notify(1);
        assert_eq!(f.status(), S_OK);
        assert_eq!(f.used_slot(0).0, 1);
        assert_eq!(*f.interrupt.notifies.lock(), vec![1]);
        let saved = f.block.save_state().unwrap();
        f.block.queues[1].set_next_avail(0);
        f.block.restore_state(&saved).unwrap();
        assert_eq!(f.block.queues[1].next_avail(), 1);
        f.block.queue_notify(1);
        assert_eq!(f.used_slot(0).0, 1);
    }

    #[test]
    fn flush_completes_with_status_only() {
        let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
        f.put_header(T_FLUSH, 0);
        f.chain_hdr_status();
        f.submit(0);
        assert_eq!(f.status(), S_OK);
        assert_eq!(f.used_slot(0), (1, 0, 1));
    }

    #[test]
    fn get_id_returns_the_device_serial() {
        let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
        f.put_header(T_GET_ID, 0);
        // Header -> 20 writable serial bytes -> status.
        f.write_desc(0, desc(f.headers, 16, NEXT, 1));
        f.write_desc(1, desc(f.buf, 20, NEXT | WRITE, 2));
        f.write_desc(2, desc(f.buf + 0x800, 1, WRITE, 0));
        f.submit(0);
        assert_eq!(f.status(), S_OK);
        let mut serial = [0u8; 20];
        f.mem.read_slice(&mut serial, GuestAddress(f.buf)).unwrap();
        assert_eq!(&serial, b"vmlab-hv-blk-0000000");
        assert_eq!(f.used_slot(0), (1, 0, 21));
    }

    #[test]
    fn discard_is_forwarded_per_range() {
        let disk = MemDisk::new(64 * 1024);
        let log = disk.discard_log();
        let mut f = Fixture::new(Box::new(disk));
        f.put_header(T_DISCARD, 0);
        // Header -> two 16-byte ranges (readable) -> status.
        f.mem
            .write_obj(
                DiscardRange {
                    sector: Le64::from(8),
                    num_sectors: Le32::from(2),
                    flags: Le32::from(0),
                },
                GuestAddress(f.buf),
            )
            .unwrap();
        f.mem
            .write_obj(
                DiscardRange {
                    sector: Le64::from(16),
                    num_sectors: Le32::from(1),
                    flags: Le32::from(0),
                },
                GuestAddress(f.buf + 16),
            )
            .unwrap();
        f.write_desc(0, desc(f.headers, 16, NEXT, 1));
        f.write_desc(1, desc(f.buf, 32, NEXT, 2));
        f.write_desc(2, desc(f.buf + 0x800, 1, WRITE, 0));
        f.submit(0);
        assert_eq!(f.status(), S_OK);
        assert_eq!(*log.lock(), vec![(8 * 512, 1024), (16 * 512, 512)]);
    }

    #[test]
    fn read_only_image_fails_writes_and_discards() {
        let mut f = Fixture::new(Box::new(MemDisk::new_read_only(64 * 1024)));
        f.put_header(T_OUT, 1);
        f.chain_hdr_data_status(512, false);
        f.submit(0);
        assert_eq!(f.status(), S_IOERR);

        f.put_header(T_DISCARD, 0);
        f.chain_hdr_data_status(16, false);
        f.submit(0);
        // DISCARD is not advertised for a read-only device: UNSUPP.
        assert_eq!(f.status(), S_UNSUPP);

        // Reads still work.
        f.put_header(T_IN, 1);
        f.chain_hdr_data_status(512, true);
        f.submit(0);
        assert_eq!(f.status(), S_OK);
    }

    #[test]
    fn read_only_advertises_ro_feature() {
        let ro = Block::new("b", Box::new(MemDisk::new_read_only(512)));
        assert_ne!(ro.features() & F_RO, 0);
        assert_eq!(ro.features() & F_DISCARD, 0);
        let rw = Block::new("b", Box::new(MemDisk::new(512)));
        assert_eq!(rw.features() & F_RO, 0);
    }

    #[test]
    fn config_carries_capacity_and_limits() {
        let blk = Block::new("b", Box::new(MemDisk::new(1 << 20)));
        let mut cfg = [0u8; 8];
        blk.read_config(0, &mut cfg);
        assert_eq!(u64::from_le_bytes(cfg), (1 << 20) / 512);
        let mut four = [0u8; 4];
        blk.read_config(OFF_DISCARD as u64, &mut four);
        assert_eq!(u32::from_le_bytes(four), u32::MAX);
        // Reads past the end are zero-padded.
        let mut tail = [0u8; 4];
        blk.read_config(CONFIG_LEN as u64 - 2, &mut tail);
        assert_eq!(tail[2..], [0, 0]);
    }

    #[test]
    fn out_of_range_requests_fail_with_ioerr() {
        let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
        // The image is 128 sectors; the last full read is at sector 127.
        f.put_header(T_IN, 127);
        f.chain_hdr_data_status(512, true);
        f.submit(0);
        assert_eq!(f.status(), S_OK);
        f.put_header(T_IN, 128);
        f.chain_hdr_data_status(512, true);
        f.submit(0);
        assert_eq!(f.status(), S_IOERR);
    }

    #[test]
    fn unknown_request_type_is_unsupported() {
        let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
        f.put_header(0xDEAD, 0);
        f.chain_hdr_data_status(512, true);
        f.submit(0);
        assert_eq!(f.status(), S_UNSUPP);
    }

    #[test]
    fn reset_clears_activation() {
        let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
        f.put_header(T_IN, 0);
        f.chain_hdr_data_status(512, true);
        f.block.reset();
        f.submit(0);
        // Nothing published: the used ring never advanced.
        assert_eq!(f.used_slot(0).0, 0);
        assert!(f.interrupt.notifies.lock().is_empty());
    }

    #[test]
    fn snapshot_restores_ring_positions() {
        let mut f = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
        f.put_header(T_IN, 1);
        f.chain_hdr_data_status(512, true);
        f.submit(0);
        assert_eq!(f.used_slot(0).0, 1);
        let state = f.block.save_state().unwrap();

        // A fresh device with a fresh queue continues from the restored
        // position instead of restarting at slot 0.
        let mut f2 = Fixture::new(Box::new(MemDisk::new(64 * 1024)));
        f2.block.restore_state(&state).unwrap();
        f2.put_header(T_IN, 2);
        f2.chain_hdr_data_status(512, true);
        f2.submit_at(0, 1, 2); // second chain, avail idx 2
        // Used idx goes 1 -> 2 and lands in slot 1 with head 0, not slot 0.
        assert_eq!(f2.used_slot(1), (2, 0, 513));
        assert_eq!(f2.status(), S_OK);
    }

    #[test]
    fn snapshot_rejects_garbage() {
        let mut blk = Block::new("b", Box::new(MemDisk::new(512)));
        assert!(blk.restore_state(&[0u8; 20]).is_err());
        let ok = blk.save_state().unwrap();
        let mut truncated = ok.clone();
        truncated.pop();
        assert!(blk.restore_state(&truncated).is_err());
    }

    #[test]
    fn block_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Block>();
    }
}
