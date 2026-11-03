//! The test's side of the NIC: vmlab's switch socket (a unix listener the
//! hypervisor's virtio-net connects to, frames as 4-byte big-endian length +
//! one Ethernet frame) with a minimal DHCP responder, so the guest's eth0
//! comes up through a real DISCOVER/OFFER/REQUEST/ACK exchange and the test
//! can assert on raw frames from the guest's MAC.
//!
//! The DHCP server is deliberately tiny: one hardcoded lease, no options
//! beyond mask/router/dns/lease/server-id, broadcast replies — enough for
//! busybox `udhcpc`, which is what cinit runs.

use std::collections::{BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;
use vmlab_hv_proto::net::{Header, MAX_FRAME_LEN, METADATA_LEN, NEEDS_CSUM};

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
    /// UDP echo requests with valid IPv4 and nonzero UDP checksums.
    pub udp_echoes: usize,
    /// Transmit pair ids carried by offload-framed records.
    pub queues: BTreeSet<u16>,
    /// Records which requested checksum completion or segmentation.
    pub offload_packets: usize,
    /// Ethernet frames produced by completing offload-framed records.
    pub normalized_frames: usize,
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
        Self::bind_mode(path, false)
    }

    pub fn bind_vnet(path: &Path) -> std::io::Result<Self> {
        Self::bind_mode(path, true)
    }

    fn bind_mode(path: &Path, vnet: bool) -> std::io::Result<Self> {
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
                serve(listener, thread_inner, thread_stopped, vnet);
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

fn serve(listener: UnixListener, inner: Arc<Mutex<Inner>>, stopped: Arc<AtomicBool>, vnet: bool) {
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
            if len > MAX_FRAME_LEN + if vnet { METADATA_LEN } else { 0 } {
                rx.clear();
                break;
            }
            if rx.len() < 4 + len {
                break;
            }
            let body: Vec<u8> = rx[4..4 + len].to_vec();
            rx.drain(..4 + len);
            let (queue, frames) = if vnet {
                let decoded = Header::from_metadata(&body).and_then(|(header, queue, frame)| {
                    header
                        .segments(frame, 1500)
                        .map(|frames| (header, queue, frames))
                });
                let (header, queue, frames) = match decoded {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        eprintln!("test switch: invalid vnet record: {error}");
                        continue;
                    }
                };
                let mut state = inner.lock();
                state.stats.queues.insert(queue);
                state.stats.offload_packets += usize::from(header.flags & NEEDS_CSUM != 0);
                state.stats.normalized_frames += frames.len();
                (queue, frames)
            } else {
                (0, vec![body])
            };
            for frame in frames {
                if let Some(reply) = handle_frame(&frame, &inner) {
                    let mut inner = inner.lock();
                    let metadata = vnet.then(|| Header::default().metadata(queue));
                    let len = reply.len() + metadata.as_ref().map_or(0, |bytes| bytes.len());
                    inner.out.extend_from_slice(&(len as u32).to_be_bytes());
                    if let Some(bytes) = metadata {
                        inner.out.extend_from_slice(&bytes);
                    }
                    inner.out.extend_from_slice(&reply);
                }
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

/// Record one frame and answer DHCP, gateway ARP, ICMP, and UDP echo requests.
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
    if ethertype == 0x0806
        && frame.len() >= 42
        && frame[14..22] == [0, 1, 8, 0, 6, 4, 0, 1]
        && frame[38..42] == SERVER_IP.octets()
    {
        let mut reply = frame[..42].to_vec();
        reply[..6].copy_from_slice(&src);
        reply[6..12].copy_from_slice(&SWITCH_MAC);
        reply[20..22].copy_from_slice(&2u16.to_be_bytes());
        reply[32..38].copy_from_slice(&frame[22..28]);
        reply[38..42].copy_from_slice(&frame[28..32]);
        reply[22..28].copy_from_slice(&SWITCH_MAC);
        reply[28..32].copy_from_slice(&SERVER_IP.octets());
        reply.resize(60, 0);
        return Some(reply);
    }
    if ethertype != 0x0800 {
        return None;
    }
    if let Some(reply) = echo_reply(frame) {
        return Some(reply);
    }
    if let Some(reply) = udp_echo_reply(frame) {
        inner.lock().stats.udp_echoes += 1;
        return Some(reply);
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

fn echo_reply(frame: &[u8]) -> Option<Vec<u8>> {
    let ip = frame.get(14..)?;
    if ip.len() < 20
        || ip[0] >> 4 != 4
        || ip[9] != 1
        || ip[16..20] != SERVER_IP.octets()
        || u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0
    {
        return None;
    }
    let header = usize::from(ip[0] & 15) * 4;
    let length = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    if header < 20
        || length < header + 8
        || length > ip.len()
        || ip[header] != 8
        || ip[header + 1] != 0
    {
        return None;
    }
    let mut reply = frame[..14 + length].to_vec();
    reply[..6].copy_from_slice(&frame[6..12]);
    reply[6..12].copy_from_slice(&SWITCH_MAC);
    reply[26..30].copy_from_slice(&SERVER_IP.octets());
    reply[30..34].copy_from_slice(&ip[12..16]);
    reply[24..26].fill(0);
    let ip_sum = ip_checksum(&reply[14..14 + header]);
    reply[24..26].copy_from_slice(&ip_sum.to_be_bytes());
    reply[14 + header] = 0;
    reply[16 + header..18 + header].fill(0);
    let icmp_sum = ip_checksum(&reply[14 + header..]);
    reply[16 + header..18 + header].copy_from_slice(&icmp_sum.to_be_bytes());
    reply.resize(reply.len().max(60), 0);
    Some(reply)
}

fn udp_echo_reply(frame: &[u8]) -> Option<Vec<u8>> {
    let ip = frame.get(14..)?;
    if ip.len() < 20
        || ip[0] >> 4 != 4
        || ip[9] != 17
        || ip[16..20] != SERVER_IP.octets()
        || u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0
    {
        return None;
    }
    let header = usize::from(ip[0] & 15) * 4;
    let length = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    if header < 20 || length < header + 8 || length > ip.len() || ip_checksum(&ip[..header]) != 0 {
        return None;
    }
    let udp = &ip[header..length];
    if u16::from_be_bytes([udp[2], udp[3]]) != 46000
        || usize::from(u16::from_be_bytes([udp[4], udp[5]])) != udp.len()
        || udp[6..8] == [0, 0]
        || udp_checksum(&ip[..header], udp) != 0
    {
        return None;
    }
    let mut reply = frame[..14 + length].to_vec();
    reply[..6].copy_from_slice(&frame[6..12]);
    reply[6..12].copy_from_slice(&SWITCH_MAC);
    reply[26..30].copy_from_slice(&SERVER_IP.octets());
    reply[30..34].copy_from_slice(&ip[12..16]);
    let offset = 14 + header;
    reply[offset..offset + 2].copy_from_slice(&udp[2..4]);
    reply[offset + 2..offset + 4].copy_from_slice(&udp[..2]);
    reply[24..26].fill(0);
    let checksum = ip_checksum(&reply[14..offset]);
    reply[24..26].copy_from_slice(&checksum.to_be_bytes());
    reply[offset + 6..offset + 8].fill(0);
    let checksum = udp_checksum(&reply[14..offset], &reply[offset..]);
    let checksum = if checksum == 0 { 0xffff } else { checksum };
    reply[offset + 6..offset + 8].copy_from_slice(&checksum.to_be_bytes());
    reply.resize(reply.len().max(60), 0);
    Some(reply)
}

fn udp_checksum(ip: &[u8], udp: &[u8]) -> u16 {
    let mut pseudo = ip[12..20].to_vec();
    pseudo.extend_from_slice(&[0, 17]);
    pseudo.extend_from_slice(&(udp.len() as u16).to_be_bytes());
    pseudo.extend_from_slice(udp);
    ip_checksum(&pseudo)
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
    if bootp[236..240] != [0x63, 0x82, 0x53, 0x63] {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn udp_request(payload: &[u8]) -> Vec<u8> {
        let length = 20 + 8 + payload.len();
        let mut frame = vec![0; 14 + length];
        frame[..6].copy_from_slice(&SWITCH_MAC);
        frame[6..12].copy_from_slice(&crate::TEST_MAC);
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&(length as u16).to_be_bytes());
        frame[22] = 64;
        frame[23] = 17;
        frame[26..30].copy_from_slice(&LEASE_IP.octets());
        frame[30..34].copy_from_slice(&SERVER_IP.octets());
        frame[34..36].copy_from_slice(&12345u16.to_be_bytes());
        frame[36..38].copy_from_slice(&46000u16.to_be_bytes());
        frame[38..40].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        frame[42..].copy_from_slice(payload);
        let checksum = ip_checksum(&frame[14..34]);
        frame[24..26].copy_from_slice(&checksum.to_be_bytes());
        let checksum = udp_checksum(&frame[14..34], &frame[34..]);
        frame[40..42].copy_from_slice(&checksum.to_be_bytes());
        frame
    }

    #[test]
    fn vnet_socket_segments_udp_with_valid_checksums_and_preserves_queue_id() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("switch.sock");
        let switch = MiniSwitch::bind_vnet(&socket).unwrap();
        let mut client = UnixStream::connect(socket).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let payload: Vec<u8> = (0..4097).map(|index| index as u8).collect();
        let mut frame = udp_request(&payload);
        frame[40..42].fill(0);
        let header = Header {
            flags: NEEDS_CSUM,
            gso_type: vmlab_hv_proto::net::GSO_UDP,
            hdr_len: 42,
            gso_size: 1000,
            csum_start: 34,
            csum_offset: 6,
        };
        let metadata = header.metadata(1);
        client
            .write_all(&((METADATA_LEN + frame.len()) as u32).to_be_bytes())
            .unwrap();
        client.write_all(&metadata[..3]).unwrap();
        client.write_all(&metadata[3..]).unwrap();
        client.write_all(&frame).unwrap();

        let mut received = Vec::new();
        for _ in 0..5 {
            let mut prefix = [0; 4];
            client.read_exact(&mut prefix).unwrap();
            let mut body = vec![0; u32::from_be_bytes(prefix) as usize];
            client.read_exact(&mut body).unwrap();
            let (header, queue, reply) = Header::from_metadata(&body).unwrap();
            assert_eq!(header, Header::default());
            assert_eq!(queue, 1);
            let length = usize::from(u16::from_be_bytes([reply[16], reply[17]]));
            assert!(length <= 1500);
            assert_eq!(ip_checksum(&reply[14..34]), 0);
            assert_eq!(udp_checksum(&reply[14..34], &reply[34..14 + length]), 0);
            received.extend_from_slice(&reply[42..14 + length]);
        }
        assert_eq!(received, payload);
        let stats = switch.stats();
        assert_eq!(stats.queues, BTreeSet::from([1]));
        assert_eq!(stats.offload_packets, 1);
        assert_eq!(stats.normalized_frames, 5);
        assert_eq!(stats.udp_echoes, 5);
    }

    #[test]
    fn udp_echo_requires_valid_wire_checksums_and_returns_exact_payload() {
        let payload = b"odd checksum payload!";
        let length = 20 + 8 + payload.len();
        let mut frame = vec![0; 14 + length];
        frame[..6].copy_from_slice(&SWITCH_MAC);
        frame[6..12].copy_from_slice(&crate::TEST_MAC);
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&(length as u16).to_be_bytes());
        frame[22] = 64;
        frame[23] = 17;
        frame[26..30].copy_from_slice(&LEASE_IP.octets());
        frame[30..34].copy_from_slice(&SERVER_IP.octets());
        frame[34..36].copy_from_slice(&12345u16.to_be_bytes());
        frame[36..38].copy_from_slice(&46000u16.to_be_bytes());
        frame[38..40].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        frame[42..].copy_from_slice(payload);
        let checksum = ip_checksum(&frame[14..34]);
        frame[24..26].copy_from_slice(&checksum.to_be_bytes());
        let checksum = udp_checksum(&frame[14..34], &frame[34..]);
        frame[40..42].copy_from_slice(&checksum.to_be_bytes());
        let reply = udp_echo_reply(&frame).unwrap();
        assert_eq!(&reply[42..14 + length], payload);
        assert_eq!(&reply[34..36], &46000u16.to_be_bytes());
        assert_eq!(&reply[36..38], &12345u16.to_be_bytes());
        assert_eq!(ip_checksum(&reply[14..34]), 0);
        assert_eq!(udp_checksum(&reply[14..34], &reply[34..14 + length]), 0);
        let mut bad = frame.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(udp_echo_reply(&bad).is_none());
        bad = frame.clone();
        bad[40..42].fill(0);
        assert!(udp_echo_reply(&bad).is_none());
        bad = frame.clone();
        bad[24] ^= 1;
        assert!(udp_echo_reply(&bad).is_none());
        for len in 0..frame.len() {
            assert!(udp_echo_reply(&frame[..len]).is_none());
        }
    }

    #[test]
    fn gateway_answers_arp_and_icmp_over_the_switch_socket() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("switch.sock");
        let switch = MiniSwitch::bind(&socket).unwrap();
        let mut client = UnixStream::connect(socket).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut exchange = |frame: &[u8]| {
            client
                .write_all(&(frame.len() as u32).to_be_bytes())
                .unwrap();
            client.write_all(frame).unwrap();
            let mut size = [0; 4];
            client.read_exact(&mut size).unwrap();
            let mut reply = vec![0; u32::from_be_bytes(size) as usize];
            client.read_exact(&mut reply).unwrap();
            reply
        };
        let mut arp = vec![0; 42];
        arp[..6].fill(255);
        arp[6..12].copy_from_slice(&crate::TEST_MAC);
        arp[12..22].copy_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
        arp[22..28].copy_from_slice(&crate::TEST_MAC);
        arp[28..32].copy_from_slice(&LEASE_IP.octets());
        arp[38..42].copy_from_slice(&SERVER_IP.octets());
        let reply = exchange(&arp);
        assert_eq!(&reply[..6], &crate::TEST_MAC);
        assert_eq!(&reply[6..12], &SWITCH_MAC);
        assert_eq!(&reply[20..22], &[0, 2]);
        assert_eq!(&reply[22..28], &SWITCH_MAC);
        assert_eq!(&reply[28..32], &SERVER_IP.octets());
        assert_eq!(&reply[32..38], &crate::TEST_MAC);
        assert_eq!(&reply[38..42], &LEASE_IP.octets());

        let mut echo = vec![0; 14 + 20 + 8 + 5];
        echo[..6].copy_from_slice(&SWITCH_MAC);
        echo[6..12].copy_from_slice(&crate::TEST_MAC);
        echo[12..14].copy_from_slice(&[8, 0]);
        echo[14] = 0x45;
        echo[16..18].copy_from_slice(&33u16.to_be_bytes());
        echo[22] = 64;
        echo[23] = 1;
        echo[26..30].copy_from_slice(&LEASE_IP.octets());
        echo[30..34].copy_from_slice(&SERVER_IP.octets());
        echo[34] = 8;
        echo[38..42].copy_from_slice(&[0x12, 0x34, 0, 1]);
        echo[42..].copy_from_slice(b"hello");
        let checksum = ip_checksum(&echo[14..34]);
        echo[24..26].copy_from_slice(&checksum.to_be_bytes());
        let checksum = ip_checksum(&echo[34..]);
        echo[36..38].copy_from_slice(&checksum.to_be_bytes());
        let reply = exchange(&echo);
        assert_eq!(&reply[..6], &crate::TEST_MAC);
        assert_eq!(&reply[6..12], &SWITCH_MAC);
        assert_eq!(&reply[26..30], &SERVER_IP.octets());
        assert_eq!(&reply[30..34], &LEASE_IP.octets());
        assert_eq!(reply[34], 0);
        assert_eq!(&reply[38..47], &echo[38..]);
        assert_eq!(ip_checksum(&reply[14..34]), 0);
        assert_eq!(ip_checksum(&reply[34..47]), 0);
        assert_eq!(switch.stats().frames_from_guest_mac, 2);
        echo[20] = 0x20;
        assert!(echo_reply(&echo).is_none(), "fragmented echo request");
        echo[20] = 0;
        for length in 0..echo.len() {
            assert!(echo_reply(&echo[..length]).is_none());
        }
    }
}
