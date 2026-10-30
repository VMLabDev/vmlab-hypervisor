//! The test's side of the NIC: vmlab's switch socket (a unix listener the
//! hypervisor's virtio-net connects to, frames as 4-byte big-endian length +
//! one Ethernet frame) with a minimal DHCP responder, so the guest's eth0
//! comes up through a real DISCOVER/OFFER/REQUEST/ACK exchange and the test
//! can assert on raw frames from the guest's MAC.
//!
//! The DHCP server is deliberately tiny: one hardcoded lease, no options
//! beyond mask/router/dns/lease/server-id, broadcast replies — enough for
//! busybox `udhcpc`, which is what cinit runs.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;

/// The lease handed to the guest.
pub const LEASE_IP: Ipv4Addr = Ipv4Addr::new(10, 42, 0, 50);
/// Our address: server id, router, DNS.
pub const SERVER_IP: Ipv4Addr = Ipv4Addr::new(10, 42, 0, 1);
/// The switch port's own MAC on the wire.
const SWITCH_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const MASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);

/// What the switch observed.
#[derive(Debug, Default, Clone)]
pub struct SwitchStats {
    /// Frames whose source is the guest's configured MAC.
    pub frames_from_guest_mac: usize,
    pub dhcp_discover: bool,
    pub dhcp_request: bool,
    /// The lease was ACKed (the exchange completed).
    pub dhcp_acked: bool,
    /// Every (source MAC, ethertype) seen, for diagnostics.
    pub seen: Vec<([u8; 6], u16)>,
}

struct Inner {
    stats: SwitchStats,
    /// Outbound bytes not yet accepted by the client socket.
    out: Vec<u8>,
}

/// A hosted switch socket. Stops when dropped (or when the peer closes).
pub struct MiniSwitch {
    path: PathBuf,
    inner: Arc<Mutex<Inner>>,
    stopped: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MiniSwitch {
    /// Bind and serve on a background thread. Safe to bind before the
    /// hypervisor spawns: its NIC connects during machine build.
    pub fn bind(path: &Path) -> std::io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        let inner = Arc::new(Mutex::new(Inner {
            stats: SwitchStats::default(),
            out: Vec::new(),
        }));
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_inner = inner.clone();
        let thread_stopped = stopped.clone();
        let handle = std::thread::Builder::new()
            .name("test-switch".to_string())
            .spawn(move || {
                serve(listener, thread_inner, thread_stopped);
            })?;
        Ok(MiniSwitch {
            path: path.to_path_buf(),
            inner,
            stopped,
            handle: Some(handle),
        })
    }

    pub fn stats(&self) -> SwitchStats {
        self.inner.lock().stats.clone()
    }
}

impl Drop for MiniSwitch {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn serve(listener: UnixListener, inner: Arc<Mutex<Inner>>, stopped: Arc<AtomicBool>) {
    let mut clients: Vec<UnixStream> = Vec::new();
    let mut rx: Vec<u8> = Vec::new();
    let mut fresh: VecDeque<Vec<u8>> = VecDeque::new();
    while !stopped.load(Ordering::Relaxed) {
        // Accept without blocking.
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(true);
                    clients.push(stream);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return,
            }
        }
        // Read every client dry.
        let mut chunk = [0u8; 64 * 1024];
        for client in &mut clients {
            loop {
                match client.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => fresh.push_back(chunk[..n].to_vec()),
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        while let Some(bytes) = fresh.pop_front() {
            rx.extend_from_slice(&bytes);
        }
        // Parse whole length-prefixed frames out of the stream; replies are
        // queued for the clients.
        while rx.len() >= 4 {
            let len = u32::from_be_bytes(rx[..4].try_into().unwrap()) as usize;
            if len > 65536 + 14 {
                rx.clear();
                break;
            }
            if rx.len() < 4 + len {
                break;
            }
            let frame: Vec<u8> = rx[4..4 + len].to_vec();
            rx.drain(..4 + len);
            if let Some(reply) = handle_frame(&frame, &inner) {
                let mut inner = inner.lock();
                inner
                    .out
                    .extend_from_slice(&(reply.len() as u32).to_be_bytes());
                inner.out.extend_from_slice(&reply);
            }
        }
        // Write what we can; the rest waits in `out`.
        {
            let mut inner = inner.lock();
            for client in &mut clients {
                loop {
                    match client.write(&inner.out) {
                        Ok(0) => break,
                        Ok(n) => {
                            inner.out.drain(..n);
                            if inner.out.is_empty() {
                                break;
                            }
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut =>
                        {
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Record one frame and produce a reply when it is DHCP.
fn handle_frame(frame: &[u8], inner: &Arc<Mutex<Inner>>) -> Option<Vec<u8>> {
    if frame.len() < 14 {
        return None;
    }
    let src: [u8; 6] = frame[6..12].try_into().unwrap();
    let ethertype = u16::from_be_bytes(frame[12..14].try_into().unwrap());
    {
        let mut inner = inner.lock();
        inner.stats.seen.push((src, ethertype));
        if src == crate::TEST_MAC {
            inner.stats.frames_from_guest_mac += 1;
        }
    }
    if ethertype != 0x0800 {
        return None; // ARP and friends: observed, not answered
    }
    let dhcp = parse_dhcp(frame)?;
    let reply_type = match dhcp.message_type {
        1 => {
            inner.lock().stats.dhcp_discover = true;
            2 // OFFER
        }
        3 => {
            inner.lock().stats.dhcp_request = true;
            5 // ACK
        }
        _ => return None,
    };
    if reply_type == 5 {
        inner.lock().stats.dhcp_acked = true;
    }
    let mut reply = build_dhcp_reply(&dhcp, reply_type, LEASE_IP);
    reply.resize(reply.len().max(60), 0); // Ethernet minimum
    Some(reply)
}

// ---- DHCP -------------------------------------------------------------------

struct DhcpRequest {
    xid: u32,
    flags: u16,
    chaddr: [u8; 16],
    message_type: u8,
}

fn parse_dhcp(frame: &[u8]) -> Option<DhcpRequest> {
    if frame.len() < 14 + 20 + 8 + 240 {
        return None;
    }
    let ip = &frame[14..];
    if ip[0] >> 4 != 4 || ip[9] != 17 {
        return None; // not IPv4 UDP
    }
    let ihl = (ip[0] & 0x0F) as usize * 4;
    let udp = &ip[ihl..];
    let sport = u16::from_be_bytes(udp[0..2].try_into().unwrap());
    let dport = u16::from_be_bytes(udp[2..4].try_into().unwrap());
    if (sport, dport) != (68, 67) {
        return None;
    }
    let bootp = &udp[8..];
    if &bootp[236..240] != &[0x63, 0x82, 0x53, 0x63] {
        return None;
    }
    let mut message_type = 0u8;
    let mut i = 240;
    while i + 1 < bootp.len() {
        let opt = bootp[i];
        match opt {
            0 => {
                i += 1;
                continue;
            }
            255 => break,
            _ => {
                let len = bootp[i + 1] as usize;
                if opt == 53 && len == 1 && i + 2 < bootp.len() {
                    message_type = bootp[i + 2];
                }
                i += 2 + len;
            }
        }
    }
    if message_type == 0 {
        return None;
    }
    Some(DhcpRequest {
        xid: u32::from_be_bytes(bootp[4..8].try_into().unwrap()),
        flags: u16::from_be_bytes(bootp[10..12].try_into().unwrap()),
        chaddr: bootp[28..44].try_into().unwrap(),
        message_type,
    })
}

fn build_dhcp_reply(req: &DhcpRequest, message_type: u8, yiaddr: Ipv4Addr) -> Vec<u8> {
    let client_mac: [u8; 6] = req.chaddr[..6].try_into().unwrap();

    // BOOTP reply, then options.
    let mut bootp = vec![0u8; 240];
    bootp[0] = 2; // BOOTREPLY
    bootp[1] = 1; // htype ethernet
    bootp[2] = 6; // hlen
    bootp[4..8].copy_from_slice(&req.xid.to_be_bytes());
    bootp[10..12].copy_from_slice(&req.flags.to_be_bytes());
    bootp[16..20].copy_from_slice(&yiaddr.octets()); // yiaddr
    bootp[20..24].copy_from_slice(&SERVER_IP.octets()); // siaddr
    bootp[28..44].copy_from_slice(&req.chaddr);
    bootp[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);

    fn opt(bootp: &mut Vec<u8>, code: u8, data: &[u8]) {
        bootp.push(code);
        bootp.push(data.len() as u8);
        bootp.extend_from_slice(data);
    }
    opt(&mut bootp, 53, &[message_type]);
    opt(&mut bootp, 54, &SERVER_IP.octets()); // server id
    opt(&mut bootp, 51, &3600u32.to_be_bytes()); // lease time
    opt(&mut bootp, 1, &MASK.octets()); // subnet mask
    opt(&mut bootp, 3, &SERVER_IP.octets()); // router
    opt(&mut bootp, 6, &SERVER_IP.octets()); // dns
    bootp.push(255);

    // UDP (checksum 0: legal for IPv4).
    let udp_len = 8 + bootp.len();
    let mut udp = Vec::with_capacity(udp_len);
    udp.extend_from_slice(&67u16.to_be_bytes());
    udp.extend_from_slice(&68u16.to_be_bytes());
    udp.extend_from_slice(&(udp_len as u16).to_be_bytes());
    udp.extend_from_slice(&0u16.to_be_bytes());
    udp.extend_from_slice(&bootp);

    // IPv4.
    let total = 20 + udp.len();
    let mut ip = vec![0u8; 20];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    ip[4..6].copy_from_slice(&0x1234u16.to_be_bytes());
    ip[8] = 64; // ttl
    ip[9] = 17; // UDP
    ip[12..16].copy_from_slice(&SERVER_IP.octets());
    ip[16..20].copy_from_slice(&Ipv4Addr::BROADCAST.octets());
    let checksum = ip_checksum(&ip);
    ip[10..12].copy_from_slice(&checksum.to_be_bytes());

    // Ethernet: unicast to the client, IP broadcast.
    let mut out = Vec::with_capacity(14 + total);
    out.extend_from_slice(&client_mac);
    out.extend_from_slice(&SWITCH_MAC);
    out.extend_from_slice(&0x0800u16.to_be_bytes());
    out.extend_from_slice(&ip);
    out.extend_from_slice(&udp);
    out
}

fn ip_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = header
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32)
        .sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}
