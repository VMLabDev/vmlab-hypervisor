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

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use parking_lot::{Mutex, MutexGuard};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use virtio_queue::{Queue, QueueState, QueueT};

use crate::{DeviceError, GuestMemory, Snapshot, VirtioDevice, VirtioInterrupt};

/// Virtio device id 1, the network device.
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
        let worker = thread::Builder::new()
            .name("virtio-net".into())
            .spawn({
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
            if let Some(s) = stream.as_mut() {
                if flush_tx(&shared, s).is_err() {
                    drop_link(&mut stream, &shared);
                }
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
            if revents & libc::POLLOUT != 0 {
                if let Some(s) = stream.as_mut() {
                    if flush_tx(&shared, s).is_err() {
                        drop_link(&mut stream, &shared);
                        continue;
                    }
                }
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
                    // Volatile read state belongs to the old connection.
                    {
                        let mut sh = lock(&shared);
                        sh.rx = RxRead::Idle;
                        sh.rx_pending.clear();
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
            if let Ok(mut writer) = chain.writer(mem) {
                if writer.available_bytes() >= need
                    && writer.write_all(&hdr).is_ok()
                    && writer.write_all(&frame).is_ok()
                {
                    written = need as u32;
                }
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
    if let Some(stream_ref) = stream.as_mut() {
        if flush_tx(shared, stream_ref).is_err() {
            drop_link(stream, shared);
        }
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
        let mut config = [0u8; 10];
        config[0..6].copy_from_slice(&self.config.mac);
        let status = if link_up { S_LINK_UP } else { 0 };
        config[6..8].copy_from_slice(&status.to_le_bytes());
        if self.config.mtu != DEFAULT_MTU {
            config[8..10].copy_from_slice(&self.config.mtu.to_le_bytes());
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
        self.pending_queues = Some(queues);
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
        self.take(n).map(|s| s)
    }
    fn bytes(&mut self) -> Result<Vec<u8>, DeviceError> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
}
