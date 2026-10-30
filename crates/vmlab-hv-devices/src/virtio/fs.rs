//! vhost-user-fs front-end (VIRTIO 1.2 §5.11, device id 26).
//!
//! [`Fs`] is a virtio-fs device whose queues are served by an external
//! virtiofsd over vhost-user: the queues live in the backend, and this module
//! is only the front-end — it negotiates features, publishes the guest's
//! shared-memfd RAM to the backend (`SET_MEM_TABLE`), forwards driver kicks to
//! the backend's kick eventfds and turns the backend's call eventfds into
//! interrupts through the transport's [`VirtioInterrupt`].
//!
//! Guest RAM must be backed by the shared memfd the VMM creates for machines
//! with shares; every region's fd and file offset is handed to the backend.
//!
//! Snapshots use the vhost-user device-state transfer
//! (`VHOST_USER_PROTOCOL_F_DEVICE_STATE`): at save the queues are stopped,
//! `GET_VRING_BASE` records the ring positions, virtiofsd serialises its state
//! into a pipe (`SET_DEVICE_STATE_FD`, direction save, phase stopped) and
//! `CHECK_DEVICE_STATE` confirms it, and the queues are re-armed so a running
//! machine continues. Restore replays the reverse into a fresh virtiofsd. The
//! section is an ordinary device-state section the snapshot orchestration
//! stores and restores like any other device's.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crate::{DeviceError, GuestMemory, Snapshot, VirtioDevice, VirtioInterrupt};
use parking_lot::Mutex;
use vhost::vhost_user::message::{
    VhostTransferStateDirection, VhostTransferStatePhase, VhostUserHeaderFlag,
    VhostUserProtocolFeatures, VhostUserVirtioFeatures,
};
use vhost::vhost_user::{Frontend, VhostUserFrontend};
use vhost::{VhostBackend, VhostUserMemoryRegionInfo, VringConfigData};
use virtio_queue::Queue;
use vm_memory::{Address, GuestMemoryBackend, GuestMemoryRegion, MemoryRegionAddress};
use vmm_sys_util::eventfd::EventFd;

/// Virtio device id of virtio-fs (VIRTIO 1.2 §5.11).
pub const FS_DEVICE_TYPE: u32 = 26;

/// `virtio_fs_config::tag` length; tags are zero-padded to it.
const TAG_LEN: usize = 36;
/// Device-specific configuration: the tag, then `num_request_queues`.
const CONFIG_LEN: usize = TAG_LEN + 4;
/// One hiprio queue plus one request queue, virtiofsd's default shape.
const NUM_REQUEST_QUEUES: u32 = 1;
const NUM_QUEUES: usize = 1 + NUM_REQUEST_QUEUES as usize;
/// virtiofsd's queue capacity.
const QUEUE_MAX_SIZE: u16 = 1024;

/// Snapshot format magic: "VUFS".
const SNAPSHOT_MAGIC: u32 = 0x5346_5556;

/// A share to serve: the guest's mount tag and virtiofsd's socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsConfig {
    /// Mount tag the guest uses; at most 36 bytes.
    pub tag: String,
    /// virtiofsd's vhost-user socket.
    pub socket: PathBuf,
}

/// A vhost-user-fs device backed by an external virtiofsd.
pub struct Fs {
    id: String,
    /// Guest-visible device-specific configuration (tag, request queues).
    config: [u8; CONFIG_LEN],
    inner: Mutex<Inner>,
}

/// Everything that talks to the backend; guarded so `Snapshot::save_state`
/// (`&self`) and the transport's calls (`&mut self` through the trait) share
/// one session safely.
struct Inner {
    vu: Frontend,
    /// Backend virtio features offered to the driver, without the
    /// vhost-user protocol bit.
    features: u64,
    acked_features: u64,
    /// Live once `activate` ran; cleared by `reset`.
    active: bool,
    /// Per-queue driver configuration and eventfds, by queue index.
    vrings: Vec<Option<Vring>>,
    /// The call-eventfd worker, present while active.
    worker: Option<WorkerHandle>,
    /// The transport's interrupt handle from `activate`, reused when the
    /// worker restarts around a save.
    interrupt: Option<Arc<dyn VirtioInterrupt>>,
    /// The published memory table: (guest-physical base, size, host address
    /// of the mapping) per region, for guest-to-host translation.
    mem_regions: Vec<(u64, u64, u64)>,
    /// Restored state applied by the next `activate` (the device was
    /// inactive when the section arrived).
    pending: Option<Pending>,
}

/// One live vring: the driver's ring layout plus both eventfds.
struct Vring {
    size: u16,
    desc: u64,
    avail: u64,
    used: u64,
    kick: EventFd,
    call: Arc<EventFd>,
}

/// State restored before the device is active.
struct Pending {
    bases: Vec<u32>,
    device_state: Option<Vec<u8>>,
}

/// The call-eventfd worker's stop switch and waker.
struct WorkerHandle {
    stop: Arc<AtomicBool>,
    wake: EventFd,
    join: JoinHandle<()>,
}

impl WorkerHandle {
    fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.wake.write(1);
        let _ = self.join.join();
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop();
        }
    }
}

impl Fs {
    /// Connect to virtiofsd on `config.socket` and negotiate the protocol.
    ///
    /// The backend must offer `VHOST_USER_PROTOCOL_F_DEVICE_STATE`: the
    /// device's snapshot section is the device-state transfer, and a backend
    /// without it cannot be snapshotted.
    pub fn new(id: impl Into<String>, config: FsConfig) -> Result<Self, DeviceError> {
        let tag = config.tag.as_bytes();
        if tag.len() > TAG_LEN {
            return Err(DeviceError::Other(format!(
                "share tag `{}` is {} bytes; virtio-fs tags are at most {TAG_LEN}",
                config.tag,
                tag.len()
            )));
        }
        let vu = Frontend::connect(&config.socket, NUM_QUEUES as u64).map_err(|e| {
            DeviceError::Other(format!(
                "connecting to virtiofsd socket {}: {e}",
                config.socket.display()
            ))
        })?;
        let mut vu = vu;
        let vhost_err =
            |what: &str, e: vhost::Error| DeviceError::Other(format!("vhost-user {what}: {e}"));
        vu.set_owner().map_err(|e| vhost_err("SET_OWNER", e))?;
        let backend_features = vu
            .get_features()
            .map_err(|e| vhost_err("GET_FEATURES", e))?;
        if backend_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() == 0 {
            return Err(DeviceError::Other(format!(
                "backend on {} does not support vhost-user protocol features",
                config.socket.display()
            )));
        }
        let protocol = vu
            .get_protocol_features()
            .map_err(|e| vhost_err("GET_PROTOCOL_FEATURES", e))?;
        let acked_protocol = protocol
            & (VhostUserProtocolFeatures::REPLY_ACK | VhostUserProtocolFeatures::DEVICE_STATE);
        if !acked_protocol.contains(VhostUserProtocolFeatures::DEVICE_STATE) {
            return Err(DeviceError::Other(format!(
                "backend on {} lacks VHOST_USER_PROTOCOL_F_DEVICE_STATE; \
                 vhost-user-fs state transfer is required for snapshots",
                config.socket.display()
            )));
        }
        vu.set_protocol_features(acked_protocol)
            .map_err(|e| vhost_err("SET_PROTOCOL_FEATURES", e))?;
        // Every request now carries NEED_REPLY, so backend failures surface
        // as errors instead of silently dropped messages.
        vu.set_hdr_flags(VhostUserHeaderFlag::NEED_REPLY);

        let mut config_bytes = [0u8; CONFIG_LEN];
        config_bytes[..tag.len()].copy_from_slice(tag);
        config_bytes[TAG_LEN..].copy_from_slice(&NUM_REQUEST_QUEUES.to_le_bytes());

        Ok(Self {
            id: id.into(),
            config: config_bytes,
            inner: Mutex::new(Inner {
                features: backend_features & !VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits(),
                acked_features: 0,
                active: false,
                vrings: (0..NUM_QUEUES).map(|_| None).collect(),
                worker: None,
                interrupt: None,
                mem_regions: Vec::new(),
                pending: None,
                vu,
            }),
        })
    }
}

impl Inner {
    /// Stop the vrings and the worker; the connection stays for the next
    /// activation.
    fn quiesce(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop();
        }
        for (index, vring) in self.vrings.iter().enumerate() {
            if vring.is_some() {
                let _ = self.vu.set_vring_enable(index, false);
            }
        }
    }

    /// Send one vring's layout and eventfds to the backend.
    fn arm_vring(&mut self, index: usize, base: u32) -> Result<(), DeviceError> {
        let vhost_err =
            |what: &str, e: vhost::Error| DeviceError::Other(format!("vhost-user {what}: {e}"));
        let Some(vring) = self.vrings[index].as_ref() else {
            return Ok(());
        };
        self.vu
            .set_vring_num(index, vring.size)
            .map_err(|e| vhost_err("SET_VRING_NUM", e))?;
        self.vu
            .set_vring_addr(
                index,
                &VringConfigData {
                    queue_max_size: QUEUE_MAX_SIZE,
                    queue_size: vring.size,
                    flags: 0,
                    desc_table_addr: self.host_address(vring.desc)?,
                    used_ring_addr: self.host_address(vring.used)?,
                    avail_ring_addr: self.host_address(vring.avail)?,
                    log_addr: None,
                },
            )
            .map_err(|e| vhost_err("SET_VRING_ADDR", e))?;
        self.vu
            .set_vring_kick(index, &vring.kick)
            .map_err(|e| vhost_err("SET_VRING_KICK", e))?;
        self.vu
            .set_vring_call(index, &vring.call)
            .map_err(|e| vhost_err("SET_VRING_CALL", e))?;
        self.vu
            .set_vring_base(index, base as u16)
            .map_err(|e| vhost_err("SET_VRING_BASE", e))?;
        Ok(())
    }

    /// Enable every armed vring.
    fn enable_vrings(&mut self) -> Result<(), DeviceError> {
        for (index, vring) in self.vrings.iter().enumerate() {
            if vring.is_some() {
                self.vu
                    .set_vring_enable(index, true)
                    .map_err(|e| DeviceError::Other(format!("vhost-user SET_VRING_ENABLE: {e}")))?;
            }
        }
        Ok(())
    }

    /// Publish the guest's shared-memfd RAM to the backend.
    fn set_mem_table(&mut self, mem: &GuestMemory) -> Result<(), DeviceError> {
        let mut regions = Vec::new();
        let mut published = Vec::new();
        // The cloned fds must outlive the call: the table carries raw fds
        // duplicated into the message's ancillary data at send time.
        let mut keep_open = Vec::new();
        for region in mem.iter() {
            if region.len() == 0 {
                continue;
            }
            let file_offset = region.file_offset().ok_or_else(|| {
                DeviceError::Other(
                    "guest RAM is not shared memory; a machine with vhost-user-fs \
                     shares must back RAM with the shared memfd"
                        .into(),
                )
            })?;
            let file = file_offset
                .file()
                .try_clone()
                .map_err(|e| DeviceError::Other(format!("cloning the guest RAM memfd: {e}")))?;
            let host = region
                .get_host_address(MemoryRegionAddress(0))
                .map_err(|e| {
                    DeviceError::Memory(format!("guest RAM region has no host mapping: {e}"))
                })?;
            regions.push(VhostUserMemoryRegionInfo {
                guest_phys_addr: region.start_addr().raw_value(),
                memory_size: region.len(),
                userspace_addr: host as u64,
                mmap_offset: file_offset.start(),
                mmap_handle: file.as_raw_fd(),
            });
            published.push((region.start_addr().raw_value(), region.len(), host as u64));
            keep_open.push(file);
        }
        self.vu
            .set_mem_table(&regions)
            .map(|_| ())
            .map_err(|e| DeviceError::Other(format!("vhost-user SET_MEM_TABLE: {e}")))?;
        self.mem_regions = published;
        Ok(())
    }

    /// Translate a guest-physical address to this process's address, which
    /// is what vhost-user ring addresses are: the backend resolves them
    /// against the `user_addr` base of the region covering the guest
    /// address.
    fn host_address(&self, gpa: u64) -> Result<u64, DeviceError> {
        self.mem_regions
            .iter()
            .find(|(base, size, _)| gpa >= *base && gpa < *base + *size)
            .map(|(base, _, user)| user + (gpa - *base))
            .ok_or_else(|| {
                DeviceError::Memory(format!(
                    "guest address {gpa:#x} is outside the memory published to virtiofsd"
                ))
            })
    }

    /// Ask the backend to serialise its state (direction save, phase stopped)
    /// and return the bytes; `CHECK_DEVICE_STATE` must pass.
    fn save_backend_state(&mut self) -> Result<Vec<u8>, DeviceError> {
        let (mut rx, tx) = pipe().map_err(|e| DeviceError::Other(format!("pipe: {e}")))?;
        // The backend writes into its own copy; the write end crosses the wire.
        let reply = self.transfer(VhostTransferStateDirection::SAVE, tx.into())?;
        let mut state = Vec::new();
        match reply {
            None => {
                rx.read_to_end(&mut state)
                    .map_err(|e| DeviceError::Other(format!("reading device state: {e}")))?;
            }
            // The backend supplied its own fd to read from instead.
            Some(mut file) => {
                file.read_to_end(&mut state)
                    .map_err(|e| DeviceError::Other(format!("reading device state: {e}")))?;
            }
        }
        self.check_state()?;
        Ok(state)
    }

    /// Replay a saved backend state (direction load, phase stopped). The
    /// device must be stopped, which the callers guarantee.
    fn load_backend_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let (rx, tx) = pipe().map_err(|e| DeviceError::Other(format!("pipe: {e}")))?;
        // The backend reads from its own copy; the read end crosses the wire.
        let reply = self.transfer(VhostTransferStateDirection::LOAD, rx.into())?;
        match reply {
            None => {
                let mut sink = tx;
                sink.write_all(state)
                    .map_err(|e| DeviceError::Other(format!("writing device state: {e}")))?;
                // Dropping `sink` closes the pipe: the backend sees EOF.
            }
            Some(mut file) => {
                file.write_all(state)
                    .map_err(|e| DeviceError::Other(format!("writing device state: {e}")))?;
            }
        }
        self.check_state()?;
        Ok(())
    }

    fn transfer(
        &mut self,
        direction: VhostTransferStateDirection,
        fd: OwnedFd,
    ) -> Result<Option<File>, DeviceError> {
        self.vu
            .set_device_state_fd(direction, VhostTransferStatePhase::STOPPED, fd)
            .map_err(|e| {
                DeviceError::Other(format!(
                    "vhost-user SET_DEVICE_STATE_FD ({direction:?}): {e}"
                ))
            })
    }

    fn check_state(&mut self) -> Result<(), DeviceError> {
        self.vu.check_device_state().map_err(|e| {
            DeviceError::State(format!("virtiofsd reported invalid device state: {e}"))
        })
    }

    /// Start the worker that turns call eventfds into transport interrupts,
    /// using the interrupt handle `activate` left behind.
    fn start_worker(&mut self) -> Result<(), DeviceError> {
        let Some(interrupt) = self.interrupt.clone() else {
            return Ok(());
        };
        let stop = Arc::new(AtomicBool::new(false));
        let wake = EventFd::new(0).map_err(|e| DeviceError::Other(format!("eventfd: {e}")))?;
        let calls: Vec<Option<Arc<EventFd>>> = self
            .vrings
            .iter()
            .map(|v| v.as_ref().map(|v| Arc::clone(&v.call)))
            .collect();
        let worker_stop = Arc::clone(&stop);
        let worker_wake = wake
            .try_clone()
            .map_err(|e| DeviceError::Other(format!("eventfd: {e}")))?;
        let join = std::thread::Builder::new()
            .name("vhost-user-fs-call".into())
            .spawn(move || call_worker(worker_stop, worker_wake, calls, interrupt))
            .map_err(|e| DeviceError::Other(format!("spawning the call worker: {e}")))?;
        self.worker = Some(WorkerHandle { stop, wake, join });
        Ok(())
    }
}

/// Poll the wake fd and every queue's call fd; a readable call fd becomes a
/// queue interrupt through the transport.
fn call_worker(
    stop: Arc<AtomicBool>,
    wake: EventFd,
    calls: Vec<Option<Arc<EventFd>>>,
    interrupt: Arc<dyn VirtioInterrupt>,
) {
    let mut fds: Vec<libc::pollfd> = std::iter::once(libc::pollfd {
        fd: wake.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    })
    .chain(calls.iter().map(|call| libc::pollfd {
        fd: call.as_ref().map(|c| c.as_raw_fd()).unwrap_or(-1),
        events: libc::POLLIN,
        revents: 0,
    }))
    .collect();
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        // SAFETY: `fds` is a well-formed pollfd array of length `fds.len()`;
        // poll(2) only reads it and writes the `revents` fields back.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 500) };
        if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            let _ = wake.read();
            if stop.load(Ordering::SeqCst) {
                return;
            }
        }
        for (index, call) in calls.iter().enumerate() {
            if fds[index + 1].revents & libc::POLLIN == 0 {
                continue;
            }
            if let Some(call) = call {
                let _ = call.read();
                interrupt.notify_queue(index as u16);
            }
        }
    }
}

/// A `pipe2(O_CLOEXEC)` pair as owned files.
fn pipe() -> std::io::Result<(File, File)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` points to two writable c_ints; pipe2(2) fills them or
    // returns -1 with errno set.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both fds are fresh, owned descriptors from pipe2 above.
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

impl VirtioDevice for Fs {
    fn device_type(&self) -> u32 {
        FS_DEVICE_TYPE
    }

    fn queue_max_sizes(&self) -> &[u16] {
        const MAX_SIZES: [u16; NUM_QUEUES] = [QUEUE_MAX_SIZE; NUM_QUEUES];
        &MAX_SIZES
    }

    fn features(&self) -> u64 {
        self.inner.lock().features
    }

    fn ack_features(&mut self, features: u64) {
        // Set semantics: the transport hands over the whole negotiated set.
        let mut inner = self.inner.lock();
        inner.acked_features = features & inner.features;
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
        queues: Vec<Queue>,
    ) -> Result<(), DeviceError> {
        let mut inner = self.inner.lock();
        if inner.active {
            return Ok(());
        }
        // The protocol-features bit rides along so the backend enables
        // protocol messages (SET_VRING_ENABLE among them).
        inner
            .vu
            .set_features(inner.acked_features | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits())
            .map_err(|e| DeviceError::Other(format!("vhost-user SET_FEATURES: {e}")))?;
        inner.set_mem_table(&mem)?;

        let pending = inner.pending.take();
        for (index, queue) in queues.into_iter().enumerate() {
            let state = queue.state();
            if index >= inner.vrings.len() || !state.ready {
                continue;
            }
            inner.vrings[index] = Some(Vring {
                size: state.size,
                desc: state.desc_table,
                avail: state.avail_ring,
                used: state.used_ring,
                kick: EventFd::new(0).map_err(|e| DeviceError::Other(format!("eventfd: {e}")))?,
                call: Arc::new(
                    EventFd::new(0).map_err(|e| DeviceError::Other(format!("eventfd: {e}")))?,
                ),
            });
        }
        let bases = pending
            .as_ref()
            .map(|p| p.bases.clone())
            .unwrap_or_else(|| vec![0; inner.vrings.len()]);
        for index in 0..inner.vrings.len() {
            inner.arm_vring(index, bases.get(index).copied().unwrap_or(0))?;
        }
        // The state transfer wants the device stopped: load before enabling.
        if let Some(pending) = &pending
            && let Some(state) = &pending.device_state
        {
            inner.load_backend_state(state)?;
        }
        inner.enable_vrings()?;
        inner.interrupt = Some(interrupt);
        inner.start_worker()?;
        inner.active = true;
        Ok(())
    }

    fn queue_notify(&mut self, index: u16) {
        let inner = self.inner.lock();
        if !inner.active {
            return;
        }
        if let Some(Some(vring)) = inner.vrings.get(index as usize) {
            // The kick eventfd never blocks; a dead backend surfaces as the
            // guest timing out on the share.
            let _ = vring.kick.write(1);
        }
    }

    fn reset(&mut self) {
        let mut inner = self.inner.lock();
        inner.quiesce();
        inner.acked_features = 0;
        inner.vrings = (0..NUM_QUEUES).map(|_| None).collect();
        inner.interrupt = None;
        inner.pending = None;
        inner.active = false;
    }
}

/// Snapshot layout: magic, version, active flag, acked features, then (when
/// active) the queue count with one ring base each, and the backend's
/// device-state bytes. The transport restores the driver-written
/// configuration; this section restores the backend.
impl Snapshot for Fs {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut inner = self.inner.lock();
        let mut out = Vec::with_capacity(24);
        out.extend_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        out.push(1); // format version
        if !inner.active {
            out.push(0);
            out.extend_from_slice(&0u64.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            return Ok(out);
        }
        // The callers pause the vCPUs first, so no kick can race the stop.
        inner.quiesce();
        let mut bases = Vec::with_capacity(inner.vrings.len());
        for index in 0..inner.vrings.len() {
            bases.push(
                inner
                    .vu
                    .get_vring_base(index)
                    .map_err(|e| DeviceError::State(format!("vhost-user GET_VRING_BASE: {e}")))?,
            );
        }
        let state = inner.save_backend_state()?;
        // The machine continues after the snapshot: re-arm with the saved
        // bases, exactly where the queues stopped.
        for (index, base) in bases.iter().enumerate() {
            inner.arm_vring(index, *base)?;
        }
        inner.enable_vrings()?;
        inner.start_worker()?;

        out.push(1);
        out.extend_from_slice(&inner.acked_features.to_le_bytes());
        out.extend_from_slice(&(bases.len() as u16).to_le_bytes());
        for base in &bases {
            out.extend_from_slice(&base.to_le_bytes());
        }
        out.extend_from_slice(&(state.len() as u32).to_le_bytes());
        out.extend_from_slice(&state);
        Ok(out)
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let bad = || DeviceError::State(format!("{}: bad vhost-user-fs state", self.id));
        if state.len() < 16 || u32::from_le_bytes(state[0..4].try_into().unwrap()) != SNAPSHOT_MAGIC
        {
            return Err(bad());
        }
        if state[4] != 1 {
            return Err(DeviceError::State(format!(
                "{}: vhost-user-fs state version {} is not 1",
                self.id, state[4]
            )));
        }
        let mut cur: usize = 5;
        let mut take = |len: usize| -> Result<&[u8], DeviceError> {
            let end = cur.checked_add(len).ok_or_else(bad)?;
            let slice = state.get(cur..end).ok_or_else(bad)?;
            cur = end;
            Ok(slice)
        };
        let u32at = |s: &[u8]| -> Result<u32, DeviceError> {
            s.try_into().map(u32::from_le_bytes).map_err(|_| bad())
        };

        let active = take(1)?[0] != 0;
        let acked = u64::from_le_bytes(take(8)?.try_into().map_err(|_| bad())?);
        let n = u16::from_le_bytes(take(2)?.try_into().map_err(|_| bad())?) as usize;
        if !active {
            if n != 0 || cur != state.len() {
                return Err(bad());
            }
            let mut inner = self.inner.lock();
            inner.acked_features = acked & inner.features;
            inner.pending = None;
            return Ok(());
        }
        if n != NUM_QUEUES {
            return Err(DeviceError::State(format!(
                "{}: vhost-user-fs state has {n} queues, device has {NUM_QUEUES}",
                self.id
            )));
        }
        let mut bases = Vec::with_capacity(n);
        for _ in 0..n {
            bases.push(u32at(take(4)?)?);
        }
        let len = u32at(take(4)?)? as usize;
        let blob = take(len)?;
        if cur != state.len() {
            return Err(bad());
        }
        let blob = (!blob.is_empty()).then(|| blob.to_vec());

        let mut inner = self.inner.lock();
        inner.acked_features = acked & inner.features;
        if inner.active {
            // The transport restored before the device (its `restore_state`
            // re-activates with the driver's queue layout): replay the bases
            // and the backend state into the running session.
            inner.quiesce();
            for (index, base) in bases.iter().enumerate() {
                inner.arm_vring(index, *base)?;
            }
            if let Some(state) = &blob {
                inner.load_backend_state(state)?;
            }
            inner.enable_vrings()?;
            inner.start_worker()?;
        } else {
            inner.pending = Some(Pending {
                bases,
                device_state: blob,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::VirtioDevice;
    use virtio_queue::QueueT;
    use vm_memory::{FileOffset, GuestAddress, GuestMemoryMmap};

    /// Bits [0..2] of the header flags carry the protocol version (1); bit 2
    /// marks a reply.
    const VERSION: u32 = 0x1;
    const REPLY: u32 = 0x4;
    /// NEED_REPLY flag the front-end sets on every request.
    const NEED_REPLY: u32 = 0x8;
    /// What the fake answers GET_VRING_BASE with.
    const FAKE_VRING_BASE: u32 = 7;

    /// Everything the fake backend recorded.
    #[derive(Default, Debug)]
    struct BackendLog {
        set_features: Vec<u64>,
        protocol_features: Option<u64>,
        /// (guest_phys_addr, memory_size, mmap_offset) per region.
        mem_regions: Vec<(u64, u64, u64)>,
        mem_fd_count: usize,
        vring_num: BTreeMap<usize, u16>,
        vring_addr: BTreeMap<usize, (u64, u64, u64)>,
        vring_base: BTreeMap<usize, u16>,
        kick_queues: Vec<usize>,
        call_queues: Vec<usize>,
        enabled: Vec<(usize, bool)>,
        disabled: Vec<usize>,
        device_state_saves: usize,
        /// The bytes each load transfer delivered.
        device_state_loads: Vec<Vec<u8>>,
        check_device_state: usize,
        kicks: BTreeMap<usize, u64>,
        unexpected: Vec<String>,
    }

    /// A scripted vhost-user backend: one connection, answered inline.
    struct FakeBackend {
        log: Arc<Mutex<BackendLog>>,
        listener: UnixListener,
        socket: PathBuf,
        join: Option<JoinHandle<()>>,
        features: u64,
        protocol: VhostUserProtocolFeatures,
        save_state: Vec<u8>,
    }

    impl FakeBackend {
        /// Listen on a fresh socket; nothing is served until [`serve`].
        fn new(tag: &str) -> Self {
            let socket = std::env::temp_dir().join(format!(
                "vmlab-hv-fs-test-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let listener = UnixListener::bind(&socket).expect("bind fake backend socket");
            Self {
                log: Arc::new(Mutex::new(BackendLog::default())),
                listener,
                socket,
                join: None,
                features: (1 << 32) | (1 << 29) | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits(),
                protocol: VhostUserProtocolFeatures::REPLY_ACK
                    | VhostUserProtocolFeatures::DEVICE_STATE,
                save_state: b"virtiofsd-state-bytes".to_vec(),
            }
        }

        fn config(&self) -> FsConfig {
            FsConfig {
                tag: "scratch".into(),
                socket: self.socket.clone(),
            }
        }

        /// Accept one connection (in the serving thread) and serve it until
        /// the front-end hangs up.
        fn serve(mut self) -> Self {
            let log = Arc::clone(&self.log);
            let features = self.features;
            let protocol = self.protocol;
            let save_state = self.save_state.clone();
            let listener = self.listener.try_clone().expect("clone listener");
            self.join = Some(
                std::thread::Builder::new()
                    .name("fake-backend".into())
                    .spawn(move || {
                        let (stream, _) = listener.accept().expect("accept front-end");
                        serve_connection(stream, log, features, protocol, save_state);
                    })
                    .expect("spawn fake backend"),
            );
            self
        }

        /// Wait for the serving thread and hand back what it saw.
        fn join(mut self) -> Arc<Mutex<BackendLog>> {
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
            Arc::clone(&self.log)
        }
    }

    impl Drop for FakeBackend {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    /// One request as the fake sees it.
    struct Received {
        code: u32,
        flags: u32,
        payload: Vec<u8>,
        fds: Vec<OwnedFd>,
    }

    /// Receive one vhost-user message: a 12-byte header, any fds riding
    /// along in SCM_RIGHTS, then the payload.
    fn recv_request(stream: &mut UnixStream) -> Option<Received> {
        let mut header = [0u8; 12];
        let mut control = [0u8; 256];
        let mut iov = [libc::iovec {
            iov_base: header.as_mut_ptr().cast(),
            iov_len: header.len(),
        }];
        let mut msg = libc::msghdr {
            msg_name: std::ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: iov.as_mut_ptr(),
            msg_iovlen: 1,
            msg_control: control.as_mut_ptr().cast(),
            msg_controllen: control.len(),
            msg_flags: 0,
        };
        // SAFETY: recvmsg(2) with a well-formed msghdr pointing at buffers
        // that outlive the call; it fills them and returns the byte count.
        let count = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
        if count == 0 {
            return None; // the front-end hung up
        }
        assert_eq!(count, 12, "short vhost-user header");
        let mut fds = Vec::new();
        // SAFETY: msg's control buffer holds at most the cmsghdrs recvmsg
        // wrote; the walk uses the kernel's own length bookkeeping.
        let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        while !cmsg.is_null() {
            // SAFETY: cmsg points inside the control buffer written above.
            let (level, kind, len) =
                unsafe { ((*cmsg).cmsg_level, (*cmsg).cmsg_type, (*cmsg).cmsg_len) };
            if level == libc::SOL_SOCKET && kind == libc::SCM_RIGHTS {
                let data_len = len - std::mem::size_of::<libc::cmsghdr>();
                let count = data_len / std::mem::size_of::<RawFd>();
                // SAFETY: CMSG_DATA points at `count` raw fds in the buffer.
                let data = unsafe { libc::CMSG_DATA(cmsg) }.cast::<RawFd>();
                for i in 0..count {
                    // SAFETY: the fd arrived via SCM_RIGHTS and is owned by
                    // us from here on.
                    fds.push(unsafe { OwnedFd::from_raw_fd(data.add(i).read()) });
                }
            }
            // SAFETY: both pointers stay inside `msg`'s control buffer.
            cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
        }
        let code = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let flags = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let size = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; size];
        if !payload.is_empty() {
            stream.read_exact(&mut payload).expect("payload");
        }
        Some(Received {
            code,
            flags,
            payload,
            fds,
        })
    }

    fn send_u64_reply(stream: &mut UnixStream, code: u32, value: u64) {
        send_reply_bytes(stream, code, &value.to_le_bytes());
    }

    fn send_reply_bytes(stream: &mut UnixStream, code: u32, body: &[u8]) {
        let mut message = Vec::with_capacity(12 + body.len());
        message.extend_from_slice(&code.to_le_bytes());
        message.extend_from_slice(&(VERSION | REPLY).to_le_bytes());
        message.extend_from_slice(&(body.len() as u32).to_le_bytes());
        message.extend_from_slice(body);
        stream.write_all(&message).expect("send reply");
    }

    /// vhost-user request codes the device sends (the enum is not public).
    mod req {
        pub const SET_OWNER: u32 = 3;
        pub const GET_FEATURES: u32 = 1;
        pub const SET_FEATURES: u32 = 2;
        pub const GET_PROTOCOL_FEATURES: u32 = 15;
        pub const SET_PROTOCOL_FEATURES: u32 = 16;
        pub const SET_MEM_TABLE: u32 = 5;
        pub const SET_VRING_NUM: u32 = 8;
        pub const SET_VRING_ADDR: u32 = 9;
        pub const SET_VRING_BASE: u32 = 10;
        pub const SET_VRING_KICK: u32 = 12;
        pub const SET_VRING_CALL: u32 = 13;
        pub const SET_VRING_ENABLE: u32 = 18;
        pub const GET_VRING_BASE: u32 = 11;
        pub const SET_DEVICE_STATE_FD: u32 = 42;
        pub const CHECK_DEVICE_STATE: u32 = 43;
    }

    fn serve_connection(
        mut stream: UnixStream,
        log: Arc<Mutex<BackendLog>>,
        features: u64,
        protocol: VhostUserProtocolFeatures,
        save_state: Vec<u8>,
    ) {
        while let Some(received) = recv_request(&mut stream) {
            let u64_payload = |r: &Received| u64::from_le_bytes(r.payload[..8].try_into().unwrap());
            match received.code {
                req::SET_OWNER => {}
                req::GET_FEATURES => {
                    send_u64_reply(&mut stream, req::GET_FEATURES, features);
                    continue;
                }
                req::SET_FEATURES => {
                    log.lock().set_features.push(u64_payload(&received));
                }
                req::GET_PROTOCOL_FEATURES => {
                    send_u64_reply(&mut stream, req::GET_PROTOCOL_FEATURES, protocol.bits());
                    continue;
                }
                req::SET_PROTOCOL_FEATURES => {
                    log.lock().protocol_features = Some(u64_payload(&received));
                }
                req::SET_MEM_TABLE => {
                    let count =
                        u32::from_le_bytes(received.payload[0..4].try_into().unwrap()) as usize;
                    let mut guard = log.lock();
                    guard.mem_fd_count = received.fds.len();
                    for i in 0..count {
                        let region = &received.payload[8 + i * 32..8 + (i + 1) * 32];
                        let field =
                            |k: usize| u64::from_le_bytes(region[k..k + 8].try_into().unwrap());
                        guard.mem_regions.push((field(0), field(8), field(24)));
                    }
                }
                req::SET_VRING_NUM => {
                    let index =
                        u32::from_le_bytes(received.payload[0..4].try_into().unwrap()) as usize;
                    let num = u32::from_le_bytes(received.payload[4..8].try_into().unwrap()) as u16;
                    log.lock().vring_num.insert(index, num);
                }
                req::SET_VRING_ADDR => {
                    let index =
                        u32::from_le_bytes(received.payload[0..4].try_into().unwrap()) as usize;
                    let field = |k: usize| {
                        u64::from_le_bytes(received.payload[k..k + 8].try_into().unwrap())
                    };
                    log.lock()
                        .vring_addr
                        .insert(index, (field(8), field(24), field(16)));
                }
                req::SET_VRING_BASE => {
                    let index =
                        u32::from_le_bytes(received.payload[0..4].try_into().unwrap()) as usize;
                    let num = u32::from_le_bytes(received.payload[4..8].try_into().unwrap()) as u16;
                    log.lock().vring_base.insert(index, num);
                }
                req::SET_VRING_KICK => {
                    let queue = (u64_payload(&received) & 0xff) as usize;
                    log.lock().kick_queues.push(queue);
                    watch_kick(&log, queue, received.fds.into_iter().next().unwrap());
                }
                req::SET_VRING_CALL => {
                    let queue = (u64_payload(&received) & 0xff) as usize;
                    log.lock().call_queues.push(queue);
                    // The fd stays open until it goes out of scope here; the
                    // fake never signals calls, so nothing observes it.
                }
                req::SET_VRING_ENABLE => {
                    let index =
                        u32::from_le_bytes(received.payload[0..4].try_into().unwrap()) as usize;
                    let enable =
                        u32::from_le_bytes(received.payload[4..8].try_into().unwrap()) != 0;
                    let mut guard = log.lock();
                    if enable {
                        guard.enabled.push((index, true));
                    } else {
                        guard.disabled.push(index);
                    }
                }
                req::GET_VRING_BASE => {
                    let index = u32::from_le_bytes(received.payload[0..4].try_into().unwrap());
                    let mut body = Vec::with_capacity(8);
                    body.extend_from_slice(&index.to_le_bytes());
                    body.extend_from_slice(&FAKE_VRING_BASE.to_le_bytes());
                    send_reply_bytes(&mut stream, req::GET_VRING_BASE, &body);
                    continue;
                }
                req::SET_DEVICE_STATE_FD => {
                    let direction = u32::from_le_bytes(received.payload[0..4].try_into().unwrap());
                    // Reply "the fd you passed is the one I use" (no fd back).
                    send_u64_reply(&mut stream, req::SET_DEVICE_STATE_FD, 0x100);
                    let fd = received.fds.into_iter().next().unwrap();
                    if direction == u32::from(VhostTransferStateDirection::SAVE) {
                        // save: the backend writes its state into the fd it
                        // received, then closes it for the front-end's EOF
                        log.lock().device_state_saves += 1;
                        let state = save_state.clone();
                        let _ = std::thread::spawn(move || {
                            let mut file = File::from(fd);
                            let _ = file.write_all(&state);
                        });
                    } else {
                        // load: the backend reads until the front-end closes
                        log.lock().device_state_loads.push(Vec::new());
                        let slot = log.lock().device_state_loads.len() - 1;
                        let log = Arc::clone(&log);
                        let _ = std::thread::spawn(move || {
                            let mut bytes = Vec::new();
                            let mut file = File::from(fd);
                            let _ = file.read_to_end(&mut bytes);
                            log.lock().device_state_loads[slot] = bytes;
                        });
                    }
                    continue;
                }
                req::CHECK_DEVICE_STATE => {
                    log.lock().check_device_state += 1;
                }
                other => {
                    log.lock()
                        .unexpected
                        .push(format!("unexpected message code {other}"));
                }
            }
            if received.flags & NEED_REPLY != 0 {
                send_u64_reply(&mut stream, received.code, 0);
            }
        }
    }

    /// Count eventfd writes the backend's kick fd receives, per queue.
    fn watch_kick(log: &Arc<Mutex<BackendLog>>, queue: usize, fd: OwnedFd) {
        let log = Arc::clone(log);
        let _ = std::thread::spawn(move || {
            let mut file = File::from(fd);
            let mut counter = [0u8; 8];
            while let Ok(()) = file.read_exact(&mut counter) {
                let count = u64::from_le_bytes(counter);
                let mut guard = log.lock();
                let total = guard.kicks.get(&queue).copied().unwrap_or(0);
                guard.kicks.insert(queue, total + count);
            }
        });
    }

    /// A memfd-backed guest memory shaped like the VMM's shared RAM: two
    /// regions, each mapped at file offset equal to its guest-physical base.
    fn shared_guest_memory() -> GuestMemoryMmap {
        const REGION: u64 = 4 << 20;
        // SAFETY: memfd_create(2) with a NUL-terminated name; the return is
        // either a valid fd or -1 with errno set.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_memfd_create,
                c"fs-test-ram".as_ptr(),
                1, /* CLOEXEC */
            )
        };
        assert!(fd >= 0, "memfd_create: {}", std::io::Error::last_os_error());
        // SAFETY: the fd is fresh and owned from here on.
        let memfd = unsafe { File::from_raw_fd(fd as RawFd) };
        memfd.set_len(0x1_0000_0000 + REGION).expect("size memfd");
        let mem = GuestMemoryMmap::from_ranges_with_files([
            (
                GuestAddress(0),
                REGION as usize,
                Some(FileOffset::new(memfd.try_clone().unwrap(), 0)),
            ),
            (
                GuestAddress(0x1_0000_0000),
                REGION as usize,
                Some(FileOffset::new(memfd.try_clone().unwrap(), 0x1_0000_0000)),
            ),
        ])
        .expect("guest memory");
        std::mem::forget(memfd); // keep the mapping alive for the test
        mem
    }

    /// Two ready queues with distinct, in-memory ring addresses.
    fn ready_queues(mem: &GuestMemoryMmap, size: u16) -> Vec<Queue> {
        let layout = |n: usize| -> (u64, u64, u64) {
            // desc: n * 16 bytes, avail: 6 + 2*n (+flags), used: 6 + 8*n,
            // each ring start 4-aligned as virtio-queue's setters require.
            let align4 = |x: u64| (x + 3) & !3;
            let base = 0x1000 + (n as u64) * 0x10000;
            let desc = base;
            let avail = align4(desc + (size as u64) * 16);
            let used = align4(avail + 6 + 2 * size as u64);
            (desc, avail, used)
        };
        (0..NUM_QUEUES)
            .map(|n| {
                let (desc, avail, used) = layout(n);
                let mut queue = Queue::new(QUEUE_MAX_SIZE).expect("queue");
                queue.set_size(size);
                queue.set_desc_table_address(Some(desc as u32), Some((desc >> 32) as u32));
                queue.set_avail_ring_address(Some(avail as u32), Some((avail >> 32) as u32));
                queue.set_used_ring_address(Some(used as u32), Some((used >> 32) as u32));
                queue.set_ready(true);
                let _ = mem;
                queue
            })
            .collect()
    }

    /// Records queue notifications.
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

    /// Poll `probe` until it returns true, or panic after `secs`.
    fn eventually(secs: u64, probe: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if probe() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("condition not met within {secs}s");
    }

    #[test]
    fn config_space_and_identity() {
        let backend = FakeBackend::new("cfg").serve();
        let fs = Fs::new("fs-scratch", backend.config()).unwrap();
        assert_eq!(fs.device_type(), 26);
        assert_eq!(fs.queue_max_sizes(), &[QUEUE_MAX_SIZE, QUEUE_MAX_SIZE]);
        assert_eq!(fs.config_len(), 40);
        // The backend's virtio features, without the vhost-user protocol bit.
        assert_eq!(fs.features(), (1 << 32) | (1 << 29));

        let mut data = [0u8; 40];
        fs.read_config(0, &mut data);
        let tag = "scratch".as_bytes();
        assert_eq!(&data[..tag.len()], tag);
        assert!(data[tag.len()..36].iter().all(|b| *b == 0));
        assert_eq!(
            u32::from_le_bytes(data[36..40].try_into().unwrap()),
            NUM_REQUEST_QUEUES
        );
        // Reads past the tag and count zero-fill.
        let mut tail = [0xffu8; 8];
        fs.read_config(36, &mut tail);
        assert_eq!(&tail[..4], &NUM_REQUEST_QUEUES.to_le_bytes());
        assert_eq!(&tail[4..], &[0, 0, 0, 0]);
        let mut beyond = [0xffu8; 4];
        fs.read_config(40, &mut beyond);
        assert_eq!(beyond, [0; 4]);

        drop(fs); // close the front-end so the fake's loop ends
        let log = backend.join();
        let log = log.lock();
        assert!(log.unexpected.is_empty(), "{:?}", log.unexpected);
        assert_eq!(log.protocol_features, Some(0x8 | 0x8_0000));
    }

    #[test]
    fn tag_longer_than_the_config_is_rejected() {
        let backend = FakeBackend::new("longtag");
        let long = "x".repeat(37);
        let config = FsConfig {
            tag: long,
            socket: backend.socket.clone(),
        };
        let err = match Fs::new("fs-long", config) {
            Err(e) => e,
            Ok(_) => panic!("a 37-byte tag must be rejected"),
        };
        assert!(err.to_string().contains("at most 36"), "{err}");
    }

    #[test]
    fn missing_socket_is_a_clear_error() {
        let config = FsConfig {
            tag: "scratch".into(),
            socket: PathBuf::from("/nonexistent/virtiofsd.sock"),
        };
        let err = match Fs::new("fs-x", config) {
            Err(e) => e,
            Ok(_) => panic!("a missing socket must be an error"),
        };
        assert!(err.to_string().contains("virtiofsd socket"), "{err}");
    }

    #[test]
    fn backend_without_device_state_is_refused() {
        let mut backend = FakeBackend::new("nodstate");
        backend.protocol = VhostUserProtocolFeatures::REPLY_ACK;
        let backend = backend.serve();
        let err = match Fs::new("fs-x", backend.config()) {
            Err(e) => e,
            Ok(_) => panic!("a backend without DEVICE_STATE must be refused"),
        };
        assert!(
            err.to_string()
                .contains("VHOST_USER_PROTOCOL_F_DEVICE_STATE"),
            "{err}"
        );
    }

    #[test]
    fn activation_sets_up_queues_and_memory() {
        let backend = FakeBackend::new("activate").serve();
        let mem = shared_guest_memory();
        let mut fs = Fs::new("fs-scratch", backend.config()).unwrap();
        fs.ack_features((1 << 32) | (1 << 29) | (1 << 62)); // bit 62 is not offered: masked
        fs.activate(
            mem.clone(),
            Arc::new(RecInterrupt::default()),
            ready_queues(&mem, 128),
        )
        .unwrap();
        drop(fs); // close the front-end so the fake's loop ends

        let log = backend.join();
        let log = log.lock();
        assert!(log.unexpected.is_empty(), "{:?}", log.unexpected);
        // SET_FEATURES carries the acked set plus the protocol-features bit.
        assert_eq!(
            log.set_features,
            vec![(1 << 32) | (1 << 29) | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()]
        );
        // Both RAM regions crossed with their fds, file offset = guest base.
        assert_eq!(log.mem_fd_count, 2);
        assert_eq!(
            log.mem_regions,
            vec![(0, 4 << 20, 0), (0x1_0000_0000, 4 << 20, 0x1_0000_0000)]
        );
        // Both queues: num, addresses, kick and call fds, base 0, enabled.
        assert_eq!(log.vring_num, BTreeMap::from([(0, 128), (1, 128)]));
        // The ring addresses the backend receives are this process's
        // addresses, translated through the region's host mapping.
        let host_base = {
            use vm_memory::GuestMemoryBackend;
            mem.iter()
                .next()
                .map(|r| r.get_host_address(MemoryRegionAddress(0)).unwrap() as u64)
                .unwrap()
        };
        for (queue, (desc, avail, used)) in &log.vring_addr {
            let base = host_base + 0x1000 + *queue as u64 * 0x10000;
            assert_eq!(*desc, base);
            assert_eq!(*avail, base + 128 * 16);
            // 6 header bytes + 2*128 entries, 4-aligned like the layout.
            assert_eq!(*used, base + 128 * 16 + 6 + 2 * 128 + 2);
        }
        assert_eq!(log.vring_addr.len(), 2);
        assert_eq!(log.kick_queues, vec![0, 1]);
        assert_eq!(log.call_queues, vec![0, 1]);
        assert_eq!(log.vring_base, BTreeMap::from([(0, 0), (1, 0)]));
        assert_eq!(log.enabled, vec![(0, true), (1, true)]);
        assert_eq!(log.device_state_saves, 0);
        assert!(log.device_state_loads.is_empty());
    }

    #[test]
    fn notify_kicks_the_backend() {
        let backend = FakeBackend::new("kick").serve();
        let mem = shared_guest_memory();
        let mut fs = Fs::new("fs-scratch", backend.config()).unwrap();
        fs.ack_features(1 << 32);
        fs.activate(
            mem.clone(),
            Arc::new(RecInterrupt::default()),
            ready_queues(&mem, 128),
        )
        .unwrap();
        fs.queue_notify(1);
        fs.queue_notify(1);
        let log = Arc::clone(&backend.log);
        eventually(2, || log.lock().kicks.get(&1).copied().unwrap_or(0) >= 2);
        // A reset device ignores notifications.
        fs.reset();
        fs.queue_notify(1);
        drop(fs); // close the front-end so the fake's loop ends
        let log = backend.join();
        assert_eq!(log.lock().kicks.get(&1).copied().unwrap_or(0), 2);
    }

    #[test]
    fn snapshot_round_trip_through_the_backend() {
        let backend = FakeBackend::new("save").serve();
        let mem = shared_guest_memory();
        let mut fs = Fs::new("fs-scratch", backend.config()).unwrap();
        fs.ack_features(1 << 32);
        fs.activate(
            mem.clone(),
            Arc::new(RecInterrupt::default()),
            ready_queues(&mem, 128),
        )
        .unwrap();

        let section = {
            use crate::Snapshot;
            fs.save_state().unwrap()
        };
        // The section carries the backend's state bytes and the fake's ring
        // bases, and the running device was re-armed afterwards.
        {
            let log = backend.log.lock();
            assert_eq!(log.device_state_saves, 1);
            assert_eq!(log.check_device_state, 1);
            assert_eq!(log.disabled, vec![0, 1]);
            let enables = log.enabled.iter().filter(|(i, _)| *i < 2).count();
            assert_eq!(enables, 4, "re-armed after save: {log:?}");
        }
        assert!(
            section
                .windows(b"virtiofsd-state-bytes".len())
                .any(|w| w == b"virtiofsd-state-bytes")
        );

        // Restore into a fresh device on a fresh backend: inactive at first,
        // so the state applies at the next activation.
        let backend2 = FakeBackend::new("load").serve();
        let mem2 = shared_guest_memory();
        let mut fs2 = Fs::new("fs-scratch", backend2.config()).unwrap();
        {
            use crate::Snapshot;
            fs2.restore_state(&section).unwrap();
        }
        assert_eq!(fs2.features(), (1 << 32) | (1 << 29));
        fs2.ack_features(1 << 32);
        fs2.activate(
            mem2.clone(),
            Arc::new(RecInterrupt::default()),
            ready_queues(&mem2, 128),
        )
        .unwrap();
        // The second backend received the same bytes by load and the saved
        // ring bases.
        let log2 = Arc::clone(&backend2.log);
        eventually(2, || {
            log2.lock()
                .device_state_loads
                .first()
                .is_some_and(|bytes| !bytes.is_empty())
        });
        drop(fs2); // close the front-end so the fake's loop ends
        drop(fs);
        let log2 = backend2.join();
        let log2 = log2.lock();
        assert!(log2.unexpected.is_empty(), "{:?}", log2.unexpected);
        assert_eq!(
            log2.device_state_loads,
            vec![b"virtiofsd-state-bytes".to_vec()]
        );
        assert_eq!(log2.check_device_state, 1);
        assert_eq!(log2.vring_base, BTreeMap::from([(0, 7), (1, 7)]));
        assert_eq!(log2.enabled, vec![(0, true), (1, true)]);
    }

    #[test]
    fn restore_replays_into_an_active_device() {
        // Save from one device...
        let backend = FakeBackend::new("save2").serve();
        let mem = shared_guest_memory();
        let mut fs = Fs::new("fs-scratch", backend.config()).unwrap();
        fs.ack_features(1 << 32);
        fs.activate(
            mem.clone(),
            Arc::new(RecInterrupt::default()),
            ready_queues(&mem, 128),
        )
        .unwrap();
        let section = {
            use crate::Snapshot;
            fs.save_state().unwrap()
        };

        // ...restore into a device the transport already activated.
        let backend2 = FakeBackend::new("load2").serve();
        let mem2 = shared_guest_memory();
        let mut fs2 = Fs::new("fs-scratch", backend2.config()).unwrap();
        fs2.ack_features(1 << 32);
        fs2.activate(
            mem2.clone(),
            Arc::new(RecInterrupt::default()),
            ready_queues(&mem2, 128),
        )
        .unwrap();
        {
            use crate::Snapshot;
            fs2.restore_state(&section).unwrap();
        }
        let log2 = Arc::clone(&backend2.log);
        eventually(2, || {
            log2.lock()
                .device_state_loads
                .first()
                .is_some_and(|bytes| !bytes.is_empty())
        });
        drop(fs2); // close the front-end so the fake's loop ends
        drop(fs);
        let log2 = backend2.join();
        let log2 = log2.lock();
        assert_eq!(
            log2.device_state_loads,
            vec![b"virtiofsd-state-bytes".to_vec()]
        );
        // The fresh session was quiesced, re-based and re-enabled.
        assert_eq!(log2.vring_base, BTreeMap::from([(0, 7), (1, 7)]));
        assert_eq!(log2.disabled, vec![0, 1]);
    }

    #[test]
    fn inactive_device_snapshots_to_an_empty_section() {
        let backend = FakeBackend::new("inactive").serve();
        let fs = Fs::new("fs-scratch", backend.config()).unwrap();
        let section = {
            use crate::Snapshot;
            fs.save_state().unwrap()
        };
        assert_eq!(section.len(), 16); // magic + version + flag + features + count
        drop(fs);
        // Restoring it into another inactive device is a no-op.
        let backend2 = FakeBackend::new("inactive2").serve();
        let mut fs2 = Fs::new("fs-scratch", backend2.config()).unwrap();
        {
            use crate::Snapshot;
            fs2.restore_state(&section).unwrap();
        }
        // A truncated or garbage section is refused.
        assert!(fs2.restore_state(&section[..section.len() - 1]).is_err());
        assert!(fs2.restore_state(&[0u8; 20]).is_err());
        drop(fs2);
    }

    #[test]
    fn state_id_is_the_share_id() {
        let backend = FakeBackend::new("id").serve();
        let fs = Fs::new("fs-scratch", backend.config()).unwrap();
        assert_eq!(fs.state_id(), "fs-scratch");
    }
}
