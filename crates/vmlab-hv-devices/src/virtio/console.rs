//! virtio-console (VIRTIO 1.2 §5.3) with `VIRTIO_CONSOLE_F_MULTIPORT`.
//!
//! The device reserves console port id 0 and carries one
//! named port per [`PortConfig`]. Every named port is a unix socket the device
//! listens on from construction, matching QEMU's `server=on,wait=off` chardev
//! as vmlab relies on it: the listener exists before the machine is ready, at
//! most one host client at a time, the guest sees the port's host side open
//! (`VIRTIO_CONSOLE_PORT_OPEN`) only while a client is connected, a client may
//! disconnect and a new one connect, and guest output with no client connected
//! is discarded.
//!
//! Queue layout (guest-visible): 0/1 = port 0 receive/transmit, 2/3 = control
//! receive/transmit, `2i+2`/`2i+3` = port `i` receive/transmit for `i >= 1`.
//! `max_nr_ports` counts port 0, and the queue count is `(max_nr_ports + 1) *
//! 2` — the shape the Linux driver derives from the config space.
//!
//! All socket I/O runs on one worker thread the device owns; the vCPU path
//! only sends wakeups over a channel. Host→guest data is under backpressure:
//! the worker stops reading a client socket while the port's receiveq has no
//! free descriptor chain, and undelivered bytes wait in a staging buffer
//! instead of being dropped.

use parking_lot::{Mutex, MutexGuard};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use virtio_queue::{Queue, QueueOwnedT, QueueState, QueueT};

use crate::{DeviceError, GuestMemory, Snapshot, VirtioDevice, VirtioInterrupt};

/// Virtio device id 3, the console device.
const DEVICE_ID: u32 = 3;
/// `VIRTIO_CONSOLE_F_MULTIPORT` (feature bit 1).
const F_MULTIPORT: u64 = 1 << 1;
/// `VIRTIO_F_VERSION_1` (feature bit 32): this is a modern virtio device.
const F_VERSION_1: u64 = 1 << 32;
/// Queue size offered for data virtqueues.
const QUEUE_SIZE: u16 = 128;
/// Config space size: `cols`, `rows`, `max_nr_ports`, `emerg_wr`.
const CONFIG_LEN: usize = 12;
/// `struct virtio_console_control`: `le32 id`, `le16 event`, `le16 value`.
const CTRL_LEN: usize = 8;
/// `VIRTIO_CONSOLE_DEVICE_READY`.
const EV_DEVICE_READY: u16 = 0;
/// `VIRTIO_CONSOLE_PORT_ADD`.
const EV_PORT_ADD: u16 = 1;
/// `VIRTIO_CONSOLE_PORT_READY`.
const EV_PORT_READY: u16 = 3;
/// `VIRTIO_CONSOLE_PORT_OPEN`.
const EV_PORT_OPEN: u16 = 6;
/// `VIRTIO_CONSOLE_PORT_NAME`.
const EV_PORT_NAME: u16 = 7;
/// Largest read the worker takes from a client socket in one go.
const READ_CHUNK: usize = 64 * 1024;
/// Apply backpressure after this much staged output, plus at most one chain.
const MAX_TX_STAGE: usize = 1024 * 1024;

/// One named virtio-serial port served on a unix socket.
#[derive(Debug, Clone)]
pub struct PortConfig {
    /// Guest-visible port name announced with `VIRTIO_CONSOLE_PORT_NAME`
    /// (vmlab's are `vmlab.ctl.0` and `vmlab.agent.0`).
    pub name: String,
    /// Host socket path the device listens on; the host side connects here.
    pub path: PathBuf,
}

/// A control message waiting for a free control-receiveq buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CtrlMsg {
    id: u32,
    event: u16,
    value: u16,
    /// Name bytes sent after the header (`VIRTIO_CONSOLE_PORT_NAME` only,
    /// UTF-8 without a NUL terminator, as the spec requires).
    name: Option<String>,
}

/// Per-port state shared between the device and the worker thread.
struct PortState {
    /// A host client is connected (maintained by the worker).
    host_open: bool,
    /// The guest driver opened the port (from `VIRTIO_CONSOLE_PORT_OPEN`).
    guest_open: bool,
    /// Host→guest bytes read from the client but not yet placed in a buffer.
    rx_stage: Vec<u8>,
    /// Guest→host bytes not yet accepted by the client socket.
    tx_stage: Vec<u8>,
}

/// Everything the device and its worker thread share.
struct Shared {
    mem: Option<GuestMemory>,
    interrupt: Option<Arc<dyn VirtioInterrupt>>,
    queues: Vec<Option<Queue>>,
    /// Guest-visible port names, index `i` = guest port id `i + 1`.
    names: Vec<String>,
    /// The driver sent `VIRTIO_CONSOLE_DEVICE_READY` with value 1.
    device_ready: bool,
    ports: Vec<PortState>,
    pending: VecDeque<CtrlMsg>,
}

/// Commands from the device (vCPU path) to the worker thread.
enum Cmd {
    /// The driver kicked the queue with this index.
    Kick(u16),
    Shutdown,
}

/// The virtio-console device.
///
/// The listeners exist from [`Console::new`]; the worker thread runs until the
/// device is dropped. Snapshot restore assumes the in-place flow (same
/// process, same sockets): state is restored faithfully and the worker
/// reconciles the socket-coupled flags (`host_open`) with its live
/// connections, so restoring into a fresh device degrades gracefully.
pub struct Console {
    id: String,
    ports: Vec<PortConfig>,
    queue_sizes: Vec<u16>,
    shared: Arc<Mutex<Shared>>,
    kicks: Sender<Cmd>,
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
    /// Queue states restored before `activate`, applied when it arrives.
    pending_queues: Option<Vec<Option<QueueState>>>,
}

impl Console {
    /// Create the device and bind every port's listener. The first port in
    /// the list gets guest port id 1, the next id 2, and so on — vmlab's
    /// order (ctl, then agent) matches QEMU's device order.
    pub fn new(id: impl Into<String>, ports: Vec<PortConfig>) -> io::Result<Self> {
        let mut listeners = Vec::with_capacity(ports.len());
        for port in &ports {
            listeners.push(bind_listener(&port.path)?);
        }
        let (wake_dev, wake_worker) = UnixStream::pair()?;
        wake_dev.set_nonblocking(true)?;
        wake_worker.set_nonblocking(true)?;
        let (kicks, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared {
            mem: None,
            interrupt: None,
            queues: (0..2 * (ports.len() + 2)).map(|_| None).collect(),
            names: ports.iter().map(|p| p.name.clone()).collect(),
            device_ready: false,
            ports: (0..ports.len())
                .map(|_| PortState {
                    host_open: false,
                    guest_open: false,
                    rx_stage: Vec::new(),
                    tx_stage: Vec::new(),
                })
                .collect(),
            pending: VecDeque::new(),
        }));
        let worker = thread::Builder::new()
            .name("virtio-console".into())
            .spawn({
                let shared = shared.clone();
                move || run(listeners, shared, rx, wake_worker)
            })?;
        let mut queue_sizes = vec![QUEUE_SIZE; 2 * (ports.len() + 2)];
        queue_sizes[2..4].fill(32);
        Ok(Console {
            id: id.into(),
            queue_sizes,
            ports,
            shared,
            kicks,
            wake: wake_dev,
            worker: Some(worker),
            pending_queues: None,
        })
    }

    /// `max_nr_ports` from the config space: port 0 plus every named port.
    fn max_nr_ports(&self) -> u32 {
        (self.ports.len() + 1) as u32
    }

    fn queue_count(&self) -> usize {
        self.queue_sizes.len()
    }

    fn wake_worker(&self) {
        // A failed or full wakeup pipe means the worker already has a pending
        // wakeup (or is gone); either way there is nothing to do.
        let _ = (&self.wake).write_all(b"k");
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.kicks.send(Cmd::Shutdown);
        self.wake_worker();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Bind one port's listener, replacing any socket file a previous machine
/// left behind, exactly as a respawned QEMU rebinds its ports.
fn bind_listener(path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(path);
    UnixListener::bind(path)
}

/// The port a receiveq index (even) moves data for, if it is a port queue.
fn rx_port(queue: u16) -> Option<u16> {
    match queue {
        0 => Some(0),
        2 => None, // control receiveq
        q if q >= 4 && q % 2 == 0 => Some(q / 2 - 1),
        _ => None,
    }
}

/// The port a transmitq index (odd) moves data for, if it is a port queue.
fn tx_port(queue: u16) -> Option<u16> {
    match queue {
        1 => Some(0),
        3 => None, // control transmitq
        q if q >= 5 && q % 2 == 1 => Some((q - 3) / 2),
        _ => None,
    }
}

/// The receiveq index of a port id.
fn rx_queue(port: u16) -> u16 {
    match port {
        0 => 0,
        p => 2 * p + 2,
    }
}

/// The transmitq index of a port id.
fn tx_queue(port: u16) -> u16 {
    match port {
        0 => 1,
        p => 2 * p + 3,
    }
}

/// Lock the shared state.
fn lock(shared: &Arc<Mutex<Shared>>) -> MutexGuard<'_, Shared> {
    shared.lock()
}

// ---- the worker thread ------------------------------------------------------

/// Poll every listener and connected client plus the wakeup pipe, and drive
/// all virtqueue and socket traffic. Nothing here runs on the vCPU path.
fn run(
    listeners: Vec<UnixListener>,
    shared: Arc<Mutex<Shared>>,
    cmds: Receiver<Cmd>,
    wake: UnixStream,
) {
    let mut clients: Vec<Option<UnixStream>> = (0..listeners.len()).map(|_| None).collect();
    let mut shutdown = false;
    while !shutdown {
        // Socket-coupled truth wins over any restored value.
        {
            let mut sh = lock(&shared);
            for (i, state) in sh.ports.iter_mut().enumerate() {
                state.host_open = clients[i].is_some();
            }
        }

        let mut polls: Vec<libc::pollfd> = Vec::with_capacity(2 * clients.len() + 2);
        polls.push(libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for listener in &listeners {
            polls.push(libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        for (i, client) in clients.iter().enumerate() {
            let events = match client {
                Some(_) => {
                    let mut events = 0;
                    if rx_has_capacity(&shared, rx_queue(i as u16 + 1)) {
                        events |= libc::POLLIN;
                    }
                    if !lock(&shared).ports[i].tx_stage.is_empty() {
                        events |= libc::POLLOUT;
                    }
                    events
                }
                None => 0,
            };
            polls.push(libc::pollfd {
                fd: client.as_ref().map_or(-1, |c| c.as_raw_fd()),
                events,
                revents: 0,
            });
        }

        // SAFETY: `poll(2)` on a short-lived array of fds we own for the
        // duration of the call; no signal handlers run on this thread.
        let ready = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, -1) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }

        if polls[0].revents & libc::POLLIN != 0 {
            drain_wakeup(&wake);
            while let Ok(cmd) = cmds.try_recv() {
                match cmd {
                    Cmd::Kick(queue) => handle_kick(&shared, &mut clients, queue),
                    Cmd::Shutdown => shutdown = true,
                }
            }
            // A wakeup is also the device saying shared state changed
            // (activation, restore): give pending work another chance.
            let mut sh = lock(&shared);
            flush_pending(&mut sh);
        }

        let client_base = 1 + listeners.len();
        for (i, listener) in listeners.iter().enumerate() {
            if clients[i].is_none() && polls[1 + i].revents & libc::POLLIN != 0 {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(true);
                        clients[i] = Some(stream);
                        let mut sh = lock(&shared);
                        sh.ports[i].host_open = true;
                        let port = i as u16 + 1;
                        sh.pending.push_back(CtrlMsg {
                            id: u32::from(port),
                            event: EV_PORT_OPEN,
                            value: 1,
                            name: None,
                        });
                        flush_pending(&mut sh);
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => {}
                }
            }
        }

        for i in 0..clients.len() {
            let revents = polls[client_base + i].revents;
            if revents == 0 || clients[i].is_none() {
                continue;
            }
            if revents & libc::POLLIN != 0 {
                let mut buf = [0u8; READ_CHUNK];
                let outcome = clients[i].as_mut().map(|c| c.read(&mut buf));
                match outcome {
                    Some(Ok(0)) => disconnect(&shared, &mut clients, i),
                    Some(Ok(n)) => {
                        let mut sh = lock(&shared);
                        sh.ports[i].rx_stage.extend_from_slice(&buf[..n]);
                        drain_rx_stage(&mut sh, i);
                    }
                    Some(Err(err)) if err.kind() == io::ErrorKind::WouldBlock => {}
                    Some(Err(_)) => disconnect(&shared, &mut clients, i),
                    None => {}
                }
            }
            if clients[i].is_some() && revents & libc::POLLOUT != 0 {
                if let Some(client) = clients[i].as_mut() {
                    flush_tx(&shared, i, client);
                }
                process_tx(&shared, &mut clients, i as u16 + 1);
            }
            if clients[i].is_some() && revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                disconnect(&shared, &mut clients, i);
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

/// The peer went away: drop the client, tell the guest the port closed.
fn disconnect(shared: &Arc<Mutex<Shared>>, clients: &mut [Option<UnixStream>], i: usize) {
    clients[i] = None;
    let mut sh = lock(shared);
    sh.ports[i].host_open = false;
    // Guest bytes destined for the vanished client are dropped, as QEMU's
    // chardev does; host bytes already staged still reach the guest.
    sh.ports[i].tx_stage.clear();
    let port = i as u16 + 1;
    sh.pending.push_back(CtrlMsg {
        id: u32::from(port),
        event: EV_PORT_OPEN,
        value: 0,
        name: None,
    });
    flush_pending(&mut sh);
    drop(sh);
    process_tx(shared, clients, port);
}

/// Route a queue kick to its handler.
fn handle_kick(shared: &Arc<Mutex<Shared>>, clients: &mut [Option<UnixStream>], queue: u16) {
    match queue {
        2 => {
            // The guest added control-receiveq buffers.
            let mut sh = lock(shared);
            flush_pending(&mut sh);
        }
        3 => process_ctrl_tx(shared),
        _ => {
            if let Some(port) = rx_port(queue)
                && port >= 1
            {
                let mut sh = lock(shared);
                drain_rx_stage(&mut sh, port as usize - 1);
            }
            if let Some(port) = tx_port(queue) {
                process_tx(shared, clients, port);
            }
        }
    }
}

/// True while the queue has a descriptor chain the device has not consumed —
/// the worker only reads a client socket under this condition.
fn rx_has_capacity(shared: &Arc<Mutex<Shared>>, queue: u16) -> bool {
    let sh = lock(shared);
    let Some(mem) = sh.mem.as_ref() else {
        return false;
    };
    let Some(q) = sh.queues.get(usize::from(queue)).and_then(|q| q.as_ref()) else {
        return false;
    };
    match q.avail_idx(mem, Ordering::Acquire) {
        Ok(idx) => idx.0 != q.next_avail(),
        Err(_) => false,
    }
}

/// Move staged host→guest bytes into as many receiveq buffers as are free.
fn drain_rx_stage(sh: &mut Shared, i: usize) {
    let queue = rx_queue(i as u16 + 1);
    let mut irq: Option<(Arc<dyn VirtioInterrupt>, u16)> = None;
    {
        let Some(mem) = sh.mem.as_ref() else {
            return;
        };
        let Some(q) = sh
            .queues
            .get_mut(usize::from(queue))
            .and_then(|q| q.as_mut())
        else {
            return;
        };
        while !sh.ports[i].rx_stage.is_empty() {
            let Some(chain) = q.pop_descriptor_chain(mem) else {
                break;
            };
            let head = chain.head_index();
            let mut written = 0;
            if let Ok(mut writer) = chain.writer(mem) {
                let n = writer.available_bytes().min(sh.ports[i].rx_stage.len());
                if writer.write_all(&sh.ports[i].rx_stage[..n]).is_ok() {
                    written = n;
                }
            }
            if q.add_used(mem, head, written as u32).is_err() {
                break;
            }
            sh.ports[i].rx_stage.drain(..written);
        }
        if q.needs_notification(mem).unwrap_or(false) {
            irq = sh.interrupt.clone().map(|irq| (irq, queue));
        }
    }
    if let Some((irq, queue)) = irq {
        irq.notify_queue(queue);
    }
}

/// Consume transmitq buffers of one port: read the guest bytes and hand them
/// to the connected client, or discard them when no client is connected.
fn process_tx(shared: &Arc<Mutex<Shared>>, clients: &mut [Option<UnixStream>], port: u16) {
    let queue = tx_queue(port);
    loop {
        let mut out = Vec::new();
        let connected = port >= 1 && clients[port as usize - 1].is_some();
        let mut blocked = false;
        let mut irq: Option<(Arc<dyn VirtioInterrupt>, u16)> = None;
        {
            let mut sh = lock(shared);
            // A plain reborrow lets the field borrows below stay disjoint.
            let sh = &mut *sh;
            let pending = if connected {
                sh.ports[port as usize - 1].tx_stage.len()
            } else {
                0
            };
            let Some(mem) = sh.mem.as_ref() else {
                return;
            };
            let Some(q) = sh
                .queues
                .get_mut(usize::from(queue))
                .and_then(|q| q.as_mut())
            else {
                return;
            };
            while let Some(chain) = q.pop_descriptor_chain(mem) {
                if connected && pending + out.len() >= MAX_TX_STAGE {
                    q.go_to_previous_position();
                    blocked = true;
                    break;
                }
                let head = chain.head_index();
                let len = match chain.reader(mem) {
                    Ok(mut reader) => {
                        if !connected {
                            reader.available_bytes() as u32
                        } else {
                            let mut buf = vec![0u8; reader.available_bytes()];
                            if reader.read_exact(&mut buf).is_ok() {
                                out.extend_from_slice(&buf);
                                buf.len() as u32
                            } else {
                                0
                            }
                        }
                    }
                    Err(_) => 0,
                };
                if q.add_used(mem, head, len).is_err() {
                    break;
                }
            }
            if q.needs_notification(mem).unwrap_or(false) {
                irq = sh.interrupt.clone().map(|irq| (irq, queue));
            }
        }
        if let Some((irq, queue)) = irq {
            irq.notify_queue(queue);
        }
        // Port 0 has no host side: its output is always discarded.
        if port >= 1 {
            let i = port as usize - 1;
            if clients[i].is_some() {
                {
                    let mut sh = lock(shared);
                    sh.ports[i].tx_stage.extend_from_slice(&out);
                }
                if let Some(client) = clients[i].as_mut() {
                    flush_tx(shared, i, client);
                }
            }
        }
        if !blocked || lock(shared).ports[port as usize - 1].tx_stage.len() >= MAX_TX_STAGE {
            return;
        }
    }
}

/// Push staged guest→host bytes into the client socket; keeps the remainder
/// when the socket would block.
fn flush_tx(shared: &Arc<Mutex<Shared>>, i: usize, client: &mut UnixStream) {
    loop {
        let mut sh = lock(shared);
        if sh.ports[i].tx_stage.is_empty() {
            return;
        }
        let take = sh.ports[i].tx_stage.len().min(READ_CHUNK);
        // The socket is nonblocking. Keep reset from clearing the stage
        // between this write and draining the bytes it accepted.
        match client.write(&sh.ports[i].tx_stage[..take]) {
            Ok(n) => {
                sh.ports[i].tx_stage.drain(..n);
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
            Err(_) => return, // the poll loop's error handling drops the client
        }
    }
}

/// Consume control-transmitq buffers and dispatch the driver's messages.
fn process_ctrl_tx(shared: &Arc<Mutex<Shared>>) {
    let mut irq: Option<(Arc<dyn VirtioInterrupt>, u16)> = None;
    let mut guard = lock(shared);
    // A plain reborrow lets the field borrows below stay disjoint.
    let sh = &mut *guard;
    let msgs: Vec<(u32, u16, u16)> = {
        let Some(mem) = sh.mem.as_ref() else {
            return;
        };
        let Some(q) = sh.queues.get_mut(3).and_then(|q| q.as_mut()) else {
            return;
        };
        let mut msgs = Vec::new();
        while let Some(chain) = q.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let mut buf = [0u8; CTRL_LEN];
            let mut len = 0;
            if let Ok(mut reader) = chain.reader(mem)
                && reader.available_bytes() >= CTRL_LEN
                && reader.read_exact(&mut buf).is_ok()
            {
                len = CTRL_LEN as u32;
                msgs.push((
                    u32::from_le_bytes(buf[0..4].try_into().unwrap()),
                    u16::from_le_bytes(buf[4..6].try_into().unwrap()),
                    u16::from_le_bytes(buf[6..8].try_into().unwrap()),
                ));
            }
            if q.add_used(mem, head, len).is_err() {
                break;
            }
        }
        if q.needs_notification(mem).unwrap_or(false) {
            irq = sh.interrupt.clone().map(|irq| (irq, 3));
        }
        msgs
    };
    let max_port = sh.ports.len() as u32;
    for (id, event, value) in msgs {
        dispatch_ctrl(sh, id, event, value, max_port);
    }
    flush_pending(sh);
    drop(guard);
    if let Some((irq, queue)) = irq {
        irq.notify_queue(queue);
    }
}

/// Act on one driver control message.
fn dispatch_ctrl(sh: &mut Shared, id: u32, event: u16, value: u16, max_port: u32) {
    match event {
        EV_DEVICE_READY if value == 1 => {
            if !sh.device_ready {
                sh.device_ready = true;
                announce_ports(sh, max_port);
            }
        }
        EV_PORT_READY if value == 1 && id >= 1 && id <= max_port => {
            sh.pending.push_back(CtrlMsg {
                id,
                event: EV_PORT_NAME,
                value: 0,
                name: Some(sh.names[id as usize - 1].clone()),
            });
            // Mirror the current host connection state to a freshly added
            // port, as QEMU does when the driver acks a port.
            if sh.ports[id as usize - 1].host_open {
                sh.pending.push_back(CtrlMsg {
                    id,
                    event: EV_PORT_OPEN,
                    value: 1,
                    name: None,
                });
            }
        }
        EV_PORT_OPEN if id >= 1 && id <= max_port => {
            sh.ports[id as usize - 1].guest_open = value == 1;
        }
        _ => {}
    }
}

/// Announce only configured ports. Send their names after PORT_READY, when
/// the guest has created the port and can associate a name with it.
fn announce_ports(sh: &mut Shared, max_port: u32) {
    for id in 1..=max_port {
        sh.pending.push_back(CtrlMsg {
            id,
            event: EV_PORT_ADD,
            value: 1,
            name: None,
        });
    }
}

/// Deliver pending control messages into free control-receiveq buffers.
fn flush_pending(sh: &mut Shared) {
    let mut irq: Option<(Arc<dyn VirtioInterrupt>, u16)> = None;
    {
        let Some(mem) = sh.mem.as_ref() else {
            return;
        };
        let Some(q) = sh.queues.get_mut(2).and_then(|q| q.as_mut()) else {
            return;
        };
        while let Some(msg) = sh.pending.front().cloned() {
            let Some(chain) = q.pop_descriptor_chain(mem) else {
                break;
            };
            let head = chain.head_index();
            let mut body = Vec::with_capacity(CTRL_LEN + 32);
            body.extend_from_slice(&msg.id.to_le_bytes());
            body.extend_from_slice(&msg.event.to_le_bytes());
            body.extend_from_slice(&msg.value.to_le_bytes());
            if let Some(name) = &msg.name {
                body.extend_from_slice(name.as_bytes());
            }
            let mut written = body.len() as u32;
            match chain.writer(mem) {
                Ok(mut writer) if writer.available_bytes() >= body.len() => {
                    if writer.write_all(&body).is_err() {
                        written = 0;
                    }
                }
                _ => written = 0,
            }
            if q.add_used(mem, head, written).is_err() {
                break;
            }
            if written == 0 {
                // Buffer too small for this message; it stays queued and the
                // next control-receiveq kick retries.
                break;
            }
            sh.pending.pop_front();
        }
        if q.needs_notification(mem).unwrap_or(false) {
            irq = sh.interrupt.clone().map(|irq| (irq, 2));
        }
    }
    if let Some((irq, queue)) = irq {
        irq.notify_queue(queue);
    }
}

// ---- VirtioDevice -----------------------------------------------------------

impl VirtioDevice for Console {
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
        F_MULTIPORT | F_VERSION_1
    }

    fn ack_features(&mut self, _features: u64) {}

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0u8; CONFIG_LEN];
        config[4..8].copy_from_slice(&self.max_nr_ports().to_le_bytes());
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = config.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // Config is read-only for the driver; `emerg_wr` is not offered and
        // writes to it are accepted and discarded.
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Arc<dyn VirtioInterrupt>,
        queues: Vec<Queue>,
    ) -> Result<(), DeviceError> {
        if queues.len() != self.queue_count() {
            return Err(DeviceError::Other(format!(
                "virtio-console needs {} queues, got {}",
                self.queue_count(),
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
        // Let the worker reconcile and retry pending delivery.
        self.wake_worker();
        Ok(())
    }

    fn queue_notify(&mut self, index: u16) {
        if self.kicks.send(Cmd::Kick(index)).is_ok() {
            self.wake_worker();
        }
    }

    fn reset(&mut self) {
        self.pending_queues = None;
        let mut sh = lock(&self.shared);
        sh.queues = (0..self.queue_count()).map(|_| None).collect();
        sh.mem = None;
        sh.interrupt = None;
        sh.device_ready = false;
        sh.pending.clear();
        for port in &mut sh.ports {
            port.guest_open = false;
            port.rx_stage.clear();
            port.tx_stage.clear();
        }
    }
}

// ---- Snapshot ----------------------------------------------------------------

/// Snapshot format magic and version.
const SNAPSHOT_MAGIC: &[u8; 6] = b"VLCONS";
const SNAPSHOT_VERSION: u16 = 1;

impl Snapshot for Console {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let sh = lock(&self.shared);
        let mut enc = Enc::new();
        enc.raw(SNAPSHOT_MAGIC);
        enc.u16(SNAPSHOT_VERSION);
        enc.u8(sh.device_ready as u8);
        enc.u32(sh.ports.len() as u32);
        for port in &sh.ports {
            enc.u8(port.host_open as u8);
            enc.u8(port.guest_open as u8);
            enc.bytes(&port.rx_stage);
            enc.bytes(&port.tx_stage);
        }
        enc.u32(sh.pending.len() as u32);
        for msg in &sh.pending {
            enc.u32(msg.id);
            enc.u16(msg.event);
            enc.u16(msg.value);
            match &msg.name {
                Some(name) => {
                    enc.u8(1);
                    enc.bytes(name.as_bytes());
                }
                None => enc.u8(0),
            }
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
            return Err(DeviceError::State("not a virtio-console snapshot".into()));
        }
        if dec.u16()? != SNAPSHOT_VERSION {
            return Err(DeviceError::State(
                "unsupported virtio-console snapshot version".into(),
            ));
        }
        let device_ready = dec.u8()? == 1;
        let nports = dec.u32()? as usize;
        if nports != self.ports.len() {
            return Err(DeviceError::State(format!(
                "snapshot has {nports} ports, device has {}",
                self.ports.len()
            )));
        }
        let mut ports = Vec::with_capacity(nports);
        for _ in 0..nports {
            // `host_open` is decoded for forward compatibility but ignored:
            // the worker's live connection state is the truth.
            let _host_open = dec.u8()? == 1;
            ports.push(PortState {
                host_open: false,
                guest_open: dec.u8()? == 1,
                rx_stage: dec.bytes()?,
                tx_stage: dec.bytes()?,
            });
        }
        let npending = dec.u32()? as usize;
        let mut pending = VecDeque::new();
        for _ in 0..npending {
            let id = dec.u32()?;
            let event = dec.u16()?;
            let value = dec.u16()?;
            let name = if dec.u8()? == 1 {
                Some(
                    String::from_utf8(dec.bytes()?)
                        .map_err(|_| DeviceError::State("invalid port name in snapshot".into()))?,
                )
            } else {
                None
            };
            pending.push_back(CtrlMsg {
                id,
                event,
                value,
                name,
            });
        }
        let nqueues = dec.u32()? as usize;
        if nqueues != self.queue_count() {
            return Err(DeviceError::State(format!(
                "snapshot has {nqueues} queues, device needs {}",
                self.queue_count()
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
        sh.device_ready = device_ready;
        for (port, restored) in sh.ports.iter_mut().zip(ports) {
            port.guest_open = restored.guest_open;
            port.rx_stage = restored.rx_stage;
            port.tx_stage = restored.tx_stage;
        }
        sh.pending = pending;
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
            .ok_or_else(|| DeviceError::State("overflowed virtio-console snapshot".into()))?;
        if end > self.buf.len() {
            return Err(DeviceError::State(
                "truncated virtio-console snapshot".into(),
            ));
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
    use std::sync::atomic::AtomicU32;
    use std::time::{Duration, Instant};
    use vm_memory::{Bytes, GuestAddress};

    /// Guest memory: 32 MiB, room for the eight queues' rings plus buffers.
    const MEM_SIZE: u64 = 288 * 1024 * 1024;
    /// Where the tests put data buffers, above the rings.
    const DATA_BASE: u64 = 0x1000_0000;

    const VRING_DESC_F_NEXT: u16 = 1;
    const VRING_DESC_F_WRITE: u16 = 2;

    static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

    /// A throwaway directory for the port sockets.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            let path = std::env::temp_dir().join(format!(
                "vmlab-hv-console-{}-{}",
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
        fn queue_notified(&self, queue: u16) -> bool {
            self.queues.lock().contains(&queue)
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

    /// A driver-side split virtqueue, hand-built in guest memory: descriptor
    /// table, available ring and used ring, plus the descriptor bookkeeping
    /// to add chains to it.
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

        /// The guest address in the descriptor with this index.
        fn desc_addr(&self, mem: &GuestMemory, idx: u16) -> u64 {
            let mut raw = [0u8; 8];
            mem.read_slice(&mut raw, GuestAddress(self.desc + 16 * u64::from(idx)))
                .unwrap();
            u64::from_le_bytes(raw)
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

    /// A two-port console (vmlab's ctl and agent) wired to a guest memory
    /// with all eight queues built and the device activated.
    struct Fixture {
        _dir: TempDir,
        mem: GuestMemory,
        dev: Console,
        irq: Arc<TestInterrupt>,
        vqs: Vec<TestVq>,
        ctl_sock: PathBuf,
        agent_sock: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir = TempDir::new();
            let ctl_sock = dir.0.join("ctl.sock");
            let agent_sock = dir.0.join("agent.sock");
            let dev = Console::new(
                "console",
                vec![
                    PortConfig {
                        name: "vmlab.ctl.0".into(),
                        path: ctl_sock.clone(),
                    },
                    PortConfig {
                        name: "vmlab.agent.0".into(),
                        path: agent_sock.clone(),
                    },
                ],
            )
            .unwrap();
            let mem = GuestMemory::from_ranges(&[(GuestAddress(0), MEM_SIZE as usize)]).unwrap();
            let irq = Arc::new(TestInterrupt::default());
            let vqs: Vec<TestVq> = dev
                .queue_max_sizes()
                .iter()
                .enumerate()
                .map(|(i, &size)| TestVq::new(0x1000 + 0x1_0000 * i as u64, size))
                .collect();
            let mut f = Fixture {
                _dir: dir,
                mem,
                dev,
                irq,
                vqs,
                ctl_sock,
                agent_sock,
            };
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

        /// Post one control-receiveq buffer (queue 2) at scratch buffer `i`.
        fn post_ctrl_rx(&mut self, i: u64) {
            let addr = self.data(i);
            self.vqs[2].add_chain(&self.mem, &[(addr, 128, true)]);
        }

        /// Send one control message on the control transmitq (queue 3).
        fn send_ctrl(&mut self, id: u32, event: u16, value: u16) {
            let addr = self.data(20);
            let mut msg = Vec::new();
            msg.extend_from_slice(&id.to_le_bytes());
            msg.extend_from_slice(&event.to_le_bytes());
            msg.extend_from_slice(&value.to_le_bytes());
            self.mem.write_slice(&msg, GuestAddress(addr)).unwrap();
            self.vqs[3].add_chain(&self.mem, &[(addr, 8, false)]);
            self.dev.queue_notify(3);
        }

        /// The control messages delivered so far as `(id, event, value,
        /// trailing bytes)`.
        fn ctrl_delivered(&self) -> Vec<(u32, u16, u16, Vec<u8>)> {
            let n = usize::from(self.vqs[2].used_idx(&self.mem));
            (0..n)
                .map(|i| {
                    let (head, len) = self.vqs[2].used_entry(&self.mem, i as u16);
                    let addr = self.vqs[2].desc_addr(&self.mem, head as u16);
                    let mut body = vec![0u8; len as usize];
                    self.mem.read_slice(&mut body, GuestAddress(addr)).unwrap();
                    (
                        u32::from_le_bytes(body[0..4].try_into().unwrap()),
                        u16::from_le_bytes(body[4..6].try_into().unwrap()),
                        u16::from_le_bytes(body[6..8].try_into().unwrap()),
                        body[8..].to_vec(),
                    )
                })
                .collect()
        }

        /// Complete the driver bring-up: DEVICE_READY, then wait until
        /// `want` control messages have been delivered.
        fn device_ready(&mut self, buffers: u64, want: usize) {
            for i in 0..buffers {
                self.post_ctrl_rx(i);
            }
            self.send_ctrl(BAD_ID_FOR_TEST, EV_DEVICE_READY, 1);
            wait_for("port announcements", || {
                self.vqs[3].used_idx(&self.mem) == 1
            });
            for id in 1..=2 {
                self.send_ctrl(id, EV_PORT_READY, 1);
                wait_for("port ready acknowledgement", || {
                    self.vqs[3].used_idx(&self.mem) == id as u16 + 1
                });
            }
            wait_for("control messages", || {
                usize::from(self.vqs[2].used_idx(&self.mem)) >= want
            });
        }
    }

    /// The port id the driver puts in DEVICE_READY (VIRTIO_CONSOLE_BAD_ID).
    const BAD_ID_FOR_TEST: u32 = u32::MAX;

    #[test]
    fn transmit_staging_is_bounded_when_client_stops_reading() {
        let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 2 << 20)]).unwrap();
        let shared = Arc::new(Mutex::new(Shared {
            mem: Some(mem.clone()),
            interrupt: None,
            queues: (0..6).map(|_| None).collect(),
            names: vec!["test".into()],
            device_ready: true,
            pending: VecDeque::new(),
            ports: vec![PortState {
                host_open: true,
                guest_open: true,
                rx_stage: vec![],
                tx_stage: vec![],
            }],
        }));
        let (mut producer, _client) = UnixStream::pair().unwrap();
        producer.set_nonblocking(true).unwrap();
        let payload = vec![23; READ_CHUNK];
        while producer.write(&payload).is_ok() {}
        let mut clients = vec![Some(producer)];
        mem.write_slice(&payload, GuestAddress(0x100000)).unwrap();
        for iteration in 0..128 {
            let mut ring = TestVq::new(0x1000, 8);
            mem.write_obj(0u16, GuestAddress(ring.avail + 2)).unwrap();
            mem.write_obj(0u16, GuestAddress(ring.used + 2)).unwrap();
            ring.add_chain(&mem, &[(0x100000, payload.len() as u32, false)]);
            lock(&shared).queues[5] = Some(ring.queue());
            process_tx(&shared, &mut clients, 1);
            let bytes = lock(&shared).ports[0].tx_stage.len();
            assert!(
                bytes <= 1024 * 1024,
                "iteration {iteration}: staged {bytes} bytes"
            );
        }
    }

    #[test]
    fn config_space_names_the_port_count() {
        let f = Fixture::new();
        assert_eq!(f.dev.device_type(), 3);
        assert_eq!(f.dev.queue_max_sizes().len(), 8);
        assert_eq!(
            f.dev.queue_max_sizes(),
            &[128, 128, 32, 32, 128, 128, 128, 128]
        );
        assert!(f.dev.queue_max_sizes().iter().all(|&n| n > 0));
        assert_ne!(f.dev.features() & (1 << 1), 0); // VIRTIO_CONSOLE_F_MULTIPORT
        let mut config = [0u8; 12];
        f.dev.read_config(0, &mut config);
        assert_eq!(u32::from_le_bytes(config[4..8].try_into().unwrap()), 3);
    }

    #[test]
    fn port_names_wait_for_the_driver_to_create_the_port() {
        let mut f = Fixture::new();
        for i in 0..8 {
            f.post_ctrl_rx(i);
        }
        f.send_ctrl(BAD_ID_FOR_TEST, EV_DEVICE_READY, 1);
        wait_for("configured ports only", || f.vqs[3].used_idx(&f.mem) == 1);
        assert_eq!(
            f.ctrl_delivered(),
            vec![
                (1, EV_PORT_ADD, 1, Vec::new()),
                (2, EV_PORT_ADD, 1, Vec::new()),
            ]
        );
        f.send_ctrl(2, EV_PORT_READY, 1);
        wait_for("named agent port", || f.vqs[3].used_idx(&f.mem) == 2);
        assert_eq!(
            f.ctrl_delivered().last().unwrap(),
            &(2, EV_PORT_NAME, 0, b"vmlab.agent.0".to_vec())
        );
    }

    #[test]
    fn device_ready_announces_every_port_with_names() {
        let mut f = Fixture::new();
        f.device_ready(8, 4);
        let msgs = f.ctrl_delivered();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0], (1, EV_PORT_ADD, 1, Vec::new()));
        assert_eq!(msgs[1], (2, EV_PORT_ADD, 1, Vec::new()));
        assert_eq!(msgs[2], (1, EV_PORT_NAME, 0, b"vmlab.ctl.0".to_vec()));
        assert_eq!(msgs[3], (2, EV_PORT_NAME, 0, b"vmlab.agent.0".to_vec()));
        // The used length of a name message covers header plus name.
        let (_, len) = f.vqs[2].used_entry(&f.mem, 2);
        assert_eq!(len as usize, 8 + "vmlab.ctl.0".len());
    }

    #[test]
    fn port_open_follows_the_host_connection() {
        let mut f = Fixture::new();
        f.device_ready(8, 4);
        f.post_ctrl_rx(8);
        let client = UnixStream::connect(&f.agent_sock).unwrap();
        wait_for("PORT_OPEN(2,1)", || {
            f.ctrl_delivered()
                .last()
                .is_some_and(|m| *m == (2, EV_PORT_OPEN, 1, Vec::new()))
        });
        drop(client);
        f.post_ctrl_rx(9);
        wait_for("PORT_OPEN(2,0)", || {
            f.ctrl_delivered()
                .last()
                .is_some_and(|m| *m == (2, EV_PORT_OPEN, 0, Vec::new()))
        });
    }

    #[test]
    fn guest_output_reaches_the_connected_client() {
        let mut f = Fixture::new();
        let mut client = UnixStream::connect(&f.ctl_sock).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        // Bring-up with the client connected: the announcement carries the
        // port-open state, and proves the worker accepted the client.
        f.device_ready(8, 5);
        assert!(
            f.ctrl_delivered()
                .contains(&(1, EV_PORT_OPEN, 1, Vec::new()))
        );

        let addr = f.data(1);
        f.mem.write_slice(b"hello ctl", GuestAddress(addr)).unwrap();
        f.vqs[5].add_chain(&f.mem, &[(addr, 9, false)]);
        f.dev.queue_notify(5);

        let mut got = [0u8; 9];
        client.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello ctl");
        wait_for("tx buffer returned", || f.vqs[5].used_idx(&f.mem) == 1);
        let (_, len) = f.vqs[5].used_entry(&f.mem, 0);
        assert_eq!(len, 9);
    }

    #[test]
    fn host_input_reaches_the_guest_receiveq() {
        let mut f = Fixture::new();
        let mut client = UnixStream::connect(&f.ctl_sock).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        f.device_ready(8, 5);

        let addr = f.data(2);
        f.vqs[4].add_chain(&f.mem, &[(addr, 64, true)]);
        // The driver kicks after adding receive buffers, as the spec
        // requires; the device relies on it to refresh its poll interest.
        f.dev.queue_notify(4);
        client.write_all(b"ping").unwrap();
        wait_for("rx buffer used", || f.vqs[4].used_idx(&f.mem) == 1);

        let (_, len) = f.vqs[4].used_entry(&f.mem, 0);
        assert_eq!(len, 4);
        let mut got = [0u8; 4];
        f.mem.read_slice(&mut got, GuestAddress(addr)).unwrap();
        assert_eq!(&got, b"ping");
        assert!(f.irq.queue_notified(4));
    }

    #[test]
    fn guest_output_without_a_client_is_discarded() {
        let mut f = Fixture::new();
        let addr = f.data(1);
        f.mem
            .write_slice(b"nobody home", GuestAddress(addr))
            .unwrap();
        f.vqs[5].add_chain(&f.mem, &[(addr, 11, false)]);
        f.dev.queue_notify(5);
        // The buffer still returns to the driver.
        wait_for("tx buffer returned", || f.vqs[5].used_idx(&f.mem) == 1);

        // A client connecting afterwards receives nothing.
        let mut client = UnixStream::connect(&f.ctl_sock).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut got = [0u8; 11];
        assert!(client.read_exact(&mut got).is_err());
    }

    #[test]
    fn host_input_waits_for_a_receiveq_buffer() {
        let mut f = Fixture::new();
        let mut client = UnixStream::connect(&f.ctl_sock).unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        f.device_ready(8, 5);

        // Bytes with no receiveq buffer wait under backpressure, they are
        // not dropped.
        client.write_all(b"held back").unwrap();
        thread::sleep(Duration::from_millis(150));

        let addr = f.data(3);
        f.vqs[4].add_chain(&f.mem, &[(addr, 32, true)]);
        f.dev.queue_notify(4);
        wait_for("rx buffer used", || f.vqs[4].used_idx(&f.mem) == 1);
        let mut got = [0u8; 9];
        f.mem.read_slice(&mut got, GuestAddress(addr)).unwrap();
        assert_eq!(&got, b"held back");
    }

    #[test]
    fn snapshot_round_trip_preserves_queues_and_staged_bytes() {
        snapshot_round_trip(false);
    }

    #[test]
    fn snapshot_round_trip_into_active_device() {
        snapshot_round_trip(true);
    }

    fn snapshot_round_trip(activate_first: bool) {
        let mut f = Fixture::new();
        let mut client = UnixStream::connect(&f.ctl_sock).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        f.device_ready(8, 5);

        // Both directions move data, and one host write is left staged.
        let tx_addr = f.data(1);
        f.mem.write_slice(b"before", GuestAddress(tx_addr)).unwrap();
        f.vqs[5].add_chain(&f.mem, &[(tx_addr, 6, false)]);
        f.dev.queue_notify(5);
        let mut got = [0u8; 6];
        client.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"before");

        let rx_addr = f.data(2);
        f.vqs[4].add_chain(&f.mem, &[(rx_addr, 32, true)]);
        f.dev.queue_notify(4);
        client.write_all(b"one").unwrap();
        wait_for("rx buffer used", || f.vqs[4].used_idx(&f.mem) == 1);

        // A deliberately tiny receive buffer: the next host write lands
        // partly in the guest and partly in the device's staging buffer.
        let tiny_addr = f.data(4);
        f.vqs[4].add_chain(&f.mem, &[(tiny_addr, 2, true)]);
        f.dev.queue_notify(4);
        client.write_all(b"staged").unwrap();
        wait_for("tiny buffer used", || f.vqs[4].used_idx(&f.mem) == 2);
        let mut got = [0u8; 2];
        f.mem.read_slice(&mut got, GuestAddress(tiny_addr)).unwrap();
        assert_eq!(&got, b"st");
        let used_before = f.vqs[4].used_idx(&f.mem);
        let state = f.dev.save_state().unwrap();

        // A fresh device takes the state over.
        let Fixture {
            _dir,
            mem,
            dev,
            irq,
            mut vqs,
            ctl_sock,
            agent_sock,
        } = f;
        drop(dev);
        let mut dev2 = Console::new(
            "console",
            vec![
                PortConfig {
                    name: "vmlab.ctl.0".into(),
                    path: ctl_sock,
                },
                PortConfig {
                    name: "vmlab.agent.0".into(),
                    path: agent_sock,
                },
            ],
        )
        .unwrap();
        let queues: Vec<Queue> = vqs.iter().map(|v| v.queue()).collect();
        if activate_first {
            dev2.activate(mem.clone(), irq, queues).unwrap();
            dev2.restore_state(&state).unwrap();
        } else {
            dev2.restore_state(&state).unwrap();
            dev2.activate(mem.clone(), irq, queues).unwrap();
        }

        // The driver reposts a receiveq buffer and the staged remainder
        // lands, with the used ring continuing where the snapshot left it.
        let rx_addr = DATA_BASE + 0x1_0000 * 3;
        vqs[4].add_chain(&mem, &[(rx_addr, 32, true)]);
        dev2.queue_notify(4);
        let want = used_before + 1;
        wait_for("staged bytes delivered", || vqs[4].used_idx(&mem) == want);
        let mut got = [0u8; 4];
        mem.read_slice(&mut got, GuestAddress(rx_addr)).unwrap();
        assert_eq!(&got, b"aged");
        let (_, len) = vqs[4].used_entry(&mem, used_before);
        assert_eq!(len, 4);
    }

    #[test]
    fn reset_stops_using_the_queues() {
        let mut f = Fixture::new();
        f.device_ready(8, 4);
        f.dev.reset();

        let addr = f.data(1);
        f.mem.write_slice(b"gone", GuestAddress(addr)).unwrap();
        f.vqs[5].add_chain(&f.mem, &[(addr, 4, false)]);
        f.dev.queue_notify(5);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(f.vqs[5].used_idx(&f.mem), 0);

        // A later re-activation brings the queues back. The stale pre-reset
        // available entry is still queued, so both buffers return.
        f.activate();
        f.vqs[5].add_chain(&f.mem, &[(addr, 4, false)]);
        f.dev.queue_notify(5);
        wait_for("tx buffers returned after re-activate", || {
            f.vqs[5].used_idx(&f.mem) == 2
        });
    }
}
