//! virtio-net (VIRTIO 1.2 §5.1) attached to vmlab's network switch.
//!
//! The device is a client of a unix stream socket (the switch's listener) and
//! exchanges whole Ethernet frames in vmlab's stream framing: a 4-byte
//! big-endian length prefix followed by one frame, capped at 65536 + 14 bytes
//! (`src/net/framing.rs` in vmlab defines the format; the switch is the other
//! end of the socket).
//!
//! One receiveq and one transmitq, no control queue: the offered features are
//! `VIRTIO_NET_F_MAC`, `VIRTIO_NET_F_STATUS` and `VIRTIO_NET_F_MTU` (only when
//! an MTU other than 1500 is configured, matching what vmlab passes to QEMU).
//! The `virtio_net_hdr` (12 bytes for a modern device) is stripped on transmit
//! and prepended with `num_buffers` = 1 on receive.
//!
//! If the switch socket closes, frames are dropped, `VIRTIO_NET_S_LINK_UP` is
//! cleared in the config space with a config-change interrupt, and the device
//! reconnects on its own. All socket I/O runs on one worker thread the device
//! owns; the vCPU path only sends wakeups. Receive is under backpressure: the
//! worker stops reading the socket while frames are waiting for receiveq
//! buffers.

use parking_lot::{Mutex, MutexGuard};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use virtio_queue::{Queue, QueueState, QueueT};

use crate::{DeviceError, GuestMemory, Snapshot, VirtioDevice, VirtioInterrupt};

/// Virtio device id 1, the network device.
/// Size of `struct virtio_net_config` up to and including `mtu`.
const CONFIG_LEN: usize = 12;
const DEVICE_ID: u32 = 1;
/// `VIRTIO_NET_F_MTU` (feature bit 3).
const F_MTU: u64 = 1 << 3;
/// `VIRTIO_NET_F_MAC` (feature bit 5).
const F_MAC: u64 = 1 << 5;
/// `VIRTIO_NET_F_MRG_RXBUF` (feature bit 15) — never offered, but it changes
/// the header length if a driver negotiated it against a wider feature set.
const F_MRG_RXBUF: u64 = 1 << 15;
/// `VIRTIO_NET_F_STATUS` (feature bit 16).
const F_STATUS: u64 = 1 << 16;
/// `VIRTIO_F_VERSION_1` (feature bit 32): this is a modern virtio device.
const F_VERSION_1: u64 = 1 << 32;
/// `VIRTIO_NET_S_LINK_UP` (status bit 0).
const S_LINK_UP: u16 = 1;
/// Queue size offered for both virtqueues (QEMU's virtio-net default).
const QUEUE_SIZE: u16 = 256;
/// The receiveq index.
const RXQ: u16 = 0;
/// The transmitq index.
const TXQ: u16 = 1;
/// Largest Ethernet frame the wire format accepts (64 KiB payload + header).
const MAX_FRAME: usize = 65536 + 14;
/// Largest read the worker takes from the switch socket in one go.
const READ_CHUNK: usize = 128 * 1024;
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

/// Commands from the device (vCPU path) to the worker thread.
enum Cmd {
    /// The driver kicked the queue with this index.
    Kick(u16),
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
    config: NetConfig,
    negotiated: Arc<AtomicU64>,
    shared: Arc<Mutex<Shared>>,
    kicks: Sender<Cmd>,
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
    /// Queue states restored before `activate`, applied when it arrives.
    pending_queues: Option<Vec<Option<QueueState>>>,
}

impl Net {
    /// Create the device; the worker starts reaching for the switch at once.
    pub fn new(config: NetConfig) -> io::Result<Self> {
        Net::with_retry_interval(config, RETRY)
    }

    /// Create the device with a non-default switch reconnect interval.
    fn with_retry_interval(config: NetConfig, retry: Duration) -> io::Result<Self> {
        let (wake_dev, wake_worker) = UnixStream::pair()?;
        wake_dev.set_nonblocking(true)?;
        wake_worker.set_nonblocking(true)?;
        let (kicks, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared {
            mem: None,
            interrupt: None,
            queues: (0..2).map(|_| None).collect(),
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
            move || run(switch, retry, shared, negotiated, rx, wake_worker)
        })?;
        Ok(Net {
            config,
            negotiated,
            shared,
            kicks,
            wake: wake_dev,
            worker: Some(worker),
            pending_queues: None,
        })
    }

    fn wake_worker(&self) {
        let _ = (&self.wake).write_all(b"k");
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        let _ = self.kicks.send(Cmd::Shutdown);
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
) {
    let mut stream: Option<UnixStream> = None;
    let mut next_try = Instant::now();
    let mut shutdown = false;
    while !shutdown {
        // The live connection is the truth about the link.
        if set_link(&shared, stream.is_some()) {
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
        let timeout: i32 = if stream.is_none() {
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
            let hdr = hdr_len(negotiated.load(Ordering::Acquire));
            while let Ok(cmd) = cmds.try_recv() {
                match cmd {
                    Cmd::Kick(RXQ) => place_rx_frames(&shared, hdr),
                    Cmd::Kick(TXQ) => process_tx(&shared, &mut stream, hdr),
                    Cmd::Kick(_) => {}
                    Cmd::Shutdown => shutdown = true,
                }
            }
            // A wakeup is also the device saying shared state changed
            // (activation, restore): give pending work another chance.
            place_rx_frames(&shared, hdr);
            if let Some(s) = stream.as_mut()
                && flush_tx(&shared, s).is_err()
            {
                drop_link(&mut stream, &shared);
            }
        }

        if stream.is_some() {
            let revents = polls[1].revents;
            let dead = revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
            if revents & libc::POLLIN != 0 {
                match read_frames(&shared, stream.as_mut().unwrap()) {
                    Ok(()) => place_rx_frames(&shared, hdr_len(negotiated.load(Ordering::Acquire))),
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
            if dead {
                drop_link(&mut stream, &shared);
            }
        }

        if stream.is_none() && Instant::now() >= next_try {
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
    let irq = lock(shared).interrupt.clone();
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
    lock(shared).rx_pending.is_empty()
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
    for &byte in &chunk[..n] {
        match &mut sh.rx {
            RxRead::Idle => {
                sh.rx = RxRead::Prefix {
                    buf: [byte, 0, 0, 0],
                    got: 1,
                };
            }
            RxRead::Prefix { buf, got } => {
                buf[*got] = byte;
                *got += 1;
                if *got == 4 {
                    let len = u32::from_be_bytes(*buf) as usize;
                    if len > MAX_FRAME {
                        // A length past the cap is a corrupt stream, exactly
                        // as vmlab's framing codec treats it.
                        sh.rx = RxRead::Idle;
                        sh.rx_pending.clear();
                        return Err(());
                    }
                    sh.rx = RxRead::Body {
                        buf: vec![0u8; len],
                        got: 0,
                    };
                }
            }
            RxRead::Body { buf, got } => {
                buf[*got] = byte;
                *got += 1;
                if *got == buf.len() {
                    let frame = std::mem::take(buf);
                    complete.push(frame);
                    sh.rx = RxRead::Idle;
                }
            }
        }
    }
    sh.rx_pending.extend(complete);
    Ok(())
}

/// Move pending frames into free receiveq buffers, header first.
fn place_rx_frames(shared: &Arc<Mutex<Shared>>, hdr_len: usize) {
    let mut irq: Option<(Arc<dyn VirtioInterrupt>, u16)> = None;
    {
        let mut guard = lock(shared);
        // A plain reborrow lets the field borrows below stay disjoint.
        let sh = &mut *guard;
        if sh.rx_pending.is_empty() {
            return;
        }
        let Some(mem) = sh.mem.as_ref() else {
            return;
        };
        let Some(q) = sh.queues.get_mut(usize::from(RXQ)).and_then(|q| q.as_mut()) else {
            return;
        };
        // A zeroed header with num_buffers = 1: no offloads are offered, and
        // exactly one descriptor holds the packet (VIRTIO_NET_F_MRG_RXBUF is
        // not offered).
        let mut hdr = vec![0u8; hdr_len];
        let num_buffers_at = hdr.len().saturating_sub(2);
        hdr[num_buffers_at..].copy_from_slice(&1u16.to_le_bytes());
        while let Some(frame) = sh.rx_pending.front().cloned() {
            let Some(chain) = q.pop_descriptor_chain(mem) else {
                break;
            };
            let head = chain.head_index();
            let need = hdr.len() + frame.len();
            let mut written = 0;
            if let Ok(mut writer) = chain.writer(mem)
                && writer.available_bytes() >= need
                && writer.write_all(&hdr).is_ok()
                && writer.write_all(&frame).is_ok()
            {
                written = need as u32;
            }
            if q.add_used(mem, head, written).is_err() {
                break;
            }
            if written == 0 {
                // Buffer too small for the frame: the frame is dropped and
                // the buffer returns empty for the driver to reuse.
                break;
            }
            sh.rx_pending.pop_front();
        }
        if q.needs_notification(mem).unwrap_or(false) {
            irq = sh.interrupt.clone().map(|irq| (irq, RXQ));
        }
    }
    if let Some((irq, queue)) = irq {
        irq.notify_queue(queue);
    }
}

/// Consume transmitq buffers: strip the header, frame the packet, and send it
/// to the switch — or drop it while the link is down.
fn process_tx(shared: &Arc<Mutex<Shared>>, stream: &mut Option<UnixStream>, hdr_len: usize) {
    let mut out = Vec::new();
    let mut irq: Option<(Arc<dyn VirtioInterrupt>, u16)> = None;
    {
        let mut guard = lock(shared);
        // A plain reborrow lets the field borrows below stay disjoint.
        let sh = &mut *guard;
        let Some(mem) = sh.mem.as_ref() else {
            return;
        };
        let Some(q) = sh.queues.get_mut(usize::from(TXQ)).and_then(|q| q.as_mut()) else {
            return;
        };
        while let Some(chain) = q.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let mut consumed = 0;
            if let Ok(mut reader) = chain.reader(mem) {
                let mut hdr = vec![0u8; hdr_len.min(reader.available_bytes())];
                if reader.read_exact(&mut hdr).is_ok() {
                    let mut frame = vec![0u8; reader.available_bytes()];
                    if reader.read_exact(&mut frame).is_ok() {
                        consumed = (hdr.len() + frame.len()) as u32;
                        if frame.len() <= MAX_FRAME {
                            out.extend_from_slice(&(frame.len() as u32).to_be_bytes());
                            out.extend_from_slice(&frame);
                        }
                    }
                }
            }
            if q.add_used(mem, head, consumed).is_err() {
                break;
            }
        }
        if q.needs_notification(mem).unwrap_or(false) {
            irq = sh.interrupt.clone().map(|irq| (irq, TXQ));
        }
        if stream.is_some() {
            sh.tx_out.extend_from_slice(&out);
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
}

/// Push framed bytes into the switch socket; keeps the remainder when the
/// socket would block. `Err(())` means the connection is gone.
fn flush_tx(shared: &Arc<Mutex<Shared>>, stream: &mut UnixStream) -> Result<(), ()> {
    loop {
        let chunk = {
            let sh = lock(shared);
            if sh.tx_out.is_empty() {
                return Ok(());
            }
            let take = sh.tx_out.len().min(READ_CHUNK);
            sh.tx_out[..take].to_vec()
        };
        match stream.write(&chunk) {
            Ok(n) => {
                let mut sh = lock(shared);
                sh.tx_out.drain(..n);
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(_) => return Err(()),
        }
    }
}

// ---- VirtioDevice -----------------------------------------------------------

impl VirtioDevice for Net {
    fn device_type(&self) -> u32 {
        DEVICE_ID
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE, QUEUE_SIZE]
    }

    fn config_len(&self) -> u64 {
        CONFIG_LEN as u64
    }

    fn features(&self) -> u64 {
        let mut features = F_MAC | F_STATUS | F_VERSION_1;
        if self.config.mtu != DEFAULT_MTU {
            features |= F_MTU;
        }
        features
    }

    fn ack_features(&mut self, features: u64) {
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
        config[8..10].copy_from_slice(&1u16.to_le_bytes());
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
        if queues.len() != 2 {
            return Err(DeviceError::Other(format!(
                "virtio-net needs 2 queues, got {}",
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
        drop(sh);
        self.wake_worker();
        Ok(())
    }

    fn queue_notify(&mut self, index: u16) {
        if self.kicks.send(Cmd::Kick(index)).is_ok() {
            self.wake_worker();
        }
    }

    fn reset(&mut self) {
        let mut sh = lock(&self.shared);
        sh.queues = (0..2).map(|_| None).collect();
        sh.mem = None;
        sh.interrupt = None;
        sh.rx = RxRead::Idle;
        sh.rx_pending.clear();
        sh.tx_out.clear();
    }
}

// ---- Snapshot ----------------------------------------------------------------

/// Snapshot format magic and version.
const SNAPSHOT_MAGIC: &[u8; 5] = b"VLNET";
const SNAPSHOT_VERSION: u16 = 1;

impl Snapshot for Net {
    fn state_id(&self) -> String {
        self.config.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let sh = lock(&self.shared);
        let mut enc = Enc::new();
        enc.raw(SNAPSHOT_MAGIC);
        enc.u16(SNAPSHOT_VERSION);
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
        if dec.u16()? != SNAPSHOT_VERSION {
            return Err(DeviceError::State(
                "unsupported virtio-net snapshot version".into(),
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
        if nqueues != 2 {
            return Err(DeviceError::State(format!(
                "snapshot has {nqueues} queues, device needs 2"
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
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;
    use vm_memory::{Bytes, GuestAddress};

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
            let dir = TempDir::new();
            let switch_path = dir.0.join("switch.sock");
            let switch = UnixListener::bind(&switch_path).unwrap();
            switch.set_nonblocking(true).unwrap();
            let dev = Net::with_retry_interval(
                NetConfig {
                    id: "nic0".into(),
                    mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
                    mtu: 1500,
                    switch: switch_path.clone(),
                },
                RETRY,
            )
            .unwrap();
            let mem = GuestMemory::from_ranges(&[(GuestAddress(0), MEM_SIZE as usize)]).unwrap();
            let irq = Arc::new(TestInterrupt::default());
            let vqs = vec![TestVq::new(0x1000, 256), TestVq::new(0x2_0000, 256)];
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
            // What a modern transport negotiates: the whole feature set.
            f.dev.ack_features(f.dev.features());
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
