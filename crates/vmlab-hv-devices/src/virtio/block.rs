//! virtio-blk over the disk-format seam (VIRTIO 1.2 §5.2).
//!
//! [`Block`] runs any `Box<dyn vmlab_disk::DiskImage>` behind the virtio-pci
//! transport. Requests are processed synchronously when the driver notifies
//! the virtqueue: the disk seam is blocking, and phase 1 machines do not need
//! a worker thread. Read-only images advertise `VIRTIO_BLK_F_RO` and fail
//! writes, discards included, as the spec requires.

use std::io::{Read, Write};
use std::sync::Arc;

use virtio_queue::{DescriptorChain, Queue, QueueT};
use vm_memory::{ByteValued, Le32, Le64};

use crate::{DeviceError, GuestMemory, Snapshot, VirtioDevice, VirtioInterrupt};

/// Virtio device id for a block device.
pub const VIRTIO_ID_BLOCK: u32 = 2;

/// Logical sector size; virtio-blk capacity is in these units.
const SECTOR_SIZE: u64 = 512;
/// Maximum virtqueue size offered to the driver.
const QUEUE_MAX_SIZE: u16 = 128;

// Device feature bits (VIRTIO 1.2 §5.2.3). These are low-word device
// features — bits 5, 9 and 13 — NOT transport features: shifting them into
// the high word makes F_FLUSH land on bit 41, which is VIRTIO_F_ADMIN_VQ,
// and the Linux driver then demands the 1.2-sized common config (the real
// guest rejects the device with -EINVAL exactly there).
const F_RO: u64 = 1 << 5;
const F_FLUSH: u64 = 1 << 9;
const F_DISCARD: u64 = 1 << 13;
// Transport features the device opts into (VIRTIO 1.2 §6).
const F_RING_EVENT_IDX: u64 = 1 << 29;

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
    /// Queue states restored before activation, applied by `activate`.
    pending_queues: Option<Vec<virtio_queue::QueueState>>,
}

impl Block {
    /// New block device over `disk`. A read-only image yields a read-only
    /// device; a writable image also advertises DISCARD, which the image
    /// implementation is free to treat as a hint.
    pub fn new(id: impl Into<String>, disk: Box<dyn vmlab_disk::DiskImage>) -> Self {
        let read_only = disk.read_only();
        let capacity = disk.size() / SECTOR_SIZE;
        let mut features = F_RING_EVENT_IDX | F_FLUSH;
        if read_only {
            features |= F_RO;
        } else {
            features |= F_DISCARD;
        }

        let mut config = [0u8; CONFIG_LEN];
        config[OFF_CAPACITY..OFF_CAPACITY + 8].copy_from_slice(&capacity.to_le_bytes());
        // size_max 0, seg_max 1, geometry and topology zero, blk_size 512.
        config[12..16].copy_from_slice(&1u32.to_le_bytes());
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
            pending_queues: None,
        }
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
                let written = self.handle_chain(&mem, chain);
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
        let notified = self
            .queues
            .get_mut(index)
            .is_some_and(|q| q.needs_notification(&mem).unwrap_or(false));
        if used_any && notified {
            interrupt.notify_queue(index as u16);
        }
    }

    /// Serve one descriptor chain and return the number of bytes written to
    /// the driver-writable descriptors (data plus status byte).
    fn handle_chain(&mut self, mem: &GuestMemory, chain: DescriptorChain<&GuestMemory>) -> usize {
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
        let mut buf = vec![0u8; data_len];
        let Some(end) = offset.checked_add(data_len as u64) else {
            return S_IOERR;
        };
        if end > self.disk.size() {
            return S_IOERR; // out of range
        }
        if self.disk.read_at(offset, &mut buf).is_err() {
            return S_IOERR;
        }
        match writer.write_all(&buf) {
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
        let mut buf = vec![0u8; data_len];
        if reader.read_exact(&mut buf).is_err() {
            return S_IOERR;
        }
        if let Some(end) = offset.checked_add(data_len as u64) {
            if end > self.disk.size() {
                return S_IOERR; // out of range
            }
        } else {
            return S_IOERR;
        }
        if self.disk.write_at(offset, &buf).is_err() {
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
        &[QUEUE_MAX_SIZE]
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
