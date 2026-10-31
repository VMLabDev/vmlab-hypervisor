//! The in-process virtio-fs device: a FUSE server over the virtqueues.
//!
//! [`InProcessFs`] wraps a [`PassthroughFs`] rooted at a host directory in
//! the FUSE server stack (`fuse`) and serves it on a modern virtio-pci
//! transport (1af4:105a). One hiprio queue plus one request queue, as
//! virtiofsd shapes them; the guest's FUSE messages are decoded from the
//! descriptor chain, answered, and the used ring updated in the same kick
//! the way the block device does — every operation is a host syscall that
//! is already done when the buffer goes back.
//!
//! Snapshots carry the transport-visible queue positions and the FUSE
//! session itself (open inodes and handles, by path), so a restore resumes
//! the same conversation with the guest.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use virtio_queue::{Queue, QueueState, QueueT};
use vm_memory::GuestMemoryMmap;

use super::fuse::Server;
use super::fuse::transport::{Reader, Writer};
use super::passthrough::PassthroughFs;
use super::vhost_user::{CONFIG_LEN, FS_DEVICE_TYPE, NUM_QUEUES, QUEUE_MAX_SIZE};
use crate::{DeviceError, Snapshot, VirtioDevice, VirtioInterrupt};

/// Snapshot format magic: "IVFS" (in-process virtio-fs).
const SNAPSHOT_MAGIC: u32 = 0x5346_5649;

/// Bytes per queue in the snapshot (see [`QueueState`]'s wire form below).
const QUEUE_STATE_BYTES: usize = 2 + 2 + 2 + 1 + 2 + 1 + 8 + 8 + 8;

/// The virtio-fs configuration this device was built from: a share to
/// serve in-process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InProcessFsConfig {
    /// Mount tag the guest uses; at most 36 bytes.
    pub tag: String,
    /// Host directory to serve.
    pub path: PathBuf,
    /// Refuse every mutating operation.
    pub read_only: bool,
}

/// A virtio-fs device that serves a host directory itself.
pub struct InProcessFs {
    id: String,
    /// Guest-visible device-specific configuration (tag, request queues).
    config: [u8; CONFIG_LEN],
    fs: PassthroughFs,
    server: Arc<Server<PassthroughFs>>,
    inner: Mutex<Inner>,
}

struct Inner {
    acked_features: u64,
    /// Live once `activate` ran; cleared by `reset`.
    active: bool,
    mem: Option<GuestMemory>,
    interrupt: Option<Arc<dyn VirtioInterrupt>>,
    queues: Vec<Queue>,
    /// Queue states restored before activation, applied by `activate`.
    pending_queues: Option<Vec<QueueState>>,
}

type GuestMemory = GuestMemoryMmap<()>;

impl InProcessFs {
    /// Open the share named by `config` and build the FUSE server over it.
    pub fn new(id: impl Into<String>, config: InProcessFsConfig) -> Result<Self, DeviceError> {
        let tag = config.tag.as_bytes();
        if tag.len() > super::vhost_user::TAG_LEN {
            return Err(DeviceError::Other(format!(
                "share tag `{}` is {} bytes; virtio-fs tags are at most {}",
                config.tag,
                tag.len(),
                super::vhost_user::TAG_LEN
            )));
        }
        let meta = fs::symlink_metadata(&config.path)
            .map_err(|e| DeviceError::Other(format!("share {}: {e}", config.path.display())))?;
        if !meta.is_dir() {
            return Err(DeviceError::Other(format!(
                "share {} is not a directory",
                config.path.display()
            )));
        }
        let path = config.path.clone();
        let fs = PassthroughFs::new(config.path, config.read_only)
            .map_err(|e| DeviceError::Other(format!("share {}: {e}", path.display())))?;
        let mut config_bytes = [0u8; CONFIG_LEN];
        config_bytes[..tag.len()].copy_from_slice(tag);
        config_bytes[super::vhost_user::TAG_LEN..]
            .copy_from_slice(&super::vhost_user::NUM_REQUEST_QUEUES.to_le_bytes());
        let server = Arc::new(Server::new(fs.clone()));
        Ok(Self {
            id: id.into(),
            config: config_bytes,
            fs,
            server,
            inner: Mutex::new(Inner {
                acked_features: 0,
                active: false,
                mem: None,
                interrupt: None,
                queues: Vec::new(),
                pending_queues: None,
            }),
        })
    }

    /// The host directory this device serves.
    pub fn root(&self) -> &std::path::Path {
        self.fs.root()
    }

    fn process_queue(&self, index: usize) {
        let mut inner = self.inner.lock();
        let Some(mem) = inner.mem.clone() else {
            return;
        };
        let Some(interrupt) = inner.interrupt.clone() else {
            return;
        };
        if !inner.queues.get(index).is_some_and(|q| q.ready()) {
            return;
        }

        // With VIRTIO_F_EVENT_IDX negotiated, driver kicks are suppressed
        // once the device has consumed buffers, until the device publishes
        // the new avail_event (`enable_notification`). Drain, re-enable,
        // and drain again if buffers arrived while the event was being
        // published — the loop the spec's suppression mechanism expects.
        let mut used_any = false;
        loop {
            while let Some(chain) = inner
                .queues
                .get_mut(index)
                .and_then(|q| q.pop_descriptor_chain(&mem))
            {
                let head = chain.head_index();
                let written = self.handle_chain(&mem, chain);
                // The used ring lives in guest memory validated at
                // activation; failing here means the guest corrupted it,
                // and the only recourse is to drop the request.
                match inner
                    .queues
                    .get_mut(index)
                    .and_then(|q| q.add_used(&mem, head, written).ok())
                {
                    Some(()) => used_any = true,
                    None => break,
                }
            }
            match inner
                .queues
                .get_mut(index)
                .and_then(|q| q.enable_notification(&mem).ok())
            {
                // Buffers arrived while publishing the event: drain them.
                Some(true) => continue,
                _ => break,
            }
        }
        let notified = inner
            .queues
            .get_mut(index)
            .and_then(|q| q.needs_notification(&mem).ok())
            .unwrap_or(false);
        if used_any && notified {
            interrupt.notify_queue(index as u16);
        }
    }

    /// One FUSE request from the guest: decode, answer, and report how many
    /// bytes of reply were written (0 for the no-reply requests, which the
    /// used ring still records).
    fn handle_chain(
        &self,
        mem: &GuestMemory,
        chain: virtio_queue::DescriptorChain<&GuestMemory>,
    ) -> u32 {
        let Ok(reader) = Reader::from_descriptor_chain(mem, chain.clone()) else {
            return 0;
        };
        let Ok(writer) = Writer::new(mem, chain.clone()) else {
            return 0;
        };
        // No DAX window and no metrics: the cache-request handler is absent
        // (SETUPMAPPING answers ENOSYS) and so is the hook.
        match self.server.handle_message(reader, writer, None, None) {
            Ok(written) => written as u32,
            Err(_e) => 0,
        }
    }
}

impl VirtioDevice for InProcessFs {
    fn device_type(&self) -> u32 {
        FS_DEVICE_TYPE
    }

    fn queue_max_sizes(&self) -> &[u16] {
        const MAX_SIZES: [u16; NUM_QUEUES] = [QUEUE_MAX_SIZE; NUM_QUEUES];
        &MAX_SIZES
    }

    fn features(&self) -> u64 {
        // Modern device, event-driven rings; no DAX (no cache capability),
        // no packed rings.
        1 << 32 | 1 << 29
    }

    fn ack_features(&mut self, features: u64) {
        // Set semantics: the transport hands over the whole negotiated set.
        let mut inner = self.inner.lock();
        inner.acked_features = features & self.features();
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
        // The virtio-fs configuration is read-only.
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Arc<dyn VirtioInterrupt>,
        mut queues: Vec<Queue>,
    ) -> Result<(), DeviceError> {
        let mut inner = self.inner.lock();
        if let Some(states) = inner.pending_queues.take() {
            for (queue, state) in queues.iter_mut().zip(states) {
                if let Ok(q) = Queue::try_from(state) {
                    *queue = q;
                }
            }
        }
        inner.mem = Some(mem);
        inner.interrupt = Some(interrupt);
        inner.queues = queues;
        inner.active = true;
        Ok(())
    }

    fn queue_notify(&mut self, index: u16) {
        self.process_queue(index as usize);
    }

    fn reset(&mut self) {
        let mut inner = self.inner.lock();
        inner.acked_features = 0;
        inner.mem = None;
        inner.interrupt = None;
        inner.queues.clear();
        inner.pending_queues = None;
        inner.active = false;
    }
}

/// Snapshot layout: magic, version, acked features, queue count, one queue
/// state per queue, then the FUSE session (open inodes and handles, by
/// path). The transport restores the driver-written configuration; this
/// section restores the live ring positions and the session.
impl Snapshot for InProcessFs {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let inner = self.inner.lock();
        let mut out = Vec::with_capacity(15 + inner.queues.len() * QUEUE_STATE_BYTES);
        out.extend_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        out.push(1); // format version
        out.extend_from_slice(&inner.acked_features.to_le_bytes());
        out.extend_from_slice(&(inner.queues.len() as u16).to_le_bytes());
        for q in &inner.queues {
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
        let session = self.fs.save_state();
        out.extend_from_slice(&(session.len() as u32).to_le_bytes());
        out.extend_from_slice(&session);
        Ok(out)
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let bad = || DeviceError::State(format!("{}: bad in-process fs state", self.id));
        if state.len() < 15 || u32::from_le_bytes(state[0..4].try_into().unwrap()) != SNAPSHOT_MAGIC
        {
            return Err(bad());
        }
        if state[4] != 1 {
            return Err(DeviceError::State(format!(
                "{}: in-process fs state version {} is not 1",
                self.id, state[4]
            )));
        }
        let acked = u64::from_le_bytes(state[5..13].try_into().map_err(|_| bad())?);
        let n = u16::from_le_bytes(state[13..15].try_into().map_err(|_| bad())?) as usize;
        if n != NUM_QUEUES || state.len() < 15 + n * QUEUE_STATE_BYTES {
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
            states.push(QueueState {
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
        let mut cur = 15 + n * QUEUE_STATE_BYTES;
        let len = u32::from_le_bytes(
            state
                .get(cur..cur + 4)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(bad)?,
        ) as usize;
        cur += 4;
        let session = state.get(cur..cur + len).ok_or_else(bad)?;
        cur += len;
        if cur != state.len() {
            return Err(bad());
        }
        let mut inner = self.inner.lock();
        inner.acked_features = acked & self.features();
        self.fs.restore_state(session).map_err(|e| {
            DeviceError::State(format!("{}: restoring the FUSE session: {e}", self.id))
        })?;
        if inner.active {
            // The transport restored first and re-activated the device with
            // the driver's queue layout: apply the live positions directly.
            for (queue, st) in inner.queues.iter_mut().zip(states) {
                if let Ok(q) = Queue::try_from(st) {
                    *queue = q;
                }
            }
        } else {
            inner.pending_queues = Some(states);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::sync::Arc;

    use vm_memory::ByteValued;
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap, Le16, Le32, Le64};

    use super::*;
    use crate::Snapshot;
    use crate::virtio::fs::fuse::abi::fuse_abi::{InHeader, InitIn, Opcode, OutHeader};

    /// Records queue notifications.
    #[derive(Default)]
    struct RecInterrupt {
        notifies: Mutex<Vec<u16>>,
    }

    impl crate::VirtioInterrupt for RecInterrupt {
        fn notify_queue(&self, queue: u16) {
            self.notifies.lock().push(queue);
        }
        fn notify_config(&self) {}
    }

    /// An activated in-process fs with a 16-entry request queue at fixed
    /// guest addresses, plus the memory the "driver" builds chains in.
    struct Fixture {
        fs: InProcessFs,
        mem: GuestMemoryMmap<()>,
        desc: u64,
        avail: u64,
        used: u64,
        request: u64,
        reply: u64,
        interrupt: Arc<RecInterrupt>,
        submitted: u16,
        dir: PathBuf,
        /// Keeps the scratch directory alive for the fixture's life.
        _kept: Option<tempfile::TempDir>,
    }

    impl Fixture {
        fn new(read_only: bool) -> Self {
            Self::over(tempfile::tempdir().unwrap(), read_only)
        }

        /// The same fixture over a directory the caller keeps alive.
        fn over(dir: tempfile::TempDir, read_only: bool) -> Self {
            std::fs::write(dir.path().join("hello.txt"), b"hello fuse\n").unwrap();
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
            let interrupt = Arc::new(RecInterrupt::default());
            let mut fs = InProcessFs::new(
                "fs@test",
                InProcessFsConfig {
                    tag: "scratch".into(),
                    path: dir.path().to_path_buf(),
                    read_only,
                },
            )
            .unwrap();
            fs.ack_features(fs.features());
            let mut hiprio = Queue::new(QUEUE_MAX_SIZE).unwrap();
            hiprio.set_size(16);
            hiprio.set_desc_table_address(Some(0x1000), Some(0));
            hiprio.set_avail_ring_address(Some(0x2000), Some(0));
            hiprio.set_used_ring_address(Some(0x3000), Some(0));
            hiprio.set_ready(true);
            let mut requests = Queue::new(QUEUE_MAX_SIZE).unwrap();
            requests.set_size(16);
            requests.set_desc_table_address(Some(0x5000), Some(0));
            requests.set_avail_ring_address(Some(0x6000), Some(0));
            requests.set_used_ring_address(Some(0x7000), Some(0));
            requests.set_ready(true);
            assert!(hiprio.is_valid(&mem) && requests.is_valid(&mem));
            fs.activate(
                mem.clone(),
                interrupt.clone() as Arc<dyn crate::VirtioInterrupt>,
                vec![hiprio, requests],
            )
            .unwrap();
            Self {
                fs,
                mem,
                desc: 0x5000,
                avail: 0x6000,
                used: 0x7000,
                request: 0x9000,
                reply: 0xA000,
                interrupt,
                submitted: 0,
                dir: dir.path().to_path_buf(),
                _kept: Some(dir),
            }
        }

        /// Compose one FUSE request: header + `body` at the request buffer.
        fn request(&self, opcode: Opcode, nodeid: u64, body: &[u8]) {
            let header = InHeader {
                len: (std::mem::size_of::<InHeader>() + body.len()) as u32,
                opcode: opcode as u32,
                unique: 0x1234,
                nodeid,
                uid: 0,
                gid: 0,
                pid: 1,
                padding: 0,
            };
            self.mem
                .write_obj(header, GuestAddress(self.request))
                .unwrap();
            self.mem
                .write_slice(
                    body,
                    GuestAddress(self.request + std::mem::size_of::<InHeader>() as u64),
                )
                .unwrap();
        }

        /// Readable request descriptor, then `reply_len` writable bytes.
        fn chain(&self, request_len: u32, reply_len: u32) {
            // Descriptor 0: the request (device-readable).
            self.mem
                .write_obj(
                    Desc {
                        addr: Le64::from(self.request),
                        len: Le32::from(request_len),
                        flags: Le16::from(1), // NEXT
                        next: Le16::from(1),
                    },
                    GuestAddress(self.desc),
                )
                .unwrap();
            // Descriptor 1: the reply (device-writable).
            self.mem
                .write_obj(
                    Desc {
                        addr: Le64::from(self.reply),
                        len: Le32::from(reply_len),
                        flags: Le16::from(2), // WRITE
                        next: Le16::from(0),
                    },
                    GuestAddress(self.desc + 16),
                )
                .unwrap();
        }

        fn submit(&mut self) {
            self.submitted += 1;
            self.mem
                .write_obj(Le16::from(0u16), GuestAddress(self.avail + 4))
                .unwrap();
            self.mem
                .write_obj(Le16::from(self.submitted), GuestAddress(self.avail + 2))
                .unwrap();
            self.fs.queue_notify(1);
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

        fn out_header(&self) -> OutHeader {
            self.mem.read_obj(GuestAddress(self.reply)).unwrap()
        }

        fn reply_bytes(&self, len: usize) -> Vec<u8> {
            let mut out = vec![0u8; len];
            self.mem
                .read_slice(&mut out, GuestAddress(self.reply))
                .unwrap();
            out
        }
    }

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

    #[test]
    fn init_negotiates_capabilities() {
        let mut f = Fixture::new(false);
        let init = InitIn {
            major: 7,
            minor: 33,
            max_readahead: 128 << 10,
            // Everything the kernel may offer, minus INIT_EXT: the extended
            // negotiation form is not exercised here.
            flags: !crate::virtio::fs::fuse::abi::fuse_abi::FsOptions::INIT_EXT.bits() as u32,
        };
        // SAFETY: reading the POD request struct as its exact-size bytes.
        f.request(Opcode::Init, 0, unsafe {
            std::slice::from_raw_parts(
                (&init as *const InitIn).cast(),
                std::mem::size_of::<InitIn>(),
            )
        });
        f.chain(
            std::mem::size_of::<InHeader>() as u32 + std::mem::size_of::<InitIn>() as u32,
            512,
        );
        f.submit();
        let (idx, id, len) = f.used_slot(0);
        assert_eq!((idx, id), (1, 0));
        assert!(len > 16, "the reply carries an InitOut");
        let out = f.out_header();
        eprintln!(
            "INIT reply: len={} error={} unique={}",
            out.len, out.error, out.unique
        );
        assert_eq!(out.error, 0, "INIT must succeed");
        let bytes = f.reply_bytes(len as usize);
        // Entry point of the stack: the server decoded the request through
        // the transport and wrote a reply into the writable descriptors.
        assert_eq!(out.len as usize, bytes.len().min(out.len as usize));
        assert_eq!(*f.interrupt.notifies.lock(), vec![1]);
    }

    #[test]
    fn lookup_and_read_serve_the_share() {
        let mut f = Fixture::new(false);
        // LOOKUP "hello.txt" under the root.
        let name = b"hello.txt\0";
        f.request(Opcode::Lookup, 1, name);
        f.chain(
            std::mem::size_of::<InHeader>() as u32 + name.len() as u32,
            256,
        );
        f.submit();
        let (_, _, len) = f.used_slot(0);
        assert!(len > 16, "LOOKUP replies an EntryOut");
        let out = f.out_header();
        assert_eq!(out.error, 0);

        // The EntryOut's inode is at reply offset 16 + 8 (after OutHeader
        // and the entry's inode field of EntryOut).
        let entry_inode = {
            let bytes = f.reply_bytes(len as usize);
            u64::from_le_bytes(bytes[16..24].try_into().unwrap())
        };
        assert!(entry_inode > 1, "the file gets its own inode");

        // READ the whole file through that inode, handle-less getattr not
        // needed: OPEN first for a handle, then READ.
        use crate::virtio::fs::fuse::abi::fuse_abi::OpenIn;
        let open = OpenIn {
            flags: libc::O_RDONLY as u32,
            ..Default::default()
        };
        // SAFETY: reading the POD request struct as its exact-size bytes.
        f.request(Opcode::Open, entry_inode, unsafe {
            std::slice::from_raw_parts(
                (&open as *const OpenIn).cast(),
                std::mem::size_of::<OpenIn>(),
            )
        });
        f.chain(
            std::mem::size_of::<InHeader>() as u32 + std::mem::size_of::<OpenIn>() as u32,
            128,
        );
        f.submit();
        let (_, _, len) = f.used_slot(1);
        assert!(len > 16);
        assert_eq!(f.out_header().error, 0);
        let handle = {
            let bytes = f.reply_bytes(len as usize);
            u64::from_le_bytes(bytes[16..24].try_into().unwrap())
        };
        assert!(handle > 0, "OPEN hands out a handle");

        use crate::virtio::fs::fuse::abi::fuse_abi::ReadIn;
        let read = ReadIn {
            fh: handle,
            offset: 0,
            size: 4096,
            ..Default::default()
        };
        // SAFETY: reading the POD request struct as its exact-size bytes.
        f.request(Opcode::Read, entry_inode, unsafe {
            std::slice::from_raw_parts(
                (&read as *const ReadIn).cast(),
                std::mem::size_of::<ReadIn>(),
            )
        });
        f.chain(
            std::mem::size_of::<InHeader>() as u32 + std::mem::size_of::<ReadIn>() as u32,
            4096,
        );
        f.submit();
        let (_, _, len) = f.used_slot(2);
        let out = f.out_header();
        assert_eq!(out.error, 0);
        let bytes = f.reply_bytes(len as usize);
        assert_eq!(&bytes[16..], b"hello fuse\n");
    }

    #[test]
    fn read_only_share_refuses_a_write_on_the_queue() {
        let mut f = Fixture::new(true);
        use crate::virtio::fs::fuse::abi::fuse_abi::OpenIn;
        // LOOKUP + OPEN + WRITE on the read-only share: the OPEN fails.
        let name = b"hello.txt\0";
        f.request(Opcode::Lookup, 1, name);
        f.chain(
            std::mem::size_of::<InHeader>() as u32 + name.len() as u32,
            256,
        );
        f.submit();
        let (_, _, len) = f.used_slot(0);
        let entry_inode = {
            let bytes = f.reply_bytes(len as usize);
            u64::from_le_bytes(bytes[16..24].try_into().unwrap())
        };
        let open = OpenIn {
            flags: libc::O_WRONLY as u32,
            ..Default::default()
        };
        // SAFETY: reading the POD request struct as its exact-size bytes.
        f.request(Opcode::Open, entry_inode, unsafe {
            std::slice::from_raw_parts(
                (&open as *const OpenIn).cast(),
                std::mem::size_of::<OpenIn>(),
            )
        });
        f.chain(
            std::mem::size_of::<InHeader>() as u32 + std::mem::size_of::<OpenIn>() as u32,
            128,
        );
        f.submit();
        let out = f.out_header();
        assert_eq!(out.error, -libc::EROFS);
    }

    #[test]
    fn snapshot_round_trips_the_session_and_ring_positions() {
        let mut f = Fixture::new(false);
        // LOOKUP, then snapshot with the inode live.
        let name = b"hello.txt\0";
        f.request(Opcode::Lookup, 1, name);
        f.chain(
            std::mem::size_of::<InHeader>() as u32 + name.len() as u32,
            256,
        );
        f.submit();
        let (_, _, len) = f.used_slot(0);
        let entry_inode = {
            let bytes = f.reply_bytes(len as usize);
            u64::from_le_bytes(bytes[16..24].try_into().unwrap())
        };
        let state = f.fs.save_state().unwrap();

        // A fresh device over the same directory restores and keeps serving
        // with the same inode numbers.
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let interrupt = Arc::new(RecInterrupt::default());
        let mut fs2 = InProcessFs::new(
            "fs@test",
            InProcessFsConfig {
                tag: "scratch".into(),
                path: f.dir.as_path().to_path_buf(),
                read_only: false,
            },
        )
        .unwrap();
        fs2.ack_features(fs2.features());
        let mut requests = Queue::new(QUEUE_MAX_SIZE).unwrap();
        requests.set_size(16);
        requests.set_desc_table_address(Some(0x5000), Some(0));
        requests.set_avail_ring_address(Some(0x6000), Some(0));
        requests.set_used_ring_address(Some(0x7000), Some(0));
        requests.set_ready(true);
        let mut hiprio = Queue::new(QUEUE_MAX_SIZE).unwrap();
        hiprio.set_size(16);
        hiprio.set_desc_table_address(Some(0x1000), Some(0));
        hiprio.set_avail_ring_address(Some(0x2000), Some(0));
        hiprio.set_used_ring_address(Some(0x3000), Some(0));
        hiprio.set_ready(true);
        fs2.activate(
            mem.clone(),
            interrupt.clone() as Arc<dyn crate::VirtioInterrupt>,
            vec![hiprio, requests],
        )
        .unwrap();
        fs2.restore_state(&state).unwrap();

        // GETATTR on the restored inode answers through the queue.
        use crate::virtio::fs::fuse::abi::fuse_abi::GetattrIn;
        let getattr = GetattrIn {
            flags: 0,
            dummy: 0,
            fh: 0,
        };
        let mut f2 = Fixture {
            fs: fs2,
            mem,
            desc: 0x5000,
            avail: 0x6000,
            used: 0x7000,
            request: 0x9000,
            reply: 0xA000,
            interrupt,
            submitted: 0,
            dir: f.dir.clone(),
            _kept: f._kept.take(),
        };
        // SAFETY: reading the POD request struct as its exact-size bytes.
        f2.request(Opcode::Getattr, entry_inode, unsafe {
            std::slice::from_raw_parts(
                (&getattr as *const GetattrIn).cast(),
                std::mem::size_of::<GetattrIn>(),
            )
        });
        f2.chain(
            std::mem::size_of::<InHeader>() as u32 + std::mem::size_of::<GetattrIn>() as u32,
            256,
        );
        f2.submit();
        let out = f2.out_header();
        assert_eq!(out.error, 0, "the restored inode still answers");
    }

    #[test]
    fn long_tags_and_missing_directories_are_errors() {
        let long = "x".repeat(37);
        let result = InProcessFs::new(
            "fs@bad",
            InProcessFsConfig {
                tag: long,
                path: std::env::temp_dir(),
                read_only: false,
            },
        )
        .map(|_| ());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("at most 36"), "{err}");

        let result = InProcessFs::new(
            "fs@bad",
            InProcessFsConfig {
                tag: "t".into(),
                path: PathBuf::from("/nonexistent/share-root"),
                read_only: false,
            },
        )
        .map(|_| ());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("share"), "{err}");
    }
}
