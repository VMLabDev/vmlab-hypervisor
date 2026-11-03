//! virtio-net (VIRTIO 1.2 §5.1) on a switch stream or daemon-owned TAP queues.
//!
//! The device is a client of a unix stream socket (the switch's listener) and
//! exchanges whole Ethernet frames in vmlab's stream framing: a 4-byte
//! big-endian length prefix followed by one frame, capped at 65536 + 14 bytes
//! (`src/net/framing.rs` in vmlab defines the format; the switch is the other
//! end of the socket).
//! Vnet streams preserve offload metadata. TAP descriptors arrive over the
//! control socket before resume, with one optional vhost-net backend per pair.
//!
//! Legacy stream attachments use one receiveq and one transmitq. Vnet attachments
//! preserve segmentation metadata and expose queue pairs plus a control queue.
//! The common offered features are
//! `VIRTIO_NET_F_CSUM`, `VIRTIO_NET_F_GUEST_CSUM`, `VIRTIO_NET_F_MAC`,
//! `VIRTIO_NET_F_STATUS`, `VIRTIO_NET_F_MRG_RXBUF` and
//! `VIRTIO_NET_F_MTU` (only when
//! an MTU other than 1500 is configured, matching what vmlab passes to QEMU).
//! Indirect descriptor tables let one ring entry describe a scatter/gather packet.
//! The `virtio_net_hdr` (12 bytes for a modern device) is stripped on transmit
//! and prepended on receive. Mergeable receive buffers carry one header per
//! packet, with `num_buffers` covering all used chains.
//!
//! If the switch socket closes, frames are dropped, `VIRTIO_NET_S_LINK_UP` is
//! cleared in the config space with a config-change interrupt, and the device
//! reconnects on its own. Userspace I/O and vhost call-event forwarding run on
//! one worker thread; the vCPU path sends wakeups or kernel kick eventfds. Receive is under backpressure: the
//! worker stops reading the socket while frames are waiting for receiveq
//! buffers.

use parking_lot::{Mutex, MutexGuard};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use virtio_queue::{Queue, QueueOwnedT, QueueState, QueueT};
use vm_memory::bitmap::Bitmap;
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryBackend, VolatileSlice};

use crate::{DeviceError, GuestMemory, Snapshot, VirtioDevice, VirtioInterrupt};
use vmlab_hv_proto::net::{
    GSO_ECN, GSO_NONE, GSO_TCPV4, GSO_TCPV6, GSO_UDP, Header, METADATA_LEN, NEEDS_CSUM, flow_hash,
};

mod attach;
mod buffer;
mod checksum;
mod tap;
mod worker;

pub use attach::{NetBackendHandle, NetBackendRegistry};

use buffer::ReceiveWriter;
use checksum::CopyChecksum;

/// Virtio device id 1, the network device.
/// Size of `struct virtio_net_config` up to and including `mtu`.
const CONFIG_LEN: usize = 12;
const DEVICE_ID: u32 = 1;
/// The device completes partial checksums before sending bare Ethernet frames.
const F_CSUM: u64 = 1;
/// The device validates receive checksums before setting DATA_VALID.
const F_GUEST_CSUM: u64 = 1 << 1;
const F_GUEST_TSO4: u64 = 1 << 7;
const F_GUEST_TSO6: u64 = 1 << 8;
const F_GUEST_ECN: u64 = 1 << 9;
const F_HOST_TSO4: u64 = 1 << 11;
const F_HOST_TSO6: u64 = 1 << 12;
const F_HOST_ECN: u64 = 1 << 13;
const F_CTRL_VQ: u64 = 1 << 17;
const F_MQ: u64 = 1 << 22;
const F_GUEST_USO4: u64 = 1 << 54;
const F_GUEST_USO6: u64 = 1 << 55;
const F_HOST_USO: u64 = 1 << 56;
const F_NET_OFFLOADS: u64 = F_CSUM
    | F_GUEST_CSUM
    | F_GUEST_TSO4
    | F_GUEST_TSO6
    | F_GUEST_ECN
    | F_HOST_TSO4
    | F_HOST_TSO6
    | F_HOST_ECN
    | F_GUEST_USO4
    | F_GUEST_USO6
    | F_HOST_USO;
const HDR_F_NEEDS_CSUM: u8 = 1;
const HDR_F_DATA_VALID: u8 = 2;
/// `VIRTIO_NET_F_MTU` (feature bit 3).
const F_MTU: u64 = 1 << 3;
/// `VIRTIO_NET_F_MAC` (feature bit 5).
const F_MAC: u64 = 1 << 5;
/// `VIRTIO_NET_F_MRG_RXBUF` (feature bit 15).
const F_MRG_RXBUF: u64 = 1 << 15;
/// `VIRTIO_NET_F_STATUS` (feature bit 16).
const F_STATUS: u64 = 1 << 16;
/// `VIRTIO_F_RING_INDIRECT_DESC` (feature bit 28).
const F_INDIRECT_DESC: u64 = 1 << 28;
/// `VIRTIO_F_RING_EVENT_IDX` (feature bit 29).
const F_EVENT_IDX: u64 = 1 << 29;
/// `VIRTIO_F_VERSION_1` (feature bit 32): this is a modern virtio device.
const F_VERSION_1: u64 = 1 << 32;
/// `VIRTIO_NET_S_LINK_UP` (status bit 0).
const S_LINK_UP: u16 = 1;
/// Queue size offered for both virtqueues (QEMU's virtio-net default).
const QUEUE_SIZE: u16 = 256;
/// Split-ring driver flag suppressing used-buffer interrupts.
const AVAIL_F_NO_INTERRUPT: u16 = 1;
/// The receiveq index.
const RXQ: u16 = 0;
/// The transmitq index.
#[cfg(test)]
const TXQ: u16 = 1;
/// Largest Ethernet frame the wire format accepts (64 KiB payload + header).
const MAX_FRAME: usize = 65536 + 14;
/// Bound host-side output while the switch is backpressured.
const MAX_TX_STAGE: usize = 1024 * 1024;
/// Largest read the worker takes from the switch socket in one go.
const READ_CHUNK: usize = 512 * 1024;
/// Default reconnect interval after the switch socket closes.
const RETRY: Duration = Duration::from_secs(1);
/// The MTU at which `VIRTIO_NET_F_MTU` is not offered, matching vmlab's
/// QEMU argument builder (an MTU is only passed when it differs from this).
const DEFAULT_MTU: u16 = 1500;

/// NIC description: what the machine description carries per NIC.
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// Stable device id used as the snapshot section key.
    pub id: String,
    /// Guest-visible MAC address (`VIRTIO_NET_F_MAC`).
    pub mac: [u8; 6],
    /// MTU advice; `VIRTIO_NET_F_MTU` is offered only when this is not 1500.
    pub mtu: u16,
    /// The switch's unix socket path; the device connects as a client.
    pub switch: PathBuf,
}

/// Where the worker is in reading one length-prefixed frame.
#[derive(Debug, Clone, Default)]
enum RxRead {
    /// No frame in progress.
    #[default]
    Idle,
    /// Part of the 4-byte big-endian length prefix arrived.
    Prefix { buf: [u8; 4], got: usize },
    /// Part of the frame body arrived.
    Body { buf: Vec<u8>, got: usize },
}

/// Everything the device and its worker thread share.
struct Shared {
    tap_mode: bool,
    tap: Option<tap::TapBackend>,
    tap_error: Option<String>,
    vnet: Option<VnetState>,
    snapshot_paused: bool,
    config_pending: bool,
    mem: Option<GuestMemory>,
    interrupt: Option<Arc<dyn VirtioInterrupt>>,
    queues: Vec<Option<Queue>>,
    /// The switch connection is established (maintained by the worker).
    link_up: bool,
    rx: RxRead,
    /// Complete frames waiting for free receiveq buffers.
    rx_pending: VecDeque<Vec<u8>>,
    /// Framed bytes not yet accepted by the switch socket.
    tx_out: Vec<u8>,
}

struct VnetState {
    max_pairs: u16,
    active_pairs: u16,
    features: u64,
    mtu: u16,
    steering: HashMap<u32, u16>,
}

/// Lifecycle commands sent to the worker thread.
enum Cmd {
    Barrier(Sender<()>),
    Shutdown,
}

/// The header length in bytes for a negotiated feature set: the
/// `virtio_net_hdr` gains `num_buffers` for modern devices.
fn hdr_len(negotiated: u64) -> usize {
    if negotiated & (F_VERSION_1 | F_MRG_RXBUF) != 0 {
        12
    } else {
        10
    }
}

/// The virtio-net device.
///
/// The worker thread exists from [`Net::new`] and keeps trying to reach the
/// switch until it connects. Snapshot restore assumes the in-place flow (same
/// process, same socket): state is restored faithfully and the worker
/// reconciles `link_up` with the live connection, so restoring into a fresh
/// device degrades gracefully (it reconnects).
pub struct Net {
    queue_sizes: Vec<u16>,
    config: NetConfig,
    negotiated: Arc<AtomicU64>,
    shared: Arc<Mutex<Shared>>,
    commands: Sender<Cmd>,
    wake: UnixStream,
    wake_pending: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    /// Queue states restored before `activate`, applied when it arrives.
    pending_queues: Option<Vec<Option<QueueState>>>,
}

impl Net {
    /// Create the device; the worker starts reaching for the switch at once.
    pub fn new(config: NetConfig) -> io::Result<Self> {
        Net::with_retry_interval(config, RETRY)
    }

    /// Carry offload metadata over the switch connection and offer multiple pairs.
    pub fn with_offload(config: NetConfig, pairs: u16) -> io::Result<Self> {
        if !(1..=31).contains(&pairs) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "virtio-net needs 1..31 queue pairs",
            ));
        }
        Self::create(config, RETRY, Some(pairs))
    }

    /// Create the device with a non-default switch reconnect interval.
    fn with_retry_interval(config: NetConfig, retry: Duration) -> io::Result<Self> {
        Self::create(config, retry, None)
    }

    fn create(config: NetConfig, retry: Duration, pairs: Option<u16>) -> io::Result<Self> {
        let (wake_dev, wake_worker) = UnixStream::pair()?;
        wake_dev.set_nonblocking(true)?;
        wake_worker.set_nonblocking(true)?;
        let wake_pending = Arc::new(AtomicBool::new(false));
        let (commands, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared {
            tap_mode: false,
            tap: None,
            tap_error: None,
            vnet: pairs.map(|max_pairs| VnetState {
                max_pairs,
                active_pairs: 1,
                features: 0,
                mtu: config.mtu,
                steering: HashMap::new(),
            }),
            snapshot_paused: false,
            config_pending: false,
            mem: None,
            interrupt: None,
            queues: (0..pairs.map_or(2, |pairs| 2 * pairs + 1))
                .map(|_| None)
                .collect(),
            link_up: false,
            rx: RxRead::Idle,
            rx_pending: VecDeque::new(),
            tx_out: Vec::new(),
        }));
        let negotiated = Arc::new(AtomicU64::new(0));
        let switch = config.switch.clone();
        let worker = thread::Builder::new().name("virtio-net".into()).spawn({
            let shared = shared.clone();
            let negotiated = negotiated.clone();
            let pending = wake_pending.clone();
            move || run(switch, retry, shared, negotiated, rx, wake_worker, pending)
        })?;
        Ok(Net {
            queue_sizes: vec![QUEUE_SIZE; usize::from(pairs.map_or(2, |pairs| 2 * pairs + 1))],
            config,
            negotiated,
            shared,
            commands,
            wake: wake_dev,
            wake_pending,
            worker: Some(worker),
            pending_queues: None,
        })
    }

    fn wake_worker(&self) {
        notify_worker(&self.wake, &self.wake_pending);
    }
}

fn notify_worker(mut wake: &UnixStream, pending: &AtomicBool) {
    if !pending.swap(true, Ordering::AcqRel) {
        let _ = wake.write_all(b"k");
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        let _ = self.commands.send(Cmd::Shutdown);
        self.wake_worker();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Lock the shared state.
fn lock(shared: &Arc<Mutex<Shared>>) -> MutexGuard<'_, Shared> {
    shared.lock()
}

// ---- the worker thread ------------------------------------------------------

/// Connect to the switch, exchange frames, and reconnect when it drops.
fn run(
    switch: PathBuf,
    retry: Duration,
    shared: Arc<Mutex<Shared>>,
    negotiated: Arc<AtomicU64>,
    cmds: Receiver<Cmd>,
    wake: UnixStream,
    wake_pending: Arc<AtomicBool>,
) {
    let mut stream: Option<UnixStream> = None;
    let mut next_try = Instant::now();
    let mut shutdown = false;
    while !shutdown {
        // The live connection is the truth about the link.
        let (tap_mode, tap_up) = {
            let sh = lock(&shared);
            (sh.tap_mode, sh.tap.is_some() && sh.tap_error.is_none())
        };
        if set_link(&shared, stream.is_some() || tap_up) {
            fire_config(&shared);
        }

        let mut polls: Vec<libc::pollfd> = Vec::with_capacity(2);
        polls.push(libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        if let Some(stream) = &stream {
            let mut events = 0;
            if rx_may_read(&shared) {
                events |= libc::POLLIN;
            }
            if !lock(&shared).tx_out.is_empty() {
                events |= libc::POLLOUT;
            }
            polls.push(libc::pollfd {
                fd: stream.as_raw_fd(),
                events,
                revents: 0,
            });
        }
        let tap_start = polls.len();
        {
            let sh = lock(&shared);
            if !sh.snapshot_paused
                && let Some(tap) = &sh.tap
            {
                polls.extend(tap.pollfds().into_iter().map(|(_, poll)| poll));
            }
        }
        let timeout: i32 = if stream.is_none() && !tap_mode {
            next_try
                .saturating_duration_since(Instant::now())
                .as_millis() as i32
        } else {
            -1
        };

        // SAFETY: `poll(2)` on a short-lived array of fds we own for the
        // duration of the call; no signal handlers run on this thread.
        let ready = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, timeout) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }

        if polls[0].revents & libc::POLLIN != 0 {
            drain_wakeup(&wake);
            // Rearm before consuming commands. Earlier senders are covered by
            // this drain; later senders must publish another readable byte.
            wake_pending.swap(false, Ordering::AcqRel);
            let features = negotiated.load(Ordering::Acquire);
            let hdr = hdr_len(features);
            while let Ok(cmd) = cmds.try_recv() {
                match cmd {
                    Cmd::Barrier(done) => {
                        let _ = done.send(());
                    }
                    Cmd::Shutdown => shutdown = true,
                }
            }
            // A wakeup is also the device saying shared state changed
            // (activation, restore): give pending work another chance.
            process_wakeup(
                &shared,
                &mut stream,
                hdr,
                features & F_MRG_RXBUF != 0,
                features & F_CSUM != 0,
                features & F_GUEST_CSUM != 0,
            );
        }

        if stream.is_some() {
            let revents = polls[1].revents;
            let dead = revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
            if revents & libc::POLLIN != 0 {
                let features = negotiated.load(Ordering::Acquire);
                let socket = stream.as_mut().unwrap();
                let received = read_guest_frame(
                    &shared,
                    socket,
                    hdr_len(features),
                    features & F_MRG_RXBUF != 0,
                    features & F_GUEST_CSUM != 0,
                )
                .unwrap_or_else(|| read_frames(&shared, socket));
                match received {
                    Ok(()) => {
                        let features = negotiated.load(Ordering::Acquire);
                        place_rx_frames(
                            &shared,
                            hdr_len(features),
                            features & F_MRG_RXBUF != 0,
                            features & F_GUEST_CSUM != 0,
                        );
                    }
                    Err(()) => {
                        drop_link(&mut stream, &shared);
                        continue;
                    }
                }
            }
            if revents & libc::POLLOUT != 0
                && let Some(s) = stream.as_mut()
                && flush_tx(&shared, s).is_err()
            {
                drop_link(&mut stream, &shared);
                continue;
            }
            if revents & libc::POLLOUT != 0 {
                let features = negotiated.load(Ordering::Acquire);
                process_tx(
                    &shared,
                    &mut stream,
                    hdr_len(features),
                    features & F_CSUM != 0,
                );
            }
            if dead {
                drop_link(&mut stream, &shared);
            }
        }

        if tap_mode && polls[tap_start..].iter().any(|poll| poll.revents != 0) {
            worker::process_tap(&shared);
        }

        if !tap_mode && stream.is_none() && Instant::now() >= next_try {
            match UnixStream::connect(&switch) {
                Ok(conn) => {
                    let _ = conn.set_nonblocking(true);
                    stream = Some(conn);
                    // If this connection later dies, wait a full interval
                    // before reaching for the switch again.
                    next_try = Instant::now() + retry;
                    // Partial reads belong to the old connection. Complete
                    // frames may have been restored before this connection;
                    // keep them until the guest posts receive buffers.
                    {
                        let mut sh = lock(&shared);
                        sh.rx = RxRead::Idle;
                    }
                    if set_link(&shared, true) {
                        fire_config(&shared);
                    }
                }
                Err(_) => next_try = Instant::now() + retry,
            }
        }
    }
}

/// Read the wakeup pipe dry (nonblocking).
fn drain_wakeup(mut wake: &UnixStream) {
    let mut buf = [0u8; 256];
    loop {
        match wake.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

fn process_wakeup(
    shared: &Arc<Mutex<Shared>>,
    stream: &mut Option<UnixStream>,
    hdr: usize,
    mergeable: bool,
    csum: bool,
    guest_csum: bool,
) {
    if lock(shared).tap_mode {
        worker::process_tap(shared);
        return;
    }
    place_rx_frames(shared, hdr, mergeable, guest_csum);
    if let Some(socket) = stream.as_mut()
        && flush_tx(shared, socket).is_err()
    {
        drop_link(stream, shared);
    }
    // Draining staging can remove POLLOUT while guest kicks are suppressed.
    process_tx(shared, stream, hdr, csum);
}

/// Update the link bit; true when it changed.
fn set_link(shared: &Arc<Mutex<Shared>>, up: bool) -> bool {
    let mut sh = lock(shared);
    if sh.link_up == up {
        false
    } else {
        sh.link_up = up;
        true
    }
}

/// Deliver a config-change interrupt for a link transition.
fn fire_config(shared: &Arc<Mutex<Shared>>) {
    let mut state = lock(shared);
    if state.snapshot_paused {
        state.config_pending = true;
        return;
    }
    let irq = state.interrupt.clone();
    drop(state);
    if let Some(irq) = irq {
        irq.notify_config();
    }
}

/// Drop the switch connection: frames are discarded until it is back.
fn drop_link(stream: &mut Option<UnixStream>, shared: &Arc<Mutex<Shared>>) {
    *stream = None;
    let mut sh = lock(shared);
    sh.rx = RxRead::Idle;
    sh.rx_pending.clear();
    sh.tx_out.clear();
    let was_up = sh.link_up;
    sh.link_up = false;
    drop(sh);
    if was_up {
        fire_config(shared);
    }
}

/// True while no complete frame is waiting for a receiveq buffer — the worker
/// only reads the switch socket under this condition (a partially read frame
/// always finishes first, and cannot coexist with a queued complete frame).
fn rx_may_read(shared: &Arc<Mutex<Shared>>) -> bool {
    let sh = lock(shared);
    sh.vnet.is_some() || sh.rx_pending.is_empty()
}

/// Read one complete packet directly into available guest buffers. A partial
/// packet is copied into the existing decoder state without publishing DMA.
/// Return None when the buffered decoder must handle framing or backpressure.
fn read_guest_frame(
    shared: &Arc<Mutex<Shared>>,
    stream: &mut UnixStream,
    hdr: usize,
    mergeable: bool,
    guest_csum: bool,
) -> Option<Result<(), ()>> {
    let mut guard = lock(shared);
    let sh = &mut *guard;
    if sh.vnet.is_some()
        || sh.snapshot_paused
        || !sh.rx_pending.is_empty()
        || !matches!(sh.rx, RxRead::Idle)
    {
        return None;
    }
    let mem = sh.mem.as_ref()?;
    let q = sh.queues.get_mut(usize::from(RXQ))?.as_mut()?;
    let mut prefix = [0u8; 4];
    let received = loop {
        match stream.read(&mut prefix) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => break result,
        }
    };
    match received {
        Ok(4) => {}
        Ok(0) => return Some(Err(())),
        Ok(got) => {
            sh.rx = RxRead::Prefix { buf: prefix, got };
            return Some(Ok(()));
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Some(Ok(())),
        Err(_) => return Some(Err(())),
    }
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME {
        return Some(Err(()));
    }
    if len == 0 {
        sh.rx_pending.push_back(Vec::new());
        return Some(Ok(()));
    }
    let start = q.next_avail();
    let old_used = q.next_used();
    let mut header = Vec::new();
    let mut payload = Vec::new();
    let mut entries = Vec::new();
    let mut remaining = len;
    let prepared = (|| {
        while remaining != 0 {
            let chain = q.iter(mem).ok()?.next()?;
            let head = chain.head_index();
            let mut capacity = 0usize;
            let mut skip = if entries.is_empty() { hdr } else { 0 };
            let mut written = skip;
            for descriptor in chain.writable() {
                let slice = mem
                    .get_slice(descriptor.addr(), descriptor.len() as usize)
                    .ok()?;
                capacity = capacity.checked_add(slice.len())?;
                let offset = skip.min(slice.len());
                skip -= offset;
                if offset != 0 {
                    header.push(slice.subslice(0, offset).ok()?);
                }
                let count = (slice.len() - offset).min(remaining);
                if count != 0 {
                    payload.push(slice.subslice(offset, count).ok()?);
                    remaining -= count;
                    written += count;
                }
            }
            if capacity < hdr || skip != 0 || payload.len() > 1024 {
                return None;
            }
            entries.push((head, written as u32));
            if remaining != 0 && (!mergeable || entries.len() >= usize::from(q.size())) {
                return None;
            }
        }
        Some(())
    })();
    if prepared.is_none() {
        q.set_next_avail(start);
        sh.rx = RxRead::Body {
            buf: vec![0; len],
            got: 0,
        };
        return Some(Ok(()));
    }
    let _ = q.disable_notification(mem);
    let count = {
        let guards: Vec<_> = payload.iter().map(VolatileSlice::ptr_guard_mut).collect();
        let vectors: Vec<_> = guards
            .iter()
            .zip(&payload)
            .map(|(guard, slice)| libc::iovec {
                iov_base: guard.as_ptr().cast(),
                iov_len: slice.len(),
            })
            .collect();
        for slice in &payload {
            slice.bitmap().mark_dirty(0, slice.len());
        }
        loop {
            // SAFETY: validated writable mappings and their guards remain live
            // until readv completes. No Rust reference aliases the guest bytes.
            let read =
                unsafe { libc::readv(stream.as_raw_fd(), vectors.as_ptr(), vectors.len() as i32) };
            if read >= 0 {
                break Ok(read as usize);
            }
            match io::Error::last_os_error().kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => break Ok(0),
                _ => break Err(()),
            }
        }
    };
    let Ok(count) = count else {
        q.set_next_avail(start);
        return Some(Err(()));
    };
    if count != len {
        let mut bytes = vec![0; len];
        let mut copied = 0;
        for slice in &payload {
            let take = slice.len().min(count - copied);
            slice.copy_to(&mut bytes[copied..copied + take]);
            copied += take;
            if copied == count {
                break;
            }
        }
        sh.rx = RxRead::Body {
            buf: bytes,
            got: count,
        };
        q.set_next_avail(start);
        return Some(Ok(()));
    }
    let mut bytes = [0; 12];
    if guest_csum {
        let mut header = [0; 128];
        let mut copied = 0;
        for slice in &payload {
            let count = slice.len().min(header.len() - copied);
            slice.copy_to(&mut header[copied..copied + count]);
            copied += count;
            if copied == header.len() {
                break;
            }
        }
        if let Some(mut checksum) = CopyChecksum::receive_header(&header[..copied], len) {
            for slice in &payload {
                let guard = slice.ptr_guard();
                // SAFETY: readv initialized this entire guarded guest span.
                unsafe { checksum.update_guest(guard.as_ptr(), slice.len()) };
            }
            if checksum.valid() {
                bytes[0] = HDR_F_DATA_VALID;
            }
        }
    }
    if hdr == 12 {
        bytes[10..].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    }
    let mut copied = 0;
    for slice in header {
        slice.copy_from(&bytes[copied..copied + slice.len()]);
        copied += slice.len();
    }
    if publish_receive(mem, q, &entries).is_err() {
        return Some(Err(()));
    }
    let irq = needs_receive_interrupt(q, mem, old_used)
        .then(|| sh.interrupt.clone())
        .flatten();
    drop(guard);
    if let Some(irq) = irq {
        irq.notify_queue(RXQ);
    }
    Some(Ok(()))
}

/// Read length-prefixed frames off the switch socket into `rx_pending`.
/// `Err(())` means the stream is corrupt or the peer is gone.
fn read_frames(shared: &Arc<Mutex<Shared>>, stream: &mut UnixStream) -> Result<(), ()> {
    let mut chunk = [0u8; READ_CHUNK];
    let n = match stream.read(&mut chunk) {
        Ok(0) => return Err(()),
        Ok(n) => n,
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
        Err(_) => return Err(()),
    };
    let mut sh = lock(shared);
    let mut complete = Vec::new();
    let mut bytes = &chunk[..n];
    while !bytes.is_empty() {
        match &mut sh.rx {
            RxRead::Idle => {
                sh.rx = RxRead::Prefix {
                    buf: [0; 4],
                    got: 0,
                };
            }
            RxRead::Prefix { buf, got } => {
                let take = (buf.len() - *got).min(bytes.len());
                buf[*got..*got + take].copy_from_slice(&bytes[..take]);
                *got += take;
                bytes = &bytes[take..];
                if *got == 4 {
                    let len = u32::from_be_bytes(*buf) as usize;
                    if len > MAX_FRAME + if sh.vnet.is_some() { METADATA_LEN } else { 0 } {
                        // A length past the cap is a corrupt stream, exactly
                        // as vmlab's framing codec treats it.
                        sh.rx = RxRead::Idle;
                        sh.rx_pending.clear();
                        return Err(());
                    }
                    if len == 0 {
                        complete.push(Vec::new());
                        sh.rx = RxRead::Idle;
                    } else {
                        sh.rx = RxRead::Body {
                            buf: vec![0u8; len],
                            got: 0,
                        };
                    }
                }
            }
            RxRead::Body { buf, got } => {
                let take = (buf.len() - *got).min(bytes.len());
                buf[*got..*got + take].copy_from_slice(&bytes[..take]);
                *got += take;
                bytes = &bytes[take..];
                if *got == buf.len() {
                    let frame = std::mem::take(buf);
                    complete.push(frame);
                    sh.rx = RxRead::Idle;
                }
            }
        }
    }
    if let Some(vnet) = sh.vnet.as_ref() {
        let pair_for = |frame: &[u8]| {
            let hash = flow_hash(frame);
            usize::from(
                vnet.steering
                    .get(&hash)
                    .copied()
                    .unwrap_or((hash % u32::from(vnet.active_pairs)) as u16)
                    % vnet.active_pairs,
            )
        };
        let mut counts = vec![0usize; usize::from(vnet.active_pairs)];
        for body in &sh.rx_pending {
            if let Ok((_, _, frame)) = Header::from_metadata(body) {
                counts[pair_for(frame)] += body.len();
            }
        }
        let mut accepted = Vec::new();
        for body in complete {
            let (_, _, frame) = Header::from_metadata(&body).map_err(|_| ())?;
            let count = &mut counts[pair_for(frame)];
            // A stalled queue must neither block other flows nor grow without bound.
            if count.saturating_add(body.len()) <= MAX_TX_STAGE {
                *count += body.len();
                accepted.push(body);
            }
        }
        sh.rx_pending.extend(accepted);
    } else {
        sh.rx_pending.extend(complete);
    }
    Ok(())
}

fn needs_interrupt(queue: &mut Queue, mem: &GuestMemory) -> bool {
    // virtio-queue handles used_event and the publication fence. It does not
    // check avail.flags without EVENT_IDX.
    if !queue.needs_notification(mem).unwrap_or(false) {
        return false;
    }
    queue.event_idx_enabled()
        || mem
            .load::<u16>(GuestAddress(queue.avail_ring()), Ordering::Relaxed)
            .is_ok_and(|flags| u16::from_le(flags) & AVAIL_F_NO_INTERRUPT == 0)
}

enum Receive {
    Delivered,
    Retry,
    Wait(u16),
}

/// Publish the whole packet with one release-store to used.idx. The queue's
/// notification counter accounts for the final entry only; RX uses the
/// saved pre-batch index when checking the driver's interrupt event.
fn publish_receive(mem: &GuestMemory, q: &mut Queue, entries: &[(u16, u32)]) -> Result<(), ()> {
    let (&(head, len), earlier) = entries.split_last().ok_or(())?;
    let mut next = q.next_used();
    for &(head, len) in earlier {
        let address = GuestAddress(q.used_ring())
            .checked_add(4 + 8 * u64::from(next % q.size()))
            .ok_or(())?;
        mem.write_obj((u64::from(head) | u64::from(len) << 32).to_le(), address)
            .map_err(|_| ())?;
        next = next.wrapping_add(1);
    }
    q.set_next_used(next);
    q.add_used(mem, head, len).map_err(|_| ())
}

fn receive_merged(
    mem: &GuestMemory,
    q: &mut Queue,
    frame: &[u8],
    guest_csum: bool,
) -> Result<Receive, ()> {
    receive_merged_header(mem, q, frame, guest_csum, Header::default())
}

fn receive_merged_header(
    mem: &GuestMemory,
    q: &mut Queue,
    frame: &[u8],
    guest_csum: bool,
    metadata: Header,
) -> Result<Receive, ()> {
    let start = q.next_avail();
    let need = 12 + frame.len();
    let mut buffers = Vec::new();
    let mut entries = Vec::new();
    let mut capacity = 0;
    while capacity < need {
        let chain = q.iter(mem).map_err(|_| ())?.next();
        let Some(chain) = chain else {
            let observed = q.next_avail();
            q.set_next_avail(start);
            return Ok(Receive::Wait(observed));
        };
        let head = chain.head_index();
        let writer = ReceiveWriter::new(mem, chain);
        entries.push((head, 0));
        let Ok(writer) = writer else {
            publish_receive(mem, q, &entries)?;
            return Ok(Receive::Retry);
        };
        if writer.available_bytes() < 12 {
            publish_receive(mem, q, &entries)?;
            return Ok(Receive::Retry);
        }
        capacity += writer.available_bytes();
        buffers.push(writer);
        if buffers.len() == usize::from(q.size()) && capacity < need {
            // A full ring cannot hold this packet. Drop it and return the
            // buffers so a misconfigured driver cannot stall the worker.
            publish_receive(mem, q, &entries)?;
            return Ok(Receive::Delivered);
        }
    }
    let mut header = metadata.encode();
    header[10..].copy_from_slice(&(buffers.len() as u16).to_le_bytes());
    let mut checksum = guest_csum.then(|| CopyChecksum::receive(frame)).flatten();
    let mut remaining = frame;
    for (index, (writer, entry)) in buffers.iter_mut().zip(&mut entries).enumerate() {
        if index == 0 {
            writer.write_all(&header).map_err(|_| ())?;
            entry.1 = 12;
        }
        let len = writer.available_bytes().min(remaining.len());
        writer
            .write_frame(&remaining[..len], checksum.as_mut())
            .map_err(|_| ())?;
        entry.1 += len as u32;
        remaining = &remaining[len..];
    }
    if checksum.as_ref().is_some_and(CopyChecksum::valid) {
        buffers[0].set_flags(HDR_F_DATA_VALID).map_err(|_| ())?;
    }
    publish_receive(mem, q, &entries)?;
    Ok(Receive::Delivered)
}

fn receive_offload_supported(header: Header, frame: &[u8], features: u64) -> bool {
    if header.flags & NEEDS_CSUM != 0 && features & F_GUEST_CSUM == 0 {
        return false;
    }
    if header.gso_type & GSO_ECN != 0 && features & F_GUEST_ECN == 0 {
        return false;
    }
    let required = match header.gso_type & !GSO_ECN {
        GSO_NONE => return true,
        GSO_TCPV4 => F_GUEST_TSO4,
        GSO_TCPV6 => F_GUEST_TSO6,
        GSO_UDP => {
            let mut at = 14;
            let mut kind = u16::from_be_bytes([frame[12], frame[13]]);
            for _ in 0..2 {
                if !matches!(kind, 0x8100 | 0x88a8) {
                    break;
                }
                let Some(bytes) = frame.get(at + 2..at + 4) else {
                    return false;
                };
                kind = u16::from_be_bytes([bytes[0], bytes[1]]);
                at += 4;
            }
            if kind == 0x0800 {
                F_GUEST_USO4
            } else {
                F_GUEST_USO6
            }
        }
        _ => return false,
    };
    features & required != 0
}

/// Decode metadata before DMA, and leave a blocked flow independent of other RX queues.
fn place_vnet_rx_frames(
    shared: &Arc<Mutex<Shared>>,
    hdr_len: usize,
    mergeable: bool,
    guest_csum: bool,
) {
    let mut guard = lock(shared);
    let sh = &mut *guard;
    if sh.snapshot_paused {
        return;
    }
    let Some(vnet) = sh.vnet.as_ref() else { return };
    let Some(mem) = sh.mem.as_ref() else { return };
    let mut pending = std::mem::take(&mut sh.rx_pending);
    let mut interrupts = Vec::new();
    while let Some(packet) = pending.pop_front() {
        let Ok((mut metadata, _, frame)) = Header::from_metadata(&packet) else {
            continue;
        };
        if vnet.features & F_GUEST_CSUM == 0 {
            metadata.flags &= !HDR_F_DATA_VALID;
        }
        if !receive_offload_supported(metadata, frame, vnet.features) {
            if let Ok(parts) = metadata.segments(frame, vnet.mtu) {
                for part in parts.into_iter().rev() {
                    let mut body = Header::default().metadata(0).to_vec();
                    body.extend_from_slice(&part);
                    pending.push_front(body);
                }
            }
            continue;
        }
        let hash = flow_hash(frame);
        let pair = vnet
            .steering
            .get(&hash)
            .copied()
            .unwrap_or((hash % u32::from(vnet.active_pairs)) as u16)
            % vnet.active_pairs;
        let index = 2 * pair;
        let Some(q) = sh
            .queues
            .get_mut(usize::from(index))
            .and_then(Option::as_mut)
        else {
            sh.rx_pending.push_back(packet);
            continue;
        };
        if !q.ready() {
            sh.rx_pending.push_back(packet);
            continue;
        }
        let old_used = q.next_used();
        let _ = q.disable_notification(mem);
        let result = if mergeable {
            receive_merged_header(mem, q, frame, guest_csum && metadata.flags == 0, metadata)
        } else {
            match q.iter(mem).ok().and_then(|mut iter| iter.next()) {
                None => Ok(Receive::Wait(q.next_avail())),
                Some(chain) => {
                    let head = chain.head_index();
                    let mut header = metadata.encode();
                    if hdr_len == 12 {
                        header[10..].copy_from_slice(&1u16.to_le_bytes());
                    }
                    let need = hdr_len + frame.len();
                    let mut checksum = (guest_csum && metadata.flags == 0)
                        .then(|| CopyChecksum::receive(frame))
                        .flatten();
                    let written = if let Ok(mut writer) = ReceiveWriter::new(mem, chain)
                        && writer.available_bytes() >= need
                        && writer.write_all(&header[..hdr_len]).is_ok()
                        && writer.write_frame(frame, checksum.as_mut()).is_ok()
                        && (!checksum.as_ref().is_some_and(CopyChecksum::valid)
                            || writer.set_flags(HDR_F_DATA_VALID).is_ok())
                    {
                        need as u32
                    } else {
                        0
                    };
                    q.add_used(mem, head, written).map_err(|_| ()).map(|()| {
                        if written == 0 {
                            Receive::Retry
                        } else {
                            Receive::Delivered
                        }
                    })
                }
            }
        };
        match result {
            Ok(Receive::Delivered) => {}
            Ok(Receive::Retry) => pending.push_front(packet),
            Ok(Receive::Wait(observed)) => {
                let next = q.next_avail();
                q.set_next_avail(observed);
                let raced = q.enable_notification(mem).unwrap_or(false);
                q.set_next_avail(next);
                if raced {
                    pending.push_front(packet);
                } else {
                    sh.rx_pending.push_back(packet);
                }
            }
            Err(()) => {}
        }
        if q.next_used() != old_used && needs_receive_interrupt(q, mem, old_used) {
            interrupts.push(index);
        }
    }
    let irq = sh.interrupt.clone();
    drop(guard);
    if let Some(irq) = irq {
        for index in interrupts {
            irq.notify_queue(index);
        }
    }
}

fn prepare_vnet_tx(
    packet: &mut TxPacket<'_>,
    bytes: &[u8; 12],
    len: usize,
    pair: u16,
    vnet: &mut VnetState,
) -> bool {
    let Ok(header) = Header::decode(bytes) else {
        return false;
    };
    let required = match header.gso_type & !GSO_ECN {
        GSO_NONE => 0,
        GSO_TCPV4 => F_HOST_TSO4,
        GSO_TCPV6 => F_HOST_TSO6,
        GSO_UDP => F_HOST_USO,
        _ => return false,
    };
    if (required != 0 && vnet.features & required == 0)
        || header.flags & HDR_F_DATA_VALID != 0
        || (header.flags & NEEDS_CSUM != 0 && vnet.features & F_CSUM == 0)
        || (header.gso_type & GSO_ECN != 0 && vnet.features & F_HOST_ECN == 0)
    {
        return false;
    }
    let mut body = header.metadata(pair).to_vec();
    body.resize(METADATA_LEN + len, 0);
    let mut at = METADATA_LEN;
    for slice in &packet.slices {
        let Some(target) = body.get_mut(at..at + slice.len()) else {
            return false;
        };
        slice.copy_to(target);
        at += slice.len();
    }
    if at != body.len() || header.validate(&body[METADATA_LEN..]).is_err() {
        return false;
    }
    if vnet.steering.len() >= 65536 {
        vnet.steering.clear();
    }
    vnet.steering.insert(flow_hash(&body[METADATA_LEN..]), pair);
    packet.owned = Some(body);
    packet.slices.clear();
    true
}

/// MQ VQ_PAIRS_SET; unknown classes, malformed payloads and invalid counts return ERR.
fn process_control(shared: &Arc<Mutex<Shared>>) {
    let mut guard = lock(shared);
    let sh = &mut *guard;
    if sh.snapshot_paused {
        return;
    }
    let Some(vnet) = sh.vnet.as_mut() else { return };
    if vnet.features & F_CTRL_VQ == 0 {
        return;
    }
    let index = if vnet.features & F_MQ != 0 {
        2 * vnet.max_pairs
    } else {
        2
    };
    let Some(mem) = sh.mem.as_ref() else { return };
    let Some(mut q) = sh.queues.get_mut(usize::from(index)).and_then(Option::take) else {
        return;
    };
    if !q.ready() {
        sh.queues[usize::from(index)] = Some(q);
        return;
    }
    let old_used = q.next_used();
    let mut invalid = false;
    loop {
        let _ = q.disable_notification(mem);
        loop {
            let chain = match q.iter(mem) {
                Ok(mut iter) => iter.next(),
                Err(_) => {
                    invalid = true;
                    break;
                }
            };
            let Some(chain) = chain else { break };
            let head = chain.head_index();
            let mut command = [0; 4];
            let mut len = 0usize;
            let mut valid = true;
            for desc in chain.clone().readable() {
                let count = desc.len() as usize;
                if let Some(target) = command.get_mut(len..len.saturating_add(count)) {
                    valid &= mem.read_slice(target, desc.addr()).is_ok();
                } else {
                    valid = false;
                }
                len = len.saturating_add(count);
            }
            let pairs = u16::from_le_bytes([command[2], command[3]]);
            valid &= len == 4
                && command[..2] == [4, 0]
                && (1..=vnet.max_pairs).contains(&pairs)
                && vnet.features & F_MQ != 0;
            let written = if let Ok(mut writer) = ReceiveWriter::new(mem, chain)
                && writer.available_bytes() >= 1
            {
                if valid
                    && let Some(tap) = sh.tap.as_mut()
                    && let Err(error) = tap.configure(&mut sh.queues, vnet.features, pairs)
                {
                    valid = false;
                    log::error!("changing TAP queue pairs: {error}");
                    if let Err(error) =
                        tap.configure(&mut sh.queues, vnet.features, vnet.active_pairs)
                    {
                        sh.tap_error = Some(error.to_string());
                    }
                }
                let success = writer.write_all(&[u8::from(!valid)]).is_ok();
                if success && valid {
                    vnet.active_pairs = pairs;
                    vnet.steering.retain(|_, pair| *pair < pairs);
                }
                u32::from(success)
            } else {
                0
            };
            if q.add_used(mem, head, written).is_err() {
                invalid = true;
                break;
            }
        }
        if invalid || !q.enable_notification(mem).unwrap_or(false) {
            break;
        }
    }
    let irq = (q.next_used() != old_used && needs_receive_interrupt(&mut q, mem, old_used))
        .then(|| sh.interrupt.clone())
        .flatten();
    sh.queues[usize::from(index)] = Some(q);
    drop(guard);
    if let Some(irq) = irq {
        irq.notify_queue(index);
    }
}

fn needs_receive_interrupt(q: &mut Queue, mem: &GuestMemory, old_used: u16) -> bool {
    let normal = needs_interrupt(q, mem);
    if !q.event_idx_enabled() {
        return normal;
    }
    // needs_interrupt supplies the publication fence and resets num_added.
    let Some(event) = GuestAddress(q.avail_ring()).checked_add(4 + 2 * u64::from(q.size())) else {
        return false;
    };
    let Ok(event) = mem.load::<u16>(event, Ordering::Relaxed) else {
        return false;
    };
    let new = q.next_used();
    new.wrapping_sub(u16::from_le(event)).wrapping_sub(1) < new.wrapping_sub(old_used)
}

/// Move pending frames into free receiveq buffers, header first.
fn place_rx_frames(shared: &Arc<Mutex<Shared>>, hdr_len: usize, mergeable: bool, guest_csum: bool) {
    if lock(shared).vnet.is_some() {
        place_vnet_rx_frames(shared, hdr_len, mergeable, guest_csum);
        return;
    }
    let mut irq: Option<(Arc<dyn VirtioInterrupt>, u16)> = None;
    {
        let mut guard = lock(shared);
        // A plain reborrow lets the field borrows below stay disjoint.
        let sh = &mut *guard;
        if sh.snapshot_paused {
            return;
        }
        if sh.rx_pending.is_empty() {
            return;
        }
        let Some(mem) = sh.mem.as_ref() else {
            return;
        };
        let Some(q) = sh.queues.get_mut(usize::from(RXQ)).and_then(|q| q.as_mut()) else {
            return;
        };
        let old_used = q.next_used();
        // Without mergeable buffers, a single chain holds the whole packet.
        let mut hdr = vec![0u8; hdr_len];
        let num_buffers_at = hdr.len().saturating_sub(2);
        hdr[num_buffers_at..].copy_from_slice(&1u16.to_le_bytes());
        let mut used_any = false;
        let mut invalid = false;
        loop {
            let _ = q.disable_notification(mem);
            let mut refill_at = None;
            while let Some(frame) = sh.rx_pending.front() {
                if mergeable {
                    match receive_merged(mem, q, frame, guest_csum) {
                        Ok(Receive::Delivered) => {
                            used_any = true;
                            sh.rx_pending.pop_front();
                        }
                        Ok(Receive::Retry) => used_any = true,
                        Ok(Receive::Wait(observed)) => {
                            refill_at = Some(observed);
                            break;
                        }
                        Err(()) => {
                            invalid = true;
                            break;
                        }
                    }
                    continue;
                }
                let chain = match q.iter(mem) {
                    Ok(mut available) => available.next(),
                    Err(_) => {
                        invalid = true;
                        break;
                    }
                };
                let Some(chain) = chain else { break };
                let head = chain.head_index();
                let need = hdr.len() + frame.len();
                let mut written = 0;
                let mut checksum = guest_csum.then(|| CopyChecksum::receive(frame)).flatten();
                if let Ok(mut writer) = ReceiveWriter::new(mem, chain)
                    && writer.available_bytes() >= need
                    && writer.write_all(&hdr).is_ok()
                    && writer.write_frame(frame, checksum.as_mut()).is_ok()
                    && (!checksum.as_ref().is_some_and(CopyChecksum::valid)
                        || writer.set_flags(HDR_F_DATA_VALID).is_ok())
                {
                    written = need as u32;
                }
                if q.add_used(mem, head, written).is_err() {
                    invalid = true;
                    break;
                }
                used_any = true;
                if written == 0 {
                    break;
                }
                sh.rx_pending.pop_front();
            }
            // Socket input drives RX while buffers are available. If a frame
            // is waiting, arm the refill kick and check for a raced refill.
            if sh.rx_pending.is_empty() || invalid {
                break;
            }
            let next = q.next_avail();
            if let Some(observed) = refill_at {
                q.set_next_avail(observed);
            }
            let raced = q.enable_notification(mem).unwrap_or(false);
            q.set_next_avail(next);
            if !raced {
                break;
            }
        }
        if used_any && needs_receive_interrupt(q, mem, old_used) {
            irq = sh.interrupt.clone().map(|irq| (irq, RXQ));
        }
    }
    if let Some((irq, queue)) = irq {
        irq.notify_queue(queue);
    }
}

/// One consumed transmit chain and its validated payload spans.
struct TxPacket<'a> {
    head: u16,
    consumed: u32,
    prefix: Option<[u8; 4]>,
    slices: Vec<VolatileSlice<'a>>,
    owned: Option<Vec<u8>>,
}

fn complete_tx_checksum(
    packet: &mut TxPacket<'_>,
    header: &[u8; 12],
    len: usize,
    csum: bool,
) -> bool {
    if header[1] != 0 {
        return false;
    }
    if header[0] & HDR_F_NEEDS_CSUM == 0 {
        return true;
    }
    if !csum {
        return false;
    }
    let start = usize::from(u16::from_le_bytes([header[6], header[7]]));
    let offset = usize::from(u16::from_le_bytes([header[8], header[9]]));
    let Some(mut checksum) = CopyChecksum::transmit(start, offset, len) else {
        return false;
    };
    let mut bytes = vec![0; len];
    let mut copied = 0;
    for slice in &packet.slices {
        let guard = slice.ptr_guard();
        let Some(target) = bytes.get_mut(copied..copied + slice.len()) else {
            return false;
        };
        // SAFETY: the guest guard retains slice.len() readable bytes; the
        // disjoint host vector has the same validated total payload length.
        unsafe { checksum.copy(guard.as_ptr(), target.as_mut_ptr(), target.len()) };
        copied += slice.len();
    }
    if copied != len {
        return false;
    }
    let field = start + offset;
    bytes[field..field + 2].copy_from_slice(&checksum.completed().to_be_bytes());
    packet.owned = Some(bytes);
    packet.slices.clear();
    true
}

/// Preserve the unsent suffix before publishing any used descriptor.
fn send_tx_batch(stream: &UnixStream, packets: &[TxPacket<'_>], output: &mut Vec<u8>) {
    let guards: Vec<_> = packets
        .iter()
        .flat_map(|packet| packet.slices.iter())
        .map(VolatileSlice::ptr_guard)
        .collect();
    let mut vectors = Vec::new();
    let mut next = 0;
    for packet in packets {
        if let Some(prefix) = &packet.prefix {
            vectors.push(libc::iovec {
                iov_base: prefix.as_ptr().cast_mut().cast(),
                iov_len: 4,
            });
            if let Some(bytes) = &packet.owned {
                vectors.push(libc::iovec {
                    iov_base: bytes.as_ptr().cast_mut().cast(),
                    iov_len: bytes.len(),
                });
            }
            for slice in &packet.slices {
                vectors.push(libc::iovec {
                    iov_base: guards[next].as_ptr().cast_mut().cast(),
                    iov_len: slice.len(),
                });
                next += 1;
            }
        }
    }
    let mut sent = 0;
    if output.is_empty() && !vectors.is_empty() && vectors.len() <= 1024 {
        loop {
            // SAFETY: prefixes and validated guest mappings remain live and
            // guarded until writev completes. No Rust references alias DMA.
            let result =
                unsafe { libc::writev(stream.as_raw_fd(), vectors.as_ptr(), vectors.len() as i32) };
            if result >= 0 {
                sent = result as usize;
                break;
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
    }
    for packet in packets {
        let Some(prefix) = &packet.prefix else {
            continue;
        };
        let skip = sent.min(4);
        sent -= skip;
        output.extend_from_slice(&prefix[skip..]);
        if let Some(bytes) = &packet.owned {
            let skip = sent.min(bytes.len());
            sent -= skip;
            output.extend_from_slice(&bytes[skip..]);
        }
        for slice in &packet.slices {
            let skip = sent.min(slice.len());
            sent -= skip;
            if skip != slice.len() {
                let slice = slice.offset(skip).unwrap();
                let start = output.len();
                output.resize(start + slice.len(), 0);
                slice.copy_to(&mut output[start..]);
            }
        }
    }
}

/// Drain transmit descriptors into bounded vectored batches, or drop them
/// while the link is down.
fn process_tx(
    shared: &Arc<Mutex<Shared>>,
    stream: &mut Option<UnixStream>,
    hdr_len: usize,
    csum: bool,
) {
    process_control(shared);
    let pairs = lock(shared)
        .vnet
        .as_ref()
        .map_or(1, |vnet| vnet.active_pairs);
    for pair in 0..pairs {
        process_tx_queue(shared, stream, hdr_len, csum, 2 * pair + 1);
    }
}

fn process_tx_queue(
    shared: &Arc<Mutex<Shared>>,
    stream: &mut Option<UnixStream>,
    hdr_len: usize,
    csum: bool,
    tx_queue: u16,
) {
    loop {
        let mut blocked = false;
        let more;
        let mut irq: Option<(Arc<dyn VirtioInterrupt>, u16)> = None;
        {
            let mut guard = lock(shared);
            // A plain reborrow lets the field borrows below stay disjoint.
            let sh = &mut *guard;
            if sh.snapshot_paused {
                return;
            }
            let Some(mem) = sh.mem.as_ref() else {
                return;
            };
            let Some(q) = sh
                .queues
                .get_mut(usize::from(tx_queue))
                .and_then(|q| q.as_mut())
            else {
                return;
            };
            if !q.ready() {
                return;
            }
            let _ = q.disable_notification(mem);
            let mut used_any = false;
            let mut invalid = false;
            let mut packets = Vec::new();
            let mut bytes = sh.tx_out.len();
            let mut vectors = 0;
            let mut full_batch = false;
            loop {
                let chain = match q.iter(mem) {
                    Ok(mut available) => available.next(),
                    Err(_) => {
                        invalid = true;
                        break;
                    }
                };
                let Some(chain) = chain else { break };
                if stream.is_some() && bytes >= MAX_TX_STAGE {
                    q.go_to_previous_position();
                    blocked = true;
                    break;
                }
                let head = chain.head_index();
                let mut packet = TxPacket {
                    head,
                    consumed: 0,
                    prefix: None,
                    slices: Vec::new(),
                    owned: None,
                };
                let mut skip = hdr_len;
                let mut header = [0; 12];
                let mut total = Some(0usize);
                for descriptor in chain.readable() {
                    let Ok(slice) = mem.get_slice(descriptor.addr(), descriptor.len() as usize)
                    else {
                        total = None;
                        break;
                    };
                    total = total.and_then(|bytes| bytes.checked_add(slice.len()));
                    let offset = skip.min(slice.len());
                    slice
                        .subslice(0, offset)
                        .unwrap()
                        .copy_to(&mut header[hdr_len - skip..hdr_len - skip + offset]);
                    skip -= offset;
                    if offset < slice.len() {
                        packet.slices.push(slice.offset(offset).unwrap());
                    }
                }
                if let Some(total) = total {
                    packet.consumed = total as u32;
                    let len = total.saturating_sub(hdr_len);
                    if stream.is_some()
                        && len <= MAX_FRAME
                        && skip == 0
                        && match sh.vnet.as_mut() {
                            Some(vnet) => {
                                prepare_vnet_tx(&mut packet, &header, len, tx_queue / 2, vnet)
                            }
                            None => complete_tx_checksum(&mut packet, &header, len, csum),
                        }
                    {
                        let len = packet.owned.as_ref().map_or(len, Vec::len);
                        packet.prefix = Some((len as u32).to_be_bytes());
                        let count = 1 + packet.slices.len() + usize::from(packet.owned.is_some());
                        if !packets.is_empty() && vectors + count > 1024 {
                            q.go_to_previous_position();
                            full_batch = true;
                            break;
                        }
                        vectors += count;
                        bytes += 4 + len;
                    }
                }
                if packet.prefix.is_none() {
                    packet.slices.clear();
                    packet.owned = None;
                }
                packets.push(packet);
            }
            if let Some(socket) = stream.as_ref() {
                send_tx_batch(socket, &packets, &mut sh.tx_out);
            }
            for packet in packets {
                if q.add_used(mem, packet.head, packet.consumed).is_err() {
                    invalid = true;
                    break;
                }
                used_any = true;
            }
            // POLLOUT resumes a backpressured queue. Otherwise rearm kicks
            // and check for buffers published before the rearm became visible.
            blocked |= stream.is_some() && sh.tx_out.len() >= MAX_TX_STAGE;
            more = !invalid
                && (full_batch || (!blocked && q.enable_notification(mem).unwrap_or(false)));
            if used_any && needs_interrupt(q, mem) {
                irq = sh.interrupt.clone().map(|irq| (irq, tx_queue));
            }
        }
        if let Some((irq, queue)) = irq {
            irq.notify_queue(queue);
        }
        if let Some(stream_ref) = stream.as_mut()
            && flush_tx(shared, stream_ref).is_err()
        {
            drop_link(stream, shared);
        }
        if !more && (!blocked || lock(shared).tx_out.len() >= MAX_TX_STAGE) {
            return;
        }
    }
}

/// Push framed bytes into the switch socket; keeps the remainder when the
/// socket would block. `Err(())` means the connection is gone.
fn flush_tx(shared: &Arc<Mutex<Shared>>, stream: &mut UnixStream) -> Result<(), ()> {
    let mut sh = lock(shared);
    let mut sent = 0;
    let result = loop {
        if sent == sh.tx_out.len() {
            break Ok(());
        }
        match stream.write(&sh.tx_out[sent..]) {
            Ok(0) => break Err(()),
            Ok(n) => sent += n,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break Ok(()),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break Err(()),
        }
    };
    if sent == sh.tx_out.len() {
        sh.tx_out.clear();
    } else if sent != 0 {
        sh.tx_out.drain(..sent);
    }
    result
}

// ---- VirtioDevice -----------------------------------------------------------

impl VirtioDevice for Net {
    fn device_type(&self) -> u32 {
        DEVICE_ID
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn config_len(&self) -> u64 {
        CONFIG_LEN as u64
    }

    fn features(&self) -> u64 {
        let mut features = F_CSUM
            | F_GUEST_CSUM
            | F_MAC
            | F_STATUS
            | F_MRG_RXBUF
            | F_INDIRECT_DESC
            | F_EVENT_IDX
            | F_VERSION_1;
        if self.config.mtu != DEFAULT_MTU {
            features |= F_MTU;
        }
        if let Some(vnet) = lock(&self.shared).vnet.as_ref() {
            features |= F_GUEST_TSO4
                | F_GUEST_TSO6
                | F_GUEST_ECN
                | F_HOST_TSO4
                | F_HOST_TSO6
                | F_HOST_ECN
                | F_GUEST_USO4
                | F_GUEST_USO6
                | F_HOST_USO
                | F_CTRL_VQ;
            if vnet.max_pairs > 1 {
                features |= F_MQ;
            }
        }
        if let Some(tap) = lock(&self.shared).tap.as_ref() {
            features &= tap.features();
        }
        features
    }

    fn ack_features(&mut self, features: u64) {
        if let Some(vnet) = lock(&self.shared).vnet.as_mut() {
            vnet.features = features;
        }
        self.negotiated.store(features, Ordering::Release);
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let link_up = lock(&self.shared).link_up;
        // struct virtio_net_config: mac[6], status u16 @6,
        // max_virtqueue_pairs u16 @8, mtu u16 @10 (VIRTIO 1.2 §5.1.4).
        let mut config = [0u8; CONFIG_LEN];
        config[0..6].copy_from_slice(&self.config.mac);
        let status = if link_up { S_LINK_UP } else { 0 };
        config[6..8].copy_from_slice(&status.to_le_bytes());
        let pairs = lock(&self.shared)
            .vnet
            .as_ref()
            .map_or(1, |vnet| vnet.max_pairs);
        config[8..10].copy_from_slice(&pairs.to_le_bytes());
        if self.config.mtu != DEFAULT_MTU {
            config[10..12].copy_from_slice(&self.config.mtu.to_le_bytes());
        }
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = config.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // The virtio-net config space is read-only for the driver.
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Arc<dyn VirtioInterrupt>,
        queues: Vec<Queue>,
    ) -> Result<(), DeviceError> {
        if queues.len() != self.queue_sizes.len() {
            return Err(DeviceError::Other(format!(
                "virtio-net needs {} queues, got {}",
                self.queue_sizes.len(),
                queues.len()
            )));
        }
        let restored = self.pending_queues.take();
        let mut sh = lock(&self.shared);
        sh.mem = Some(mem);
        sh.interrupt = Some(interrupt);
        for (i, queue) in queues.into_iter().enumerate() {
            sh.queues[i] = Some(match restored.as_ref().and_then(|r| r[i]) {
                Some(state) => Queue::try_from(state)
                    .map_err(|e| DeviceError::State(format!("queue {i}: {e}")))?,
                None => queue,
            });
        }
        let (features, pairs) = sh
            .vnet
            .as_ref()
            .map_or((0, 1), |state| (state.features, state.active_pairs));
        let Shared {
            tap,
            queues,
            snapshot_paused,
            ..
        } = &mut *sh;
        if !*snapshot_paused && let Some(tap) = tap {
            tap.configure(queues, features, pairs)?;
        }
        drop(sh);
        self.wake_worker();
        Ok(())
    }

    fn queue_notify(&mut self, index: u16) {
        {
            let mut state = lock(&self.shared);
            let active = state.vnet.as_ref().map_or(1, |vnet| vnet.active_pairs);
            if index < 2 * active
                && let Some(tap) = &state.tap
                && tap.kernel()
            {
                if let Err(error) = tap.kick(index) {
                    state.tap_error = Some(error.to_string());
                }
                return;
            }
        }
        if usize::from(index) < self.queue_sizes.len() {
            self.wake_worker();
        }
    }

    fn reset(&mut self) {
        self.pending_queues = None;
        self.negotiated.store(0, Ordering::SeqCst);
        let mut sh = lock(&self.shared);
        {
            let Shared {
                tap,
                queues,
                tap_error,
                ..
            } = &mut *sh;
            if let Some(tap) = tap
                && let Err(error) = tap.pause(queues)
            {
                *tap_error = Some(error.to_string());
            }
        }
        sh.queues = self.queue_sizes.iter().map(|_| None).collect();
        if let Some(vnet) = sh.vnet.as_mut() {
            vnet.active_pairs = 1;
            vnet.features = 0;
            vnet.steering.clear();
        }
        sh.mem = None;
        sh.interrupt = None;
        sh.rx_pending.clear();
        // The switch stream survives reset. Keep partial frame decoding and
        // finish already-framed output so neither endpoint loses alignment.
    }
}

// ---- Snapshot ----------------------------------------------------------------

/// Snapshot format magic and version.
const SNAPSHOT_MAGIC: &[u8; 5] = b"VLNET";
const SNAPSHOT_VERSION: u16 = 3;

impl Snapshot for Net {
    fn quiesce(&mut self) {
        lock(&self.shared).snapshot_paused = true;
        let (done, wait) = mpsc::channel();
        if self.commands.send(Cmd::Barrier(done)).is_ok() {
            self.wake_worker();
            let _ = wait.recv();
        }
        let mut state = lock(&self.shared);
        let Shared {
            tap,
            queues,
            tap_error,
            interrupt,
            ..
        } = &mut *state;
        if let Some(tap) = tap {
            if let Err(error) = tap.pause(queues) {
                *tap_error = Some(error.to_string());
            }
            match tap.drain_calls() {
                Ok(calls) => {
                    for index in calls {
                        if let Some(irq) = interrupt.as_ref() {
                            irq.notify_queue(index);
                        }
                    }
                }
                Err(error) => *tap_error = Some(error.to_string()),
            }
        }
    }

    fn resume_snapshot(&mut self) {
        let pending = {
            let mut state = lock(&self.shared);
            let (features, pairs) = state
                .vnet
                .as_ref()
                .map_or((0, 1), |vnet| (vnet.features, vnet.active_pairs));
            let Shared {
                tap,
                queues,
                tap_error,
                mem,
                ..
            } = &mut *state;
            if mem.is_some()
                && let Some(tap) = tap
                && let Err(error) = tap.configure(queues, features, pairs)
            {
                *tap_error = Some(error.to_string());
            }
            state.snapshot_paused = false;
            std::mem::take(&mut state.config_pending)
        };
        if pending {
            fire_config(&self.shared);
        }
        for queue in 0..self.queue_sizes.len() as u16 {
            self.queue_notify(queue);
        }
    }

    fn state_id(&self) -> String {
        self.config.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let sh = lock(&self.shared);
        if let Some(error) = &sh.tap_error {
            return Err(DeviceError::State(format!("TAP backend: {error}")));
        }
        let mut enc = Enc::new();
        enc.raw(SNAPSHOT_MAGIC);
        enc.u16(SNAPSHOT_VERSION);
        enc.u8(if sh.tap_mode {
            if sh.tap.as_ref().is_some_and(|tap| tap.kernel()) {
                2
            } else {
                1
            }
        } else {
            0
        });
        match &sh.vnet {
            None => enc.u8(0),
            Some(vnet) => {
                enc.u8(1);
                enc.u16(vnet.max_pairs);
                enc.u16(vnet.active_pairs);
                enc.u64(vnet.features);
                enc.u32(vnet.steering.len() as u32);
                let mut steering: Vec<_> = vnet.steering.iter().collect();
                steering.sort_unstable_by_key(|(hash, _)| **hash);
                for (&hash, &pair) in steering {
                    enc.u32(hash);
                    enc.u16(pair);
                }
            }
        }
        enc.u8(sh.link_up as u8);
        enc.bytes(&sh.tx_out);
        match &sh.rx {
            RxRead::Idle => enc.u8(0),
            RxRead::Prefix { buf, got } => {
                enc.u8(1);
                enc.bytes(buf);
                enc.u32(*got as u32);
            }
            RxRead::Body { buf, got } => {
                enc.u8(2);
                enc.bytes(buf);
                enc.u32(*got as u32);
            }
        }
        enc.u32(sh.rx_pending.len() as u32);
        for frame in &sh.rx_pending {
            enc.bytes(frame);
        }
        enc.u32(sh.queues.len() as u32);
        for queue in &sh.queues {
            match queue {
                Some(queue) => {
                    enc.u8(1);
                    enc_queue(&mut enc, &queue.state());
                }
                None => enc.u8(0),
            }
        }
        Ok(enc.0)
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let mut dec = Dec::new(state);
        if dec.raw(SNAPSHOT_MAGIC.len())? != SNAPSHOT_MAGIC.as_slice() {
            return Err(DeviceError::State("not a virtio-net snapshot".into()));
        }
        let version = dec.u16()?;
        if !(1..=SNAPSHOT_VERSION).contains(&version) {
            return Err(DeviceError::State(
                "unsupported virtio-net snapshot version".into(),
            ));
        }
        let backend_mode = if version >= 3 { dec.u8()? } else { 0 };
        {
            let state = lock(&self.shared);
            let expected = if state.tap_mode {
                if state.tap.as_ref().is_some_and(|tap| tap.kernel()) {
                    2
                } else {
                    1
                }
            } else {
                0
            };
            if backend_mode != expected || (state.tap_mode && state.tap.is_none()) {
                return Err(DeviceError::State(
                    "snapshot network backend mode differs or TAP descriptors are missing".into(),
                ));
            }
        }
        let vnet = if version >= 2 && dec.u8()? != 0 {
            let max_pairs = dec.u16()?;
            let active_pairs = dec.u16()?;
            let features = dec.u64()?;
            let count = dec.u32()?;
            if !(1..=31).contains(&max_pairs)
                || !(1..=max_pairs).contains(&active_pairs)
                || count > 65536
            {
                return Err(DeviceError::State("invalid virtio-net queue state".into()));
            }
            let mut steering = HashMap::new();
            for _ in 0..count {
                let hash = dec.u32()?;
                let pair = dec.u16()?;
                if pair >= active_pairs {
                    return Err(DeviceError::State("invalid receive steering queue".into()));
                }
                steering.insert(hash, pair);
            }
            Some(VnetState {
                max_pairs,
                active_pairs,
                features,
                mtu: self.config.mtu,
                steering,
            })
        } else {
            None
        };
        if vnet.as_ref().map(|state| state.max_pairs)
            != lock(&self.shared)
                .vnet
                .as_ref()
                .map(|state| state.max_pairs)
        {
            return Err(DeviceError::State(
                "snapshot network framing or queue pairs differ".into(),
            ));
        }
        let link_up = dec.u8()? == 1;
        let tx_out = dec.bytes()?;
        let rx = match dec.u8()? {
            0 => RxRead::Idle,
            1 => RxRead::Prefix {
                buf: dec
                    .bytes()?
                    .try_into()
                    .map_err(|_| DeviceError::State("bad rx prefix in snapshot".into()))?,
                got: dec.u32()? as usize,
            },
            2 => RxRead::Body {
                buf: dec.bytes()?,
                got: dec.u32()? as usize,
            },
            _ => return Err(DeviceError::State("bad rx state in snapshot".into())),
        };
        let npending = dec.u32()? as usize;
        let mut rx_pending = VecDeque::new();
        for _ in 0..npending {
            rx_pending.push_back(dec.bytes()?);
        }
        let nqueues = dec.u32()? as usize;
        if nqueues != self.queue_sizes.len() {
            return Err(DeviceError::State(format!(
                "snapshot has {nqueues} queues, device needs {}",
                self.queue_sizes.len()
            )));
        }
        let mut queues = Vec::with_capacity(nqueues);
        for _ in 0..nqueues {
            queues.push(if dec.u8()? == 1 {
                Some(dec_queue(&mut dec)?)
            } else {
                None
            });
        }
        let mut sh = lock(&self.shared);
        sh.vnet = vnet;
        sh.link_up = link_up;
        sh.tx_out = tx_out;
        sh.rx = rx;
        sh.rx_pending = rx_pending;
        if sh.mem.is_some() {
            sh.queues = queues
                .into_iter()
                .enumerate()
                .map(|(i, state)| {
                    state
                        .map(Queue::try_from)
                        .transpose()
                        .map_err(|e| DeviceError::State(format!("queue {i}: {e}")))
                })
                .collect::<Result<_, _>>()?;
            self.pending_queues = None;
        } else {
            self.pending_queues = Some(queues);
        }
        drop(sh);
        self.wake_worker();
        Ok(())
    }
}

/// Encode one queue's [`QueueState`].
fn enc_queue(enc: &mut Enc, state: &QueueState) {
    enc.u16(state.max_size);
    enc.u16(state.next_avail);
    enc.u16(state.next_used);
    enc.u8(state.event_idx_enabled as u8);
    enc.u16(state.size);
    enc.u8(state.ready as u8);
    enc.u64(state.desc_table);
    enc.u64(state.avail_ring);
    enc.u64(state.used_ring);
}

/// Decode one queue's [`QueueState`].
fn dec_queue(dec: &mut Dec<'_>) -> Result<QueueState, DeviceError> {
    Ok(QueueState {
        max_size: dec.u16()?,
        next_avail: dec.u16()?,
        next_used: dec.u16()?,
        event_idx_enabled: dec.u8()? == 1,
        size: dec.u16()?,
        ready: dec.u8()? == 1,
        desc_table: dec.u64()?,
        avail_ring: dec.u64()?,
        used_ring: dec.u64()?,
    })
}

/// Minimal little-endian encoder for this crate's device-state sections.
struct Enc(Vec<u8>);

impl Enc {
    fn new() -> Self {
        Enc(Vec::new())
    }
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn raw(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }
    fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
    }
}

/// Minimal little-endian decoder for this crate's device-state sections.
struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Dec<'_> {
    fn new(buf: &[u8]) -> Dec<'_> {
        Dec { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&[u8], DeviceError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| DeviceError::State("overflowed virtio-net snapshot".into()))?;
        if end > self.buf.len() {
            return Err(DeviceError::State("truncated virtio-net snapshot".into()));
        }
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, DeviceError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, DeviceError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes(b.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, DeviceError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, DeviceError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }
    fn raw(&mut self, n: usize) -> Result<&[u8], DeviceError> {
        self.take(n)
    }
    fn bytes(&mut self) -> Result<Vec<u8>, DeviceError> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    include!("net/tap_integration_tests.rs");
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;
    use vm_memory::{Bytes, GuestAddress};

    fn wire_sum(bytes: &[u8]) -> u16 {
        let mut sum: u32 = bytes
            .chunks(2)
            .map(|pair| u32::from(u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)])))
            .sum();
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        sum as u16
    }

    fn checksum_frame(partial: bool) -> Vec<u8> {
        let mut frame = vec![0; 14 + 20 + 8 + 57];
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&85u16.to_be_bytes());
        frame[22] = 64;
        frame[23] = 17;
        frame[26..34].copy_from_slice(&[192, 0, 2, 1, 198, 51, 100, 2]);
        frame[34..38].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        frame[38..40].copy_from_slice(&65u16.to_be_bytes());
        for (n, byte) in frame[42..].iter_mut().enumerate() {
            *byte = n as u8;
        }
        let checksum = !wire_sum(&frame[14..34]);
        frame[24..26].copy_from_slice(&checksum.to_be_bytes());
        let mut pseudo = frame[26..34].to_vec();
        pseudo.extend_from_slice(&[0, 17, 0, 65]);
        let checksum = if partial {
            wire_sum(&pseudo)
        } else {
            pseudo.extend_from_slice(&frame[34..]);
            !wire_sum(&pseudo)
        };
        frame[40..42].copy_from_slice(&checksum.to_be_bytes());
        frame
    }

    #[test]
    fn transmit_completes_partial_checksum_without_changing_guest_memory() {
        let frame = checksum_frame(true);
        let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();
        mem.write_slice(&frame, GuestAddress(0)).unwrap();
        let mut packet = TxPacket {
            head: 0,
            consumed: (12 + frame.len()) as u32,
            prefix: Some((frame.len() as u32).to_be_bytes()),
            owned: None,
            slices: vec![
                mem.get_slice(GuestAddress(0), 35).unwrap(),
                mem.get_slice(GuestAddress(35), frame.len() - 35).unwrap(),
            ],
        };
        let mut header = [0; 12];
        header[0] = HDR_F_NEEDS_CSUM;
        header[6..8].copy_from_slice(&34u16.to_le_bytes());
        header[8..10].copy_from_slice(&6u16.to_le_bytes());
        assert!(complete_tx_checksum(
            &mut packet,
            &header,
            frame.len(),
            true
        ));
        assert_eq!(packet.owned.as_ref().unwrap(), &checksum_frame(false));
        let mut original = vec![0; frame.len()];
        mem.read_slice(&mut original, GuestAddress(0)).unwrap();
        assert_eq!(original, frame);
        let (producer, mut receiver) = UnixStream::pair().unwrap();
        producer.set_nonblocking(true).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut output = Vec::new();
        send_tx_batch(&producer, &[packet], &mut output);
        assert!(output.is_empty());
        let mut wire = vec![0; frame.len() + 4];
        receiver.read_exact(&mut wire).unwrap();
        assert_eq!(&wire[..4], &(frame.len() as u32).to_be_bytes());
        assert_eq!(&wire[4..], &checksum_frame(false));
    }

    #[test]
    fn owned_checksum_payload_survives_short_writes() {
        for field in [0usize, 1, 40, 41, 8191, MAX_FRAME - 2] {
            let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 128 * 1024)]).unwrap();
            let bytes: Vec<_> = (0..MAX_FRAME).map(|n| (n % 251) as u8).collect();
            mem.write_slice(&bytes, GuestAddress(0)).unwrap();
            let mut owned = bytes.clone();
            owned[field..field + 2].copy_from_slice(&[0x12, 0x34]);
            let packet = TxPacket {
                head: 0,
                consumed: (12 + MAX_FRAME) as u32,
                prefix: Some((MAX_FRAME as u32).to_be_bytes()),
                owned: Some(owned),
                slices: Vec::new(),
            };
            let (producer, mut receiver) = UnixStream::pair().unwrap();
            producer.set_nonblocking(true).unwrap();
            receiver
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let size: libc::c_int = 4096;
            // SAFETY: producer and the initialized integer option stay live.
            let result = unsafe {
                libc::setsockopt(
                    producer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    std::ptr::from_ref(&size).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            };
            assert_eq!(result, 0);
            let mut output = Vec::new();
            send_tx_batch(&producer, &[packet], &mut output);
            assert!(!output.is_empty());
            let mut expected = (MAX_FRAME as u32).to_be_bytes().to_vec();
            expected.extend_from_slice(&bytes);
            expected[4 + field..4 + field + 2].copy_from_slice(&[0x12, 0x34]);
            let sent = expected.len() - output.len();
            mem.write_slice(&vec![0; MAX_FRAME], GuestAddress(0))
                .unwrap();
            let mut actual = vec![0; sent];
            receiver.read_exact(&mut actual).unwrap();
            actual.extend_from_slice(&output);
            assert_eq!(actual, expected, "checksum field {field}");
        }
    }

    #[test]
    fn invalid_checksum_metadata_and_unoffered_segmentation_do_not_reach_the_socket() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.dev.quiesce();
        lock(&f.dev.shared).snapshot_paused = false;
        let (producer, mut receiver) = UnixStream::pair().unwrap();
        producer.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let mut stream = Some(producer);
        for (index, csum, gso, start, offset) in [
            (0, true, 0, 99u16, 0u16),
            (1, true, 0, 34, u16::MAX),
            (2, false, 0, 34, 6),
            (3, true, 1, 34, 6),
        ] {
            f.queue_tx(&checksum_frame(true));
            let mut header = [0; 12];
            header[0] = HDR_F_NEEDS_CSUM;
            header[1] = gso;
            header[6..8].copy_from_slice(&start.to_le_bytes());
            header[8..10].copy_from_slice(&offset.to_le_bytes());
            f.mem.write_slice(&header, GuestAddress(f.data(0))).unwrap();
            process_tx(&f.dev.shared, &mut stream, 12, csum);
            assert_eq!(f.vqs[1].used_idx(&f.mem), index + 1);
            assert!(lock(&f.dev.shared).tx_out.is_empty());
            assert_eq!(
                receiver.read(&mut [0; 128]).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
    }

    #[test]
    fn receive_marks_only_valid_negotiated_checksums_before_publication() {
        for mergeable in [false, true] {
            for guest_csum in [false, true] {
                for corrupt in [false, true] {
                    let mut f = Fixture::new();
                    let _switch = f.take_switch_side();
                    f.dev.quiesce();
                    lock(&f.dev.shared).snapshot_paused = false;
                    let mut frame = checksum_frame(false);
                    if corrupt {
                        *frame.last_mut().unwrap() ^= 1;
                    }
                    let address = f.data(2);
                    let additional = f.data(3);
                    f.vqs[0].add_chain(
                        &f.mem,
                        &[
                            (address, 5, true),
                            (address + 5, if mergeable { 20 } else { 251 }, true),
                        ],
                    );
                    if mergeable {
                        f.vqs[0].add_chain(&f.mem, &[(additional, 256, true)]);
                    }
                    lock(&f.dev.shared).rx_pending.push_back(frame.clone());
                    place_rx_frames(&f.dev.shared, 12, mergeable, guest_csum);
                    assert_eq!(f.vqs[0].used_idx(&f.mem), if mergeable { 2 } else { 1 });
                    let flags = f.mem.read_obj::<u8>(GuestAddress(address)).unwrap();
                    assert_eq!(
                        flags,
                        if guest_csum && !corrupt {
                            HDR_F_DATA_VALID
                        } else {
                            0
                        }
                    );
                    let mut actual = vec![0; frame.len()];
                    let first = if mergeable { 13 } else { frame.len() };
                    f.mem
                        .read_slice(&mut actual[..first], GuestAddress(address + 12))
                        .unwrap();
                    if mergeable {
                        f.mem
                            .read_slice(&mut actual[first..], GuestAddress(f.data(3)))
                            .unwrap();
                    }
                    assert_eq!(actual, frame);
                }
            }
        }
    }

    #[test]
    fn direct_receive_checksums_valid_and_corrupt_packets_before_publishing() {
        for guest_csum in [false, true] {
            for corrupt in [false, true] {
                let mut f = Fixture::new();
                let _switch = f.take_switch_side();
                f.dev.quiesce();
                lock(&f.dev.shared).snapshot_paused = false;
                let address = f.data(2);
                f.vqs[0].add_chain(&f.mem, &[(address, 256, true)]);
                let mut frame = checksum_frame(false);
                if corrupt {
                    *frame.last_mut().unwrap() ^= 1;
                }
                let (mut producer, mut receiver) = UnixStream::pair().unwrap();
                receiver.set_nonblocking(true).unwrap();
                producer
                    .write_all(&(frame.len() as u32).to_be_bytes())
                    .unwrap();
                producer.write_all(&frame).unwrap();
                assert_eq!(
                    read_guest_frame(&f.dev.shared, &mut receiver, 12, true, guest_csum),
                    Some(Ok(()))
                );
                assert_eq!(f.vqs[0].used_idx(&f.mem), 1);
                assert_eq!(
                    f.mem.read_obj::<u8>(GuestAddress(address)).unwrap(),
                    if guest_csum && !corrupt {
                        HDR_F_DATA_VALID
                    } else {
                        0
                    }
                );
            }
        }
    }

    #[test]
    fn coalesces_worker_wakes_and_rearms_after_drain() {
        let (sender, mut receiver) = UnixStream::pair().unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let pending = AtomicBool::new(false);
        for _ in 0..200 {
            notify_worker(&sender, &pending);
        }
        let mut bytes = [0; 256];
        assert_eq!(receiver.read(&mut bytes).unwrap(), 1);
        assert_eq!(
            receiver.read(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        pending.store(false, Ordering::Release);
        notify_worker(&sender, &pending);
        assert_eq!(receiver.read(&mut bytes).unwrap(), 1);
    }
    #[test]
    fn direct_receive_handles_split_headers_and_mergeable_buffers() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.dev.quiesce();
        lock(&f.dev.shared).snapshot_paused = false;
        let first = f.data(2);
        let second = f.data(3);
        let third = f.data(4);
        f.vqs[0].add_chain(&f.mem, &[(first, 5, true), (second, 20, true)]);
        f.vqs[0].add_chain(&f.mem, &[(third, 256, true)]);
        let frame: Vec<_> = (0..128).map(|n| n as u8).collect();
        let (mut producer, mut receiver) = UnixStream::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        producer
            .write_all(&(frame.len() as u32).to_be_bytes())
            .unwrap();
        producer.write_all(&frame).unwrap();
        assert_eq!(
            read_guest_frame(&f.dev.shared, &mut receiver, 12, true, false),
            Some(Ok(()))
        );
        assert_eq!(f.vqs[0].used_idx(&f.mem), 2);
        assert_eq!(f.vqs[0].used_entry(&f.mem, 0).1, 25);
        assert_eq!(f.vqs[0].used_entry(&f.mem, 1).1, 115);
        let mut bytes = [0; 140];
        f.mem
            .read_slice(&mut bytes[..5], GuestAddress(first))
            .unwrap();
        f.mem
            .read_slice(&mut bytes[5..25], GuestAddress(second))
            .unwrap();
        f.mem
            .read_slice(&mut bytes[25..], GuestAddress(third))
            .unwrap();
        assert_eq!(&bytes[..10], &[0; 10]);
        assert_eq!(&bytes[10..12], &2u16.to_le_bytes());
        assert_eq!(&bytes[12..], &frame);
    }

    #[test]
    fn partial_direct_receive_survives_snapshot_before_publication() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.dev.quiesce();
        lock(&f.dev.shared).snapshot_paused = false;
        let address = f.data(2);
        f.vqs[0].add_chain(&f.mem, &[(address, 256, true)]);
        let frame: Vec<_> = (0..128).map(|n| n as u8).collect();
        let (mut producer, mut receiver) = UnixStream::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        producer
            .write_all(&(frame.len() as u32).to_be_bytes())
            .unwrap();
        producer.write_all(&frame[..37]).unwrap();
        assert_eq!(
            read_guest_frame(&f.dev.shared, &mut receiver, 12, true, false),
            Some(Ok(()))
        );
        assert_eq!(f.vqs[0].used_idx(&f.mem), 0);
        assert!(matches!(
            lock(&f.dev.shared).rx,
            RxRead::Body { got: 37, .. }
        ));
        f.dev.quiesce();
        let snapshot = f.dev.save_state().unwrap();
        lock(&f.dev.shared).rx = RxRead::Idle;
        f.dev.restore_state(&snapshot).unwrap();
        producer.write_all(&frame[37..]).unwrap();
        read_frames(&f.dev.shared, &mut receiver).unwrap();
        lock(&f.dev.shared).snapshot_paused = false;
        place_rx_frames(&f.dev.shared, 12, true, false);
        assert_eq!(f.vqs[0].used_idx(&f.mem), 1);
        let mut received = vec![0; frame.len()];
        f.mem
            .read_slice(&mut received, GuestAddress(address + 12))
            .unwrap();
        assert_eq!(received, frame);
    }

    #[test]
    fn transmit_batch_preserves_framing_and_owns_every_unsent_guest_byte() {
        let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 128 * 1024)]).unwrap();
        let mut expected = Vec::new();
        let mut packets = Vec::new();
        let mut address = 0;
        for (head, len) in [1usize, MAX_FRAME, 17].into_iter().enumerate() {
            let bytes: Vec<_> = (0..len).map(|n| (n % 251) as u8).collect();
            mem.write_slice(&bytes, GuestAddress(address)).unwrap();
            let prefix = (len as u32).to_be_bytes();
            expected.extend_from_slice(&prefix);
            expected.extend_from_slice(&bytes);
            packets.push(TxPacket {
                head: head as u16,
                consumed: (12 + len) as u32,
                prefix: Some(prefix),
                slices: vec![mem.get_slice(GuestAddress(address), len).unwrap()],
                owned: None,
            });
            address += len as u64;
        }
        let (producer, mut receiver) = UnixStream::pair().unwrap();
        producer.set_nonblocking(true).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let size: libc::c_int = 4096;
        // SAFETY: producer is live and size is an initialized integer option.
        let result = unsafe {
            libc::setsockopt(
                producer.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                std::ptr::from_ref(&size).cast(),
                std::mem::size_of_val(&size) as libc::socklen_t,
            )
        };
        assert_eq!(result, 0);
        let mut output = Vec::new();
        send_tx_batch(&producer, &packets, &mut output);
        assert!(!output.is_empty());
        let sent = expected.len() - output.len();
        assert!(sent > 5);
        mem.write_slice(&vec![0; address as usize], GuestAddress(0))
            .unwrap();
        let mut received = vec![0; sent];
        receiver.read_exact(&mut received).unwrap();
        received.extend_from_slice(&output);
        assert_eq!(received, expected);
    }
    /// Guest memory: 288 MiB, room for the queues' rings plus buffers.
    const MEM_SIZE: u64 = 288 * 1024 * 1024;
    /// Where the tests put data buffers, above the rings.
    const DATA_BASE: u64 = 0x1000_0000;
    /// A short reconnect interval so the tests do not crawl.
    const RETRY: Duration = Duration::from_millis(50);

    const VRING_DESC_F_NEXT: u16 = 1;
    const VRING_DESC_F_WRITE: u16 = 2;

    static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

    /// A throwaway directory for the switch socket.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            let path = std::env::temp_dir().join(format!(
                "vmlab-hv-net-{}-{}",
                std::process::id(),
                DIR_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Records queue and config interrupts; the test double the brief asks
    /// for.
    #[derive(Default)]
    struct TestInterrupt {
        queues: Mutex<Vec<u16>>,
        config: AtomicU32,
    }

    impl TestInterrupt {
        fn config_count(&self) -> u32 {
            self.config.load(Ordering::Relaxed)
        }
    }

    impl VirtioInterrupt for TestInterrupt {
        fn notify_queue(&self, queue: u16) {
            self.queues.lock().push(queue);
        }
        fn notify_config(&self) {
            self.config.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Poll `cond` until it holds or five seconds pass, then panic.
    fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            if Instant::now() > deadline {
                panic!("timed out waiting for {what}");
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// A driver-side split virtqueue, hand-built in guest memory.
    struct TestVq {
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
        next_desc: u16,
    }

    impl TestVq {
        fn new(base: u64, size: u16) -> TestVq {
            let desc = base;
            let avail = (desc + 16 * u64::from(size) + 15) & !15;
            let used = (avail + 8 + 2 * u64::from(size) + 3) & !3;
            TestVq {
                size,
                desc,
                avail,
                used,
                next_desc: 0,
            }
        }

        /// A device-side `Queue` configured against this ring.
        fn queue(&self) -> Queue {
            let mut q = Queue::new(self.size).unwrap();
            q.try_set_size(self.size).unwrap();
            q.try_set_desc_table_address(GuestAddress(self.desc))
                .unwrap();
            q.try_set_avail_ring_address(GuestAddress(self.avail))
                .unwrap();
            q.try_set_used_ring_address(GuestAddress(self.used))
                .unwrap();
            q.set_event_idx(false);
            q.set_ready(true);
            q
        }

        /// Add one descriptor chain of `(addr, len, device-writable)` legs.
        fn add_chain(&mut self, mem: &GuestMemory, descs: &[(u64, u32, bool)]) -> u16 {
            let head = self.next_desc;
            for (j, &(addr, len, write)) in descs.iter().enumerate() {
                let idx = head + j as u16;
                let last = j + 1 == descs.len();
                let mut flags = if write { VRING_DESC_F_WRITE } else { 0 };
                if !last {
                    flags |= VRING_DESC_F_NEXT;
                }
                let next = if last { 0 } else { idx + 1 };
                let mut raw = [0u8; 16];
                raw[0..8].copy_from_slice(&addr.to_le_bytes());
                raw[8..12].copy_from_slice(&len.to_le_bytes());
                raw[12..14].copy_from_slice(&flags.to_le_bytes());
                raw[14..16].copy_from_slice(&next.to_le_bytes());
                mem.write_slice(&raw, GuestAddress(self.desc + 16 * u64::from(idx)))
                    .unwrap();
            }
            self.next_desc += descs.len() as u16;
            let avail_idx: u16 = mem.read_obj(GuestAddress(self.avail + 2)).unwrap();
            mem.write_obj(
                head,
                GuestAddress(self.avail + 4 + 2 * u64::from(avail_idx)),
            )
            .unwrap();
            mem.write_obj(avail_idx.wrapping_add(1), GuestAddress(self.avail + 2))
                .unwrap();
            head
        }

        fn used_idx(&self, mem: &GuestMemory) -> u16 {
            mem.read_obj::<u16>(GuestAddress(self.used + 2)).unwrap()
        }

        /// `(head, written length)` of one used entry.
        fn used_entry(&self, mem: &GuestMemory, i: u16) -> (u32, u32) {
            let base = self.used + 4 + 8 * u64::from(i);
            (
                mem.read_obj(GuestAddress(base)).unwrap(),
                mem.read_obj(GuestAddress(base + 4)).unwrap(),
            )
        }
    }

    fn gso_packet(tcp: bool, payload: usize) -> (Header, Vec<u8>) {
        let mut frame = vec![0; 34 + if tcp { 20 } else { 8 } + payload];
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        frame[14] = 0x45;
        frame[23] = if tcp { 6 } else { 17 };
        frame[26..34].copy_from_slice(&[10, 0, 0, 1, 10, 0, 0, 2]);
        frame[34..38].copy_from_slice(&[0, 80, 1, 2]);
        if tcp {
            frame[46] = 0x50;
            frame[47] = 0x18;
        }
        let header = Header {
            flags: NEEDS_CSUM,
            gso_type: if tcp { GSO_TCPV4 } else { GSO_UDP },
            hdr_len: if tcp { 54 } else { 42 },
            gso_size: 1000,
            csum_start: 34,
            csum_offset: if tcp { 16 } else { 6 },
        };
        (header, frame)
    }

    fn vnet_body(header: Header, frame: &[u8]) -> Vec<u8> {
        let mut body = header.metadata(0).to_vec();
        body.extend_from_slice(frame);
        body
    }

    #[test]
    fn multiqueue_control_rejects_invalid_counts_and_falls_back_without_mq() {
        let mut f = Fixture::new_mode(Some(2));
        let _socket = f.take_switch_side();
        f.dev.quiesce();
        lock(&f.dev.shared).snapshot_paused = false;
        assert_eq!(f.dev.queue_max_sizes(), &[256; 5]);
        assert_eq!(f.control_pairs(0, 4), 1);
        assert_eq!(f.control_pairs(3, 4), 1);
        assert_eq!(f.control_pairs(2, 4), 0);
        assert_eq!(lock(&f.dev.shared).vnet.as_ref().unwrap().active_pairs, 2);
        assert_eq!(f.control_pairs(1, 4), 0);
        f.dev.ack_features(f.dev.features() & !F_MQ);
        assert_eq!(f.control_pairs(2, 2), 1);
        assert_eq!(lock(&f.dev.shared).vnet.as_ref().unwrap().active_pairs, 1);
    }

    #[test]
    fn vnet_transmit_preserves_tso_uso_headers_and_queue_ids() {
        for tcp in [true, false] {
            let mut f = Fixture::new_mode(Some(2));
            let mut socket = f.take_switch_side();
            f.dev.quiesce();
            lock(&f.dev.shared).snapshot_paused = false;
            assert_eq!(f.control_pairs(2, 4), 0);
            let (header, frame) = gso_packet(tcp, 5000);
            let address = f.data(0);
            f.mem
                .write_slice(&header.encode(), GuestAddress(address))
                .unwrap();
            f.mem
                .write_slice(&frame, GuestAddress(address + 12))
                .unwrap();
            f.vqs[3].add_chain(&f.mem, &[(address, (12 + frame.len()) as u32, false)]);
            f.dev.queue_notify(3);
            let mut prefix = [0; 4];
            socket.read_exact(&mut prefix).unwrap();
            let mut body = vec![0; u32::from_be_bytes(prefix) as usize];
            socket.read_exact(&mut body).unwrap();
            let (actual_header, pair, actual_frame) = Header::from_metadata(&body).unwrap();
            assert_eq!(
                (actual_header, pair, actual_frame),
                (header, 1, frame.as_slice())
            );
            assert_eq!(f.vqs[3].used_idx(&f.mem), 1);
        }
    }

    #[test]
    fn vnet_receive_preserves_gso_or_segments_when_guest_declines() {
        for tcp in [true, false] {
            for supported in [true, false] {
                let mut f = Fixture::new_mode(Some(1));
                let _socket = f.take_switch_side();
                f.dev.quiesce();
                lock(&f.dev.shared).snapshot_paused = false;
                if !supported {
                    f.dev.ack_features(
                        f.dev.features()
                            & !(F_GUEST_TSO4 | F_GUEST_TSO6 | F_GUEST_USO4 | F_GUEST_USO6),
                    );
                }
                let (header, frame) = gso_packet(tcp, 5000);
                for index in 0..8 {
                    let address = f.data(index);
                    f.vqs[0].add_chain(&f.mem, &[(address, 8192, true)]);
                }
                lock(&f.dev.shared)
                    .rx_pending
                    .push_back(vnet_body(header, &frame));
                place_rx_frames(&f.dev.shared, 12, true, true);
                let count = f.vqs[0].used_idx(&f.mem);
                assert_eq!(count, if supported { 1 } else { 5 });
                let mut received = [0; 12];
                f.mem
                    .read_slice(&mut received, GuestAddress(f.data(0)))
                    .unwrap();
                let actual = Header::decode(&received).unwrap();
                assert_eq!(
                    actual.gso_type,
                    if supported { header.gso_type } else { GSO_NONE }
                );
                if supported {
                    assert_eq!(actual, header);
                } else {
                    assert_eq!(actual.flags, HDR_F_DATA_VALID);
                }
                assert!(lock(&f.dev.shared).rx_pending.is_empty());
            }
        }
    }

    #[test]
    fn vnet_receive_budget_accepts_bursts_and_bounds_stalled_queue_bytes() {
        let mut f = Fixture::new_mode(Some(1));
        let _socket = f.take_switch_side();
        f.dev.quiesce();
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        let (_, frame) = gso_packet(true, 64);
        let body = vnet_body(Header::default(), &frame);
        let mut burst = Vec::new();
        for _ in 0..64 {
            burst.extend_from_slice(&(body.len() as u32).to_be_bytes());
            burst.extend_from_slice(&body);
        }
        sender.write_all(&burst).unwrap();
        read_frames(&f.dev.shared, &mut receiver).unwrap();
        assert_eq!(lock(&f.dev.shared).rx_pending.len(), 64);
        lock(&f.dev.shared).rx_pending.clear();
        let body = vnet_body(Header::default(), &vec![0; MAX_FRAME]);
        for _ in 0..32 {
            sender
                .write_all(&(body.len() as u32).to_be_bytes())
                .unwrap();
            sender.write_all(&body).unwrap();
            read_frames(&f.dev.shared, &mut receiver).unwrap();
        }
        let state = lock(&f.dev.shared);
        assert!(state.rx_pending.iter().map(Vec::len).sum::<usize>() <= MAX_TX_STAGE);
        assert!(state.rx_pending.len() >= 15);
    }

    #[test]
    fn malformed_control_available_index_returns_without_spinning() {
        let mut f = Fixture::new_mode(Some(2));
        let _socket = f.take_switch_side();
        f.dev.quiesce();
        lock(&f.dev.shared).snapshot_paused = false;
        f.mem
            .write_obj(257u16, GuestAddress(f.vqs[4].avail + 2))
            .unwrap();
        process_control(&f.dev.shared);
        assert_eq!(f.vqs[4].used_idx(&f.mem), 0);
    }

    #[test]
    fn data_valid_is_never_accepted_from_guest_or_sent_without_negotiation() {
        let mut f = Fixture::new_mode(Some(1));
        let _socket = f.take_switch_side();
        f.dev.quiesce();
        lock(&f.dev.shared).snapshot_paused = false;
        let (_, frame) = gso_packet(true, 64);
        let metadata = Header {
            flags: HDR_F_DATA_VALID,
            ..Header::default()
        };
        let slice = f
            .mem
            .get_slice(GuestAddress(f.data(0)), frame.len())
            .unwrap();
        slice.copy_from(&frame);
        let mut packet = TxPacket {
            head: 0,
            consumed: 0,
            prefix: None,
            slices: vec![slice],
            owned: None,
        };
        assert!(!prepare_vnet_tx(
            &mut packet,
            &metadata.encode(),
            frame.len(),
            0,
            lock(&f.dev.shared).vnet.as_mut().unwrap()
        ));
        f.dev.ack_features(f.dev.features() & !F_GUEST_CSUM);
        let address = f.data(1);
        f.vqs[0].add_chain(&f.mem, &[(address, 1024, true)]);
        lock(&f.dev.shared)
            .rx_pending
            .push_back(vnet_body(metadata, &frame));
        place_rx_frames(&f.dev.shared, 12, true, false);
        assert_eq!(f.vqs[0].used_idx(&f.mem), 1);
        assert_eq!(f.mem.read_obj::<u8>(GuestAddress(address)).unwrap(), 0);
    }

    #[test]
    fn blocked_vnet_queue_keeps_socket_readable_for_other_flows() {
        let mut f = Fixture::new_mode(Some(2));
        let mut socket = f.take_switch_side();
        f.dev.quiesce();
        lock(&f.dev.shared).snapshot_paused = false;
        assert_eq!(f.control_pairs(2, 4), 0);
        let (_, blocked) = gso_packet(true, 128);
        let mut ready = blocked.clone();
        ready[35] ^= 1;
        {
            let mut state = lock(&f.dev.shared);
            let steering = &mut state.vnet.as_mut().unwrap().steering;
            steering.insert(flow_hash(&blocked), 0);
            steering.insert(flow_hash(&ready), 1);
        }
        let address = f.data(0);
        f.vqs[2].add_chain(&f.mem, &[(address, 1024, true)]);
        let body = vnet_body(Header::default(), &blocked);
        socket
            .write_all(&(body.len() as u32).to_be_bytes())
            .unwrap();
        socket.write_all(&body).unwrap();
        wait_for("blocked flow buffered", || {
            !lock(&f.dev.shared).rx_pending.is_empty()
        });
        let body = vnet_body(Header::default(), &ready);
        socket
            .write_all(&(body.len() as u32).to_be_bytes())
            .unwrap();
        socket.write_all(&body).unwrap();
        wait_for("unblocked queue receives later socket packet", || {
            f.vqs[2].used_idx(&f.mem) == 1
        });
        assert_eq!(f.vqs[0].used_idx(&f.mem), 0);
    }

    #[test]
    fn multiqueue_steering_avoids_blocked_queue_and_survives_snapshot() {
        let mut f = Fixture::new_mode(Some(2));
        let _socket = f.take_switch_side();
        f.dev.quiesce();
        lock(&f.dev.shared).snapshot_paused = false;
        assert_eq!(f.control_pairs(2, 4), 0);
        let (_, mut frame) = gso_packet(true, 128);
        let first = flow_hash(&frame);
        let mut blocked = frame.clone();
        blocked[35] ^= 1;
        let other = flow_hash(&blocked);
        {
            let mut state = lock(&f.dev.shared);
            let vnet = state.vnet.as_mut().unwrap();
            vnet.steering.insert(first, 1);
            vnet.steering.insert(other, 0);
            state
                .rx_pending
                .push_back(vnet_body(Header::default(), &blocked));
            state
                .rx_pending
                .push_back(vnet_body(Header::default(), &frame));
        }
        let address = f.data(0);
        f.vqs[2].add_chain(&f.mem, &[(address, 1024, true)]);
        place_rx_frames(&f.dev.shared, 12, false, false);
        assert_eq!(f.vqs[2].used_idx(&f.mem), 1);
        assert_eq!(lock(&f.dev.shared).rx_pending.len(), 1);
        let state = f.dev.save_state().unwrap();
        assert_eq!(f.control_pairs(1, 4), 0);
        f.dev.restore_state(&state).unwrap();
        let state = lock(&f.dev.shared);
        assert_eq!(state.vnet.as_ref().unwrap().active_pairs, 2);
        assert_eq!(state.vnet.as_ref().unwrap().steering.get(&first), Some(&1));
        assert_eq!(state.rx_pending.len(), 1);
        drop(state);
        frame.swap(26, 30);
        frame.swap(27, 31);
        frame.swap(28, 32);
        frame.swap(29, 33);
        frame.swap(34, 36);
        frame.swap(35, 37);
        assert_eq!(flow_hash(&frame), first);
    }

    /// One NIC wired to a switch listener the tests drive by hand.
    struct Fixture {
        _dir: TempDir,
        switch_path: PathBuf,
        mem: GuestMemory,
        dev: Net,
        irq: Arc<TestInterrupt>,
        vqs: Vec<TestVq>,
        switch: Option<UnixListener>,
        accepted: Option<UnixStream>,
    }

    impl Fixture {
        fn new() -> Fixture {
            Self::new_mode(None)
        }

        fn new_mode(pairs: Option<u16>) -> Fixture {
            let dir = TempDir::new();
            let switch_path = dir.0.join("switch.sock");
            let switch = UnixListener::bind(&switch_path).unwrap();
            switch.set_nonblocking(true).unwrap();
            let dev = Net::create(
                NetConfig {
                    id: "nic0".into(),
                    mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
                    mtu: 1500,
                    switch: switch_path.clone(),
                },
                RETRY,
                pairs,
            )
            .unwrap();
            let mem = GuestMemory::from_ranges(&[(GuestAddress(0), MEM_SIZE as usize)]).unwrap();
            let irq = Arc::new(TestInterrupt::default());
            let vqs = (0..pairs.map_or(2, |pairs| 2 * pairs + 1))
                .map(|index| TestVq::new(0x1000 + u64::from(index) * 0x20000, 256))
                .collect();
            let mut f = Fixture {
                _dir: dir,
                switch_path: switch_path.clone(),
                mem,
                dev,
                irq,
                vqs,
                switch: Some(switch),
                accepted: None,
            };
            // Default fixtures exercise classic interrupt suppression.
            f.dev.ack_features(f.dev.features() & !F_EVENT_IDX);
            f.activate();
            f
        }

        fn activate(&mut self) {
            let queues: Vec<Queue> = self.vqs.iter().map(|v| v.queue()).collect();
            self.dev
                .activate(self.mem.clone(), self.irq.clone(), queues)
                .unwrap();
        }

        /// The address of scratch data buffer `i`.
        fn data(&self, i: u64) -> u64 {
            DATA_BASE + 0x1_0000 * i
        }

        fn control_pairs(&mut self, pairs: u16, index: usize) -> u8 {
            let command = self.data(8);
            let reply = self.data(9);
            self.mem
                .write_slice(
                    &[4, 0, pairs as u8, (pairs >> 8) as u8],
                    GuestAddress(command),
                )
                .unwrap();
            self.mem.write_obj(0xabu8, GuestAddress(reply)).unwrap();
            self.vqs[index].add_chain(&self.mem, &[(command, 4, false), (reply, 1, true)]);
            process_control(&self.dev.shared);
            self.mem.read_obj(GuestAddress(reply)).unwrap()
        }

        /// The config status field.
        fn status(&self) -> u16 {
            let mut bytes = [0u8; 2];
            self.dev.read_config(6, &mut bytes);
            u16::from_le_bytes(bytes)
        }

        /// Wait for the device to connect and hand the switch-side stream
        /// over (owned, so the fixture stays usable while it is held).
        fn take_switch_side(&mut self) -> UnixStream {
            if self.accepted.is_none() {
                let mut stream = None;
                wait_for("device to connect to the switch", || {
                    match self.switch.as_ref().unwrap().accept() {
                        Ok((s, _)) => {
                            stream = Some(s);
                            true
                        }
                        Err(_) => false,
                    }
                });
                self.accepted = stream;
            }
            self.accepted.take().unwrap()
        }

        /// Stop serving the switch: connections die and new ones are
        /// refused, like a switch process that went away.
        fn switch_down(&mut self) {
            self.accepted = None;
            self.switch = None;
            let _ = std::fs::remove_file(&self.switch_path);
        }

        /// Serve the switch again.
        fn switch_up(&mut self) {
            let listener = UnixListener::bind(&self.switch_path).unwrap();
            listener.set_nonblocking(true).unwrap();
            self.switch = Some(listener);
        }

        /// Queue one packet for transmit: a zeroed 12-byte header descriptor
        /// chained to a frame descriptor.
        fn queue_tx(&mut self, frame: &[u8]) {
            let hdr_addr = self.data(0);
            let frame_addr = self.data(1);
            self.mem
                .write_slice(frame, GuestAddress(frame_addr))
                .unwrap();
            self.vqs[1].add_chain(
                &self.mem,
                &[
                    (hdr_addr, 12, false),
                    (frame_addr, frame.len() as u32, false),
                ],
            );
        }
    }

    #[test]
    fn empty_transmit_drain_does_not_interrupt_the_guest() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.irq.queues.lock().clear();
        process_tx(&f.dev.shared, &mut None, 12, false);
        assert_eq!(f.vqs[1].used_idx(&f.mem), 0);
        assert!(f.irq.queues.lock().is_empty());
    }

    #[test]
    fn receive_without_available_buffers_does_not_interrupt_the_guest() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.irq.queues.lock().clear();
        lock(&f.dev.shared)
            .rx_pending
            .push_back(b"waiting for a receive buffer".to_vec());
        place_rx_frames(&f.dev.shared, 12, false, false);
        assert_eq!(f.vqs[0].used_idx(&f.mem), 0);
        assert!(f.irq.queues.lock().is_empty());
    }

    #[test]
    fn driver_can_suppress_and_reenable_transmit_and_receive_interrupts() {
        let mut f = Fixture::new();
        let mut switch = f.take_switch_side();
        switch
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let frame = b"interrupt suppression";
        for suppressed in [true, false] {
            for queue in &f.vqs {
                f.mem
                    .write_obj(u16::from(suppressed).to_le(), GuestAddress(queue.avail))
                    .unwrap();
            }
            f.irq.queues.lock().clear();
            f.dev.resume_snapshot();
            let tx_used = f.vqs[1].used_idx(&f.mem) + 1;
            let rx_used = f.vqs[0].used_idx(&f.mem) + 1;
            f.queue_tx(frame);
            let receive_addr = f.data(2);
            f.vqs[0].add_chain(&f.mem, &[(receive_addr, 128, true)]);
            f.dev.queue_notify(TXQ);
            f.dev.queue_notify(RXQ);
            let mut prefix = [0; 4];
            switch.read_exact(&mut prefix).unwrap();
            assert_eq!(u32::from_be_bytes(prefix), frame.len() as u32);
            let mut received = vec![0; frame.len()];
            switch.read_exact(&mut received).unwrap();
            assert_eq!(received, frame);
            switch
                .write_all(&(frame.len() as u32).to_be_bytes())
                .unwrap();
            switch.write_all(frame).unwrap();
            wait_for("suppressed queue completions", || {
                f.vqs[0].used_idx(&f.mem) == rx_used && f.vqs[1].used_idx(&f.mem) == tx_used
            });
            f.dev.quiesce();
            let interrupts = f.irq.queues.lock();
            if suppressed {
                assert!(
                    interrupts.is_empty(),
                    "suppressed interrupts: {interrupts:?}"
                );
            } else {
                assert!(interrupts.contains(&RXQ));
                assert!(interrupts.contains(&TXQ));
            }
        }
    }

    #[test]
    fn event_idx_ignores_classic_interrupt_suppression() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        lock(&f.dev.shared).queues[usize::from(TXQ)]
            .as_mut()
            .unwrap()
            .set_event_idx(true);
        f.mem
            .write_obj(AVAIL_F_NO_INTERRUPT.to_le(), GuestAddress(f.vqs[1].avail))
            .unwrap();
        f.queue_tx(b"event-index notification");
        f.dev.queue_notify(TXQ);
        wait_for("event-index completion", || f.vqs[1].used_idx(&f.mem) == 1);
        f.dev.quiesce();
        assert!(f.irq.queues.lock().contains(&TXQ));
    }

    #[test]
    fn config_space_carries_mac_status_and_mtu() {
        let dir = TempDir::new();
        let switch = dir.0.join("switch.sock");
        let big = Net::new(NetConfig {
            id: "mtu".into(),
            mac: [0x52, 0x54, 0x00, 0xaa, 0xbb, 0xcc],
            mtu: 9000,
            switch: switch.clone(),
        })
        .unwrap();
        assert_eq!(big.device_type(), 1);
        assert_eq!(big.queue_max_sizes(), &[256, 256]);
        let features = big.features();
        assert_ne!(features & F_MTU, 0);
        assert_ne!(features & F_MAC, 0);
        assert_ne!(features & F_STATUS, 0);
        assert_ne!(features & F_INDIRECT_DESC, 0);
        assert_ne!(features & F_VERSION_1, 0);
        assert_eq!(big.config_len(), 12);
        let mut config = [0u8; 12];
        big.read_config(0, &mut config);
        assert_eq!(&config[0..6], &[0x52, 0x54, 0x00, 0xaa, 0xbb, 0xcc]);
        assert_eq!(u16::from_le_bytes(config[10..12].try_into().unwrap()), 9000);
        drop(big);

        // The default MTU keeps the field out of the config space.
        let normal = Net::new(NetConfig {
            id: "mtu".into(),
            mac: [0x52, 0x54, 0x00, 0xaa, 0xbb, 0xcc],
            mtu: 1500,
            switch,
        })
        .unwrap();
        assert_eq!(normal.features() & F_MTU, 0);
        let mut config = [0u8; 12];
        normal.read_config(0, &mut config);
        assert_eq!(&config[10..12], &[0, 0]);
    }

    #[test]
    fn partial_transmit_writes_and_snapshot_retain_exactly_the_unsent_suffix() {
        let expected: Vec<_> = (0..MAX_TX_STAGE).map(|index| (index % 251) as u8).collect();
        let directory = TempDir::new();
        let mut device = Net::new(NetConfig {
            id: "partial-transmit".into(),
            mac: [0x52, 0x54, 0, 1, 2, 3],
            mtu: DEFAULT_MTU,
            switch: directory.0.join("absent.sock"),
        })
        .unwrap();
        device.quiesce();
        let shared = device.shared.clone();
        lock(&shared).tx_out = expected.clone();
        let (mut producer, mut receiver) = UnixStream::pair().unwrap();
        producer.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let size: libc::c_int = 4096;
        assert_eq!(
            // SAFETY: the socket is live and size is a valid integer option value.
            unsafe {
                libc::setsockopt(
                    producer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    std::ptr::from_ref(&size).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        flush_tx(&shared, &mut producer).unwrap();
        assert!(!lock(&shared).tx_out.is_empty());
        let snapshot = device.save_state().unwrap();
        lock(&shared).tx_out.clear();
        device.restore_state(&snapshot).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut received = Vec::new();
        loop {
            let mut bytes = [0; 4096];
            loop {
                match receiver.read(&mut bytes) {
                    Ok(0) => panic!("transmit socket closed"),
                    Ok(count) => received.extend_from_slice(&bytes[..count]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => panic!("read transmitted bytes: {error}"),
                }
            }
            assert_eq!(lock(&shared).tx_out.as_slice(), &expected[received.len()..]);
            if received.len() == expected.len() {
                break;
            }
            assert!(Instant::now() < deadline, "transmit made no progress");
            flush_tx(&shared, &mut producer).unwrap();
        }
        assert_eq!(received, expected);
    }

    #[test]
    fn transmit_staging_is_bounded_when_switch_stops_reading() {
        let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 2 << 20)]).unwrap();
        let shared = Arc::new(Mutex::new(Shared {
            tap_mode: false,
            tap: None,
            tap_error: None,
            vnet: None,
            snapshot_paused: false,
            config_pending: false,
            mem: Some(mem.clone()),
            interrupt: None,
            queues: vec![None, None],
            link_up: true,
            rx: RxRead::Idle,
            rx_pending: VecDeque::new(),
            tx_out: vec![],
        }));
        let (mut producer, _switch) = UnixStream::pair().unwrap();
        producer.set_nonblocking(true).unwrap();
        let payload = vec![23; 4096];
        while producer.write(&payload).is_ok() {}
        let mut stream = Some(producer);
        mem.write_slice(&payload, GuestAddress(0x100000)).unwrap();
        mem.write_slice(&[0; 12], GuestAddress(0x100000)).unwrap();
        for iteration in 0..1024 {
            let mut ring = TestVq::new(0x1000, 8);
            mem.write_obj(0u16, GuestAddress(ring.avail + 2)).unwrap();
            mem.write_obj(0u16, GuestAddress(ring.used + 2)).unwrap();
            ring.add_chain(&mem, &[(0x100000, payload.len() as u32, false)]);
            lock(&shared).queues[1] = Some(ring.queue());
            process_tx(&shared, &mut stream, 12, false);
            let bytes = lock(&shared).tx_out.len();
            assert!(
                bytes <= 1024 * 1024 + MAX_FRAME,
                "iteration {iteration}: staged {bytes} bytes"
            );
            if bytes >= MAX_TX_STAGE {
                assert_eq!(
                    mem.read_obj::<u16>(GuestAddress(ring.used)).unwrap(),
                    1,
                    "backpressured TX must suppress redundant guest kicks"
                );
            }
        }
    }

    #[test]
    fn transmit_rearms_event_index_after_each_drain() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.dev.quiesce();
        let shared = f.dev.shared.clone();
        {
            let mut state = lock(&shared);
            state.snapshot_paused = false;
            state.queues[usize::from(TXQ)]
                .as_mut()
                .unwrap()
                .set_event_idx(true);
        }
        let event = GuestAddress(f.vqs[1].used + 4 + 8 * u64::from(f.vqs[1].size));
        for index in 1..=4 {
            f.queue_tx(b"next event-index packet");
            process_tx(&shared, &mut None, 12, false);
            assert_eq!(f.vqs[1].used_idx(&f.mem), index);
            assert_eq!(f.mem.read_obj::<u16>(event).unwrap(), index);
        }
    }

    #[test]
    fn wakeup_flush_resumes_a_suppressed_transmit_queue() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.dev.quiesce();
        let pending = b"descriptor waiting behind staged output";
        let staged = b"already staged frame";
        f.queue_tx(pending);
        {
            let mut state = lock(&f.dev.shared);
            state.snapshot_paused = false;
            state.queues[usize::from(TXQ)]
                .as_mut()
                .unwrap()
                .disable_notification(&f.mem)
                .unwrap();
            state
                .tx_out
                .extend_from_slice(&(staged.len() as u32).to_be_bytes());
            state.tx_out.extend_from_slice(staged);
        }
        let (producer, mut receiver) = UnixStream::pair().unwrap();
        producer.set_nonblocking(true).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut stream = Some(producer);
        process_wakeup(&f.dev.shared, &mut stream, 12, false, false, false);
        assert_eq!(
            f.vqs[1].used_idx(&f.mem),
            1,
            "a flush must not leave pending TX without a kick or POLLOUT"
        );
        assert!(lock(&f.dev.shared).tx_out.is_empty());
        for frame in [staged.as_slice(), pending.as_slice()] {
            let mut prefix = [0; 4];
            receiver.read_exact(&mut prefix).unwrap();
            assert_eq!(u32::from_be_bytes(prefix), frame.len() as u32);
            let mut received = vec![0; frame.len()];
            receiver.read_exact(&mut received).unwrap();
            assert_eq!(received, frame);
        }
    }

    #[test]
    fn malformed_transmit_available_index_does_not_spin() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.dev.quiesce();
        lock(&f.dev.shared).snapshot_paused = false;
        f.mem
            .write_obj(f.vqs[1].size + 1, GuestAddress(f.vqs[1].avail + 2))
            .unwrap();
        process_tx(&f.dev.shared, &mut None, 12, false);
        assert_eq!(f.vqs[1].used_idx(&f.mem), 0);
    }

    #[test]
    fn merged_receive_spans_buffers_with_one_header() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.dev.quiesce();
        lock(&f.dev.shared).snapshot_paused = false;
        let frame = (0..80).collect::<Vec<u8>>();
        for (index, len) in [24, 20, 64].into_iter().enumerate() {
            let address = f.data(index as u64);
            if index == 0 {
                f.vqs[0].add_chain(&f.mem, &[(address, 5, true), (address + 5, len - 5, true)]);
            } else {
                f.vqs[0].add_chain(&f.mem, &[(address, len, true)]);
            }
        }
        lock(&f.dev.shared).rx_pending.push_back(frame.clone());
        place_rx_frames(&f.dev.shared, 12, true, false);
        assert_eq!(f.vqs[0].used_idx(&f.mem), 3);
        assert_eq!(
            f.mem.read_obj::<u16>(GuestAddress(f.data(0) + 10)).unwrap(),
            3
        );
        let mut received = Vec::new();
        for (index, expected) in [24, 20, 48].into_iter().enumerate() {
            assert_eq!(f.vqs[0].used_entry(&f.mem, index as u16).1, expected);
            let skip = if index == 0 { 12 } else { 0 };
            let mut bytes = vec![0; (expected - skip) as usize];
            f.mem
                .read_slice(
                    &mut bytes,
                    GuestAddress(f.data(index as u64) + u64::from(skip)),
                )
                .unwrap();
            received.extend(bytes);
        }
        assert_eq!(received, frame);
    }

    #[test]
    fn merged_receive_waits_for_refill_without_publishing_a_partial_packet() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.dev.quiesce();
        {
            let mut state = lock(&f.dev.shared);
            state.snapshot_paused = false;
            state.queues[0].as_mut().unwrap().set_event_idx(true);
        }
        let frame = vec![0x5a; 80];
        let first = f.data(0);
        f.mem.write_slice(&[0x33; 24], GuestAddress(first)).unwrap();
        f.vqs[0].add_chain(&f.mem, &[(first, 24, true)]);
        lock(&f.dev.shared).rx_pending.push_back(frame);
        place_rx_frames(&f.dev.shared, 12, true, false);
        assert_eq!(f.vqs[0].used_idx(&f.mem), 0);
        let mut unchanged = [0; 24];
        f.mem
            .read_slice(&mut unchanged, GuestAddress(first))
            .unwrap();
        assert_eq!(unchanged, [0x33; 24]);
        let event = GuestAddress(f.vqs[0].used + 4 + 8 * u64::from(f.vqs[0].size));
        assert_eq!(f.mem.read_obj::<u16>(event).unwrap(), 1);
        let second = f.data(1);
        f.vqs[0].add_chain(&f.mem, &[(second, 128, true)]);
        place_rx_frames(&f.dev.shared, 12, true, false);
        assert_eq!(f.vqs[0].used_idx(&f.mem), 2);
        assert!(lock(&f.dev.shared).rx_pending.is_empty());
    }

    #[test]
    fn merged_receive_publishes_wrapped_ring_and_checks_the_whole_interrupt_batch() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.dev.quiesce();
        let shared = f.dev.shared.clone();
        {
            let mut state = lock(&shared);
            state.snapshot_paused = false;
            let queue = state.queues[0].as_mut().unwrap();
            queue.set_event_idx(true);
            queue.set_next_used(u16::MAX);
        }
        let event = GuestAddress(f.vqs[0].avail + 4 + 2 * u64::from(f.vqs[0].size));
        f.mem.write_obj(u16::MAX, event).unwrap();
        let first = f.data(0);
        let second = f.data(1);
        f.vqs[0].add_chain(&f.mem, &[(first, 24, true)]);
        f.vqs[0].add_chain(&f.mem, &[(second, 128, true)]);
        lock(&shared).rx_pending.push_back(vec![0x5a; 80]);
        let before = f.irq.queues.lock().len();
        place_rx_frames(&shared, 12, true, false);
        assert_eq!(f.vqs[0].used_idx(&f.mem), 1);
        assert_eq!(f.vqs[0].used_entry(&f.mem, f.vqs[0].size - 1).1, 24);
        assert_eq!(f.vqs[0].used_entry(&f.mem, 0).1, 68);
        assert_eq!(f.irq.queues.lock().len(), before + 1);
    }

    #[test]
    fn merged_receive_rejects_short_and_inaccessible_buffers() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.dev.quiesce();
        lock(&f.dev.shared).snapshot_paused = false;
        for (index, (address, size)) in [(f.data(0), 8), (MEM_SIZE + 1, 32), (f.data(2), 128)]
            .into_iter()
            .enumerate()
        {
            f.vqs[0].add_chain(&f.mem, &[(address, size, true)]);
            if index == 0 {
                lock(&f.dev.shared).rx_pending.push_back(vec![0x5a; 80]);
            }
        }
        place_rx_frames(&f.dev.shared, 12, true, false);
        assert_eq!(f.vqs[0].used_idx(&f.mem), 3);
        assert_eq!(f.vqs[0].used_entry(&f.mem, 0).1, 0);
        assert_eq!(f.vqs[0].used_entry(&f.mem, 1).1, 0);
        assert_eq!(f.vqs[0].used_entry(&f.mem, 2).1, 92);
        assert!(lock(&f.dev.shared).rx_pending.is_empty());
    }

    #[test]
    fn network_offers_event_index_notifications() {
        let f = Fixture::new();
        assert_ne!(f.dev.features() & (1 << 29), 0);
    }

    #[test]
    fn receive_rearms_event_index_when_buffers_run_out() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.dev.quiesce();
        let shared = f.dev.shared.clone();
        {
            let mut state = lock(&shared);
            state.snapshot_paused = false;
            state.queues[usize::from(RXQ)]
                .as_mut()
                .unwrap()
                .set_event_idx(true);
        }
        let frame = b"receive with event-index refill";
        for index in 1..=4 {
            let buffer = f.data(index as u64);
            f.vqs[0].add_chain(&f.mem, &[(buffer, 128, true)]);
            lock(&shared).rx_pending.push_back(frame.to_vec());
            place_rx_frames(&shared, 12, false, false);
            assert_eq!(f.vqs[0].used_idx(&f.mem), index);
            let mut received = vec![0; frame.len()];
            f.mem
                .read_slice(&mut received, GuestAddress(buffer + 12))
                .unwrap();
            assert_eq!(received, frame);
            lock(&shared).rx_pending.push_back(frame.to_vec());
            place_rx_frames(&shared, 12, false, false);
            let event = GuestAddress(f.vqs[0].used + 4 + 8 * u64::from(f.vqs[0].size));
            assert_eq!(f.mem.read_obj::<u16>(event).unwrap(), index);
            lock(&shared).rx_pending.clear();
        }
    }

    #[test]
    fn receive_skips_a_short_buffer_without_waiting_for_another_kick() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.dev.quiesce();
        lock(&f.dev.shared).snapshot_paused = false;
        let frame = b"retry the pending receive frame";
        let short = f.data(1);
        let full = f.data(2);
        f.vqs[0].add_chain(&f.mem, &[(short, 8, true)]);
        f.vqs[0].add_chain(&f.mem, &[(full, 128, true)]);
        lock(&f.dev.shared).rx_pending.push_back(frame.to_vec());
        place_rx_frames(&f.dev.shared, 12, false, false);
        assert_eq!(f.vqs[0].used_idx(&f.mem), 2);
        assert_eq!(f.vqs[0].used_entry(&f.mem, 0).1, 0);
        assert_eq!(f.vqs[0].used_entry(&f.mem, 1).1, 12 + frame.len() as u32);
        assert!(lock(&f.dev.shared).rx_pending.is_empty());
    }

    #[test]
    fn malformed_receive_available_index_does_not_spin() {
        let mut f = Fixture::new();
        let _switch = f.take_switch_side();
        f.dev.quiesce();
        {
            let mut state = lock(&f.dev.shared);
            state.snapshot_paused = false;
            state.rx_pending.push_back(b"pending receive".to_vec());
        }
        f.mem
            .write_obj(f.vqs[0].size + 1, GuestAddress(f.vqs[0].avail + 2))
            .unwrap();
        place_rx_frames(&f.dev.shared, 12, false, false);
        assert_eq!(f.vqs[0].used_idx(&f.mem), 0);
        assert_eq!(lock(&f.dev.shared).rx_pending.len(), 1);
    }

    #[test]
    fn event_index_queue_state_survives_restore_before_activation() {
        let mut f = Fixture::new();
        let mut switch = f.take_switch_side();
        switch
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        f.dev.quiesce();
        {
            let mut state = lock(&f.dev.shared);
            state.snapshot_paused = false;
            for queue in state.queues.iter_mut().flatten() {
                queue.set_event_idx(true);
            }
        }
        f.queue_tx(b"before restore");
        process_tx(&f.dev.shared, &mut None, 12, false);
        f.dev.quiesce();
        let state = f.dev.save_state().unwrap();
        f.dev.reset();
        assert_eq!(f.dev.negotiated.load(Ordering::Acquire), 0);
        f.dev.restore_state(&state).unwrap();
        f.dev.ack_features(f.dev.features());
        f.activate();
        {
            let state = lock(&f.dev.shared);
            assert!(state.queues.iter().flatten().all(QueueT::event_idx_enabled));
            assert_eq!(state.queues[1].as_ref().unwrap().next_avail(), 1);
        }
        let frame = b"after event-index restore";
        f.queue_tx(frame);
        f.dev.resume_snapshot();
        let mut prefix = [0; 4];
        switch.read_exact(&mut prefix).unwrap();
        assert_eq!(u32::from_be_bytes(prefix), frame.len() as u32);
        let mut received = vec![0; frame.len()];
        switch.read_exact(&mut received).unwrap();
        assert_eq!(received, frame);
        wait_for("restored event-index completion", || {
            f.vqs[1].used_idx(&f.mem) == 2
        });
    }

    #[test]
    fn frame_decoder_handles_fragmentation_jumbo_and_empty_frames() {
        let shared = Arc::new(Mutex::new(Shared {
            tap_mode: false,
            tap: None,
            tap_error: None,
            vnet: None,
            snapshot_paused: false,
            config_pending: false,
            mem: None,
            interrupt: None,
            queues: vec![None, None],
            link_up: true,
            rx: RxRead::Idle,
            rx_pending: VecDeque::new(),
            tx_out: vec![],
        }));
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        let jumbo: Vec<u8> = (0..MAX_FRAME).map(|i| (i % 251) as u8).collect();
        let frames = [Vec::new(), jumbo, b"following frame".to_vec()];
        let mut bytes = Vec::new();
        for frame in &frames {
            bytes.extend_from_slice(&(frame.len() as u32).to_be_bytes());
            bytes.extend_from_slice(frame);
        }
        for chunk in bytes.chunks(1023) {
            sender.write_all(chunk).unwrap();
            read_frames(&shared, &mut receiver).unwrap();
        }
        let state = lock(&shared);
        assert!(matches!(state.rx, RxRead::Idle));
        assert_eq!(
            state.rx_pending.iter().collect::<Vec<_>>(),
            frames.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn guest_tx_reaches_the_switch_with_length_framing() {
        let mut f = Fixture::new();
        let mut switch = f.take_switch_side();
        switch
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let frame = b"an ethernet frame";
        f.queue_tx(frame);
        f.dev.queue_notify(1);

        let mut prefix = [0u8; 4];
        switch.read_exact(&mut prefix).unwrap();
        assert_eq!(u32::from_be_bytes(prefix), frame.len() as u32);
        let mut got = vec![0u8; frame.len()];
        switch.read_exact(&mut got).unwrap();
        assert_eq!(&got, &frame[..]);

        wait_for("tx buffer returned", || f.vqs[1].used_idx(&f.mem) == 1);
        let (_, len) = f.vqs[1].used_entry(&f.mem, 0);
        assert_eq!(len as usize, 12 + frame.len());
    }

    #[test]
    fn indirect_transmit_chain_reaches_the_switch() {
        let mut f = Fixture::new();
        let mut switch = f.take_switch_side();
        switch
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let frame = b"indirect transmit frame";
        let header = f.data(0);
        let payload = f.data(1);
        f.mem.write_slice(frame, GuestAddress(payload)).unwrap();
        let table = 0x8_0000;
        let mut descriptors = [0u8; 32];
        descriptors[..8].copy_from_slice(&header.to_le_bytes());
        descriptors[8..12].copy_from_slice(&12u32.to_le_bytes());
        descriptors[12..14].copy_from_slice(&VRING_DESC_F_NEXT.to_le_bytes());
        descriptors[14..16].copy_from_slice(&1u16.to_le_bytes());
        descriptors[16..24].copy_from_slice(&payload.to_le_bytes());
        descriptors[24..28].copy_from_slice(&(frame.len() as u32).to_le_bytes());
        f.mem
            .write_slice(&descriptors, GuestAddress(table))
            .unwrap();
        let head = f.vqs[1].add_chain(&f.mem, &[(table, 32, false)]);
        f.mem
            .write_obj(
                4u16,
                GuestAddress(f.vqs[1].desc + u64::from(head) * 16 + 12),
            )
            .unwrap();
        f.dev.queue_notify(TXQ);
        let mut prefix = [0u8; 4];
        switch.read_exact(&mut prefix).unwrap();
        assert_eq!(u32::from_be_bytes(prefix), frame.len() as u32);
        let mut received = vec![0; frame.len()];
        switch.read_exact(&mut received).unwrap();
        assert_eq!(received, frame);
        wait_for("indirect transmit completion", || {
            f.vqs[1].used_idx(&f.mem) == 1
        });
        assert_eq!(
            f.vqs[1].used_entry(&f.mem, 0),
            (u32::from(head), 12 + frame.len() as u32)
        );
    }

    #[test]
    fn system_reset_keeps_partial_switch_framing_and_the_same_connection() {
        let mut f = Fixture::new();
        let mut switch = f.take_switch_side();
        let frame = b"frame split across reset";
        let mut bytes = (frame.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(frame);
        switch.write_all(&bytes[..2]).unwrap();
        wait_for("partial frame prefix", || {
            matches!(lock(&f.dev.shared).rx, RxRead::Prefix { got: 2, .. })
        });
        f.dev.reset();
        assert!(matches!(
            lock(&f.dev.shared).rx,
            RxRead::Prefix { got: 2, .. }
        ));
        assert!(lock(&f.dev.shared).mem.is_none());
        switch.write_all(&bytes[2..]).unwrap();
        wait_for("complete frame after reset", || {
            !lock(&f.dev.shared).rx_pending.is_empty()
        });
        assert_eq!(lock(&f.dev.shared).rx_pending.front().unwrap(), frame);
        f.switch.as_ref().unwrap().set_nonblocking(true).unwrap();
        assert!(
            matches!(f.switch.as_ref().unwrap().accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn snapshot_quiesce_defers_rx_dma_until_resume() {
        let mut f = Fixture::new();
        let mut switch = f.take_switch_side();
        let addr = f.data(2);
        f.vqs[0].add_chain(&f.mem, &[(addr, 128, true)]);
        f.dev.quiesce();
        let frame = b"snapshot-rx";
        switch
            .write_all(&(frame.len() as u32).to_be_bytes())
            .unwrap();
        switch.write_all(frame).unwrap();
        wait_for("staged frame", || {
            !lock(&f.dev.shared).rx_pending.is_empty()
        });
        assert_eq!(f.vqs[0].used_idx(&f.mem), 0);
        assert_eq!(f.mem.read_obj::<u8>(GuestAddress(addr)).unwrap(), 0);
        f.dev.resume_snapshot();
        wait_for("resumed RX DMA", || f.vqs[0].used_idx(&f.mem) == 1);
        let mut received = vec![0; frame.len()];
        f.mem
            .read_slice(&mut received, GuestAddress(addr + 12))
            .unwrap();
        assert_eq!(received, frame);
    }

    #[test]
    fn switch_rx_reaches_the_guest_behind_a_zeroed_header() {
        let mut f = Fixture::new();
        let mut switch = f.take_switch_side();
        switch
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let frame = b"frame from the switch";
        let addr = f.data(2);
        f.vqs[0].add_chain(&f.mem, &[(addr, (12 + frame.len()) as u32, true)]);
        // The driver kicks after adding receive buffers, as the spec
        // requires; the device relies on it to refresh its poll interest.
        f.dev.queue_notify(0);

        let mut wire = Vec::new();
        wire.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        wire.extend_from_slice(frame);
        switch.write_all(&wire).unwrap();

        wait_for("rx buffer used", || f.vqs[0].used_idx(&f.mem) == 1);
        let (_, len) = f.vqs[0].used_entry(&f.mem, 0);
        assert_eq!(len as usize, 12 + frame.len());
        let mut got = vec![0u8; 12 + frame.len()];
        f.mem.read_slice(&mut got, GuestAddress(addr)).unwrap();
        assert_eq!(&got[0..10], &[0u8; 10]); // no offloads, no hash
        assert_eq!(&got[10..12], &[1, 0]); // num_buffers = 1, little-endian
        assert_eq!(&got[12..], &frame[..]);
    }

    #[test]
    fn link_drops_and_reconnects_with_frames_discarded_while_down() {
        let mut f = Fixture::new();
        let switch = f.take_switch_side();
        drop(switch);
        wait_for("link up", || f.status() & S_LINK_UP != 0);

        // The switch process dies: the connection drops and nothing accepts
        // a new one until it comes back.
        let configs_before = f.irq.config_count();
        f.switch_down();
        wait_for("link down", || f.status() & S_LINK_UP == 0);
        wait_for("config-change interrupt", || {
            f.irq.config_count() > configs_before
        });

        // Transmit while down: the buffer returns, the frame is dropped.
        f.queue_tx(b"into the void");
        f.dev.queue_notify(1);
        wait_for("tx buffer returned", || f.vqs[1].used_idx(&f.mem) == 1);

        // The switch comes back and the device reconnects on its own.
        f.switch_up();
        let mut reconnected = f.take_switch_side();
        wait_for("link up again", || f.status() & S_LINK_UP != 0);

        let frame = b"after reconnect";
        f.queue_tx(frame);
        f.dev.queue_notify(1);
        let mut prefix = [0u8; 4];
        reconnected.read_exact(&mut prefix).unwrap();
        assert_eq!(u32::from_be_bytes(prefix), frame.len() as u32);
        let mut got = vec![0u8; frame.len()];
        reconnected.read_exact(&mut got).unwrap();
        assert_eq!(&got, &frame[..]);
    }

    #[test]
    fn snapshot_round_trip_preserves_queues_and_pending_frames() {
        let mut f = Fixture::new();
        let mut switch = f.take_switch_side();
        switch
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        // One delivered frame, then one frame left pending: no receiveq
        // buffer is posted for it, so it waits inside the device.
        let frame = b"delivered";
        let addr = f.data(2);
        f.vqs[0].add_chain(&f.mem, &[(addr, (12 + frame.len()) as u32, true)]);
        f.dev.queue_notify(0);
        let mut wire = Vec::new();
        wire.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        wire.extend_from_slice(frame);
        wire.extend_from_slice(&(7u32).to_be_bytes());
        wire.extend_from_slice(b"pending");
        switch.write_all(&wire).unwrap();
        wait_for("first rx frame delivered", || {
            f.vqs[0].used_idx(&f.mem) == 1
        });
        wait_for("second rx frame pending", || {
            lock(&f.dev.shared).rx_pending.len() == 1
        });
        let used_before = f.vqs[0].used_idx(&f.mem);
        let state = f.dev.save_state().unwrap();

        // A fresh device takes the state over and reconnects to the switch.
        let Fixture {
            _dir,
            switch_path,
            mem,
            dev,
            irq,
            vqs,
            switch: listener,
            accepted: _,
        } = f;
        drop(dev);
        drop(switch);
        drop(listener);
        std::fs::remove_file(&switch_path).unwrap();
        let mut dev2 = Net::with_retry_interval(
            NetConfig {
                id: "nic0".into(),
                mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
                mtu: 1500,
                switch: switch_path.clone(),
            },
            RETRY,
        )
        .unwrap();
        dev2.ack_features(dev2.features());
        dev2.restore_state(&state).unwrap();
        let queues: Vec<Queue> = vqs.iter().map(|v| v.queue()).collect();
        dev2.activate(mem.clone(), irq, queues).unwrap();

        // Force the ordering that used to discard the restored frame: the
        // worker connects after restore, before any receive buffer is posted.
        wait_for("restored device observes disconnected switch", || {
            !lock(&dev2.shared).link_up
        });
        let listener = UnixListener::bind(&switch_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut connection = None;
        wait_for("restored device reconnects", || match listener.accept() {
            Ok((stream, _)) => {
                connection = Some(stream);
                true
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => false,
            Err(err) => panic!("accept restored device: {err}"),
        });
        wait_for("restored link up", || lock(&dev2.shared).link_up);

        // The driver reposts a receiveq buffer and the pending frame lands,
        // with the used ring continuing where the snapshot left it.
        let addr = DATA_BASE + 0x1_0000 * 3;
        let mut vqs = vqs;
        vqs[0].add_chain(&mem, &[(addr, 64, true)]);
        dev2.queue_notify(0);
        let want = used_before + 1;
        wait_for("pending frame delivered", || vqs[0].used_idx(&mem) == want);
        let mut got = [0u8; 7];
        mem.read_slice(&mut got, GuestAddress(addr + 12)).unwrap();
        assert_eq!(&got, b"pending");
        let (_, len) = vqs[0].used_entry(&mem, used_before);
        assert_eq!(len, 12 + 7);
    }
}
