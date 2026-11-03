//! Versioned switch metadata and software offload completion (VIRTIO 1.2 §5.1).
//!
//! A vnet stream record contains a big-endian u32 body length, version (u16,
//! big-endian, currently 1), queue id (u16, big-endian), a 12-byte little-endian
//! virtio-net header, and Ethernet bytes. Port configuration selects this
//! format before connection; legacy stream records remain bare Ethernet.

use std::io;

pub const MAX_FRAME_LEN: usize = 65536 + 14;
pub const METADATA_LEN: usize = 16;
pub const NEEDS_CSUM: u8 = 1;
pub const DATA_VALID: u8 = 2;
pub const GSO_NONE: u8 = 0;
pub const GSO_TCPV4: u8 = 1;
pub const GSO_TCPV6: u8 = 4;
pub const GSO_UDP: u8 = 5;
pub const GSO_ECN: u8 = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid virtio-net offload packet",
    )
}

fn be16(bytes: &[u8], at: usize) -> io::Result<u16> {
    let word = bytes.get(at..at + 2).ok_or_else(invalid)?;
    Ok(u16::from_be_bytes([word[0], word[1]]))
}

impl Header {
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 10 {
            return Err(invalid());
        }
        let word = |at| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let header = Self {
            flags: bytes[0],
            gso_type: bytes[1],
            hdr_len: word(2),
            gso_size: word(4),
            csum_start: word(6),
            csum_offset: word(8),
        };
        if header.flags & !(NEEDS_CSUM | DATA_VALID) != 0
            || header.flags == NEEDS_CSUM | DATA_VALID
            || !matches!(
                header.gso_type & !GSO_ECN,
                GSO_NONE | GSO_TCPV4 | GSO_TCPV6 | GSO_UDP
            )
            || (header.gso_type & GSO_ECN != 0
                && !matches!(header.gso_type & !GSO_ECN, GSO_TCPV4 | GSO_TCPV6))
        {
            return Err(invalid());
        }
        Ok(header)
    }

    pub fn encode(self) -> [u8; 12] {
        let mut bytes = [0; 12];
        bytes[0] = self.flags;
        bytes[1] = self.gso_type;
        for (at, word) in [
            (2, self.hdr_len),
            (4, self.gso_size),
            (6, self.csum_start),
            (8, self.csum_offset),
        ] {
            bytes[at..at + 2].copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    /// Check offsets and the protocol before trusting metadata from a peer.
    pub fn validate(self, frame: &[u8]) -> io::Result<()> {
        Self::decode(&self.encode())?;
        if frame.len() > MAX_FRAME_LEN || frame.len() < 14 {
            return Err(invalid());
        }
        if self.flags & NEEDS_CSUM != 0 {
            let field = usize::from(self.csum_start) + usize::from(self.csum_offset);
            if field + 2 > frame.len() {
                return Err(invalid());
            }
        }
        let kind = self.gso_type & !GSO_ECN;
        if kind != GSO_NONE {
            let layout = Layout::parse(frame)?;
            let tcp = matches!(kind, GSO_TCPV4 | GSO_TCPV6);
            if self.gso_size == 0
                || self.flags & NEEDS_CSUM == 0
                || usize::from(self.csum_start) != layout.transport
                || self.csum_offset != if tcp { 16 } else { 6 }
                || (tcp && layout.protocol != 6)
                || (!tcp && layout.protocol != 17)
                || (kind == GSO_TCPV4 && !layout.ipv4)
                || (kind == GSO_TCPV6 && layout.ipv4)
            {
                return Err(invalid());
            }
        }
        Ok(())
    }

    pub fn metadata(self, queue: u16) -> [u8; METADATA_LEN] {
        let mut bytes = [0; METADATA_LEN];
        bytes[..2].copy_from_slice(&1u16.to_be_bytes());
        bytes[2..4].copy_from_slice(&queue.to_be_bytes());
        bytes[4..].copy_from_slice(&self.encode());
        bytes
    }

    pub fn from_metadata(body: &[u8]) -> io::Result<(Self, u16, &[u8])> {
        if body.len() < METADATA_LEN || be16(body, 0)? != 1 {
            return Err(invalid());
        }
        let header = Self::decode(&body[4..METADATA_LEN])?;
        let frame = &body[METADATA_LEN..];
        header.validate(frame)?;
        Ok((header, be16(body, 2)?, frame))
    }

    /// Complete a partial checksum or segment a GSO packet for a raw port.
    /// `mtu` counts network-layer bytes, excluding Ethernet and VLAN headers.
    pub fn segments(self, frame: &[u8], mtu: u16) -> io::Result<Vec<Vec<u8>>> {
        self.validate(frame)?;
        if self.gso_type == GSO_NONE {
            let mut frame = frame.to_vec();
            if self.flags & NEEDS_CSUM != 0 {
                let start = usize::from(self.csum_start);
                let field = start + usize::from(self.csum_offset);
                let value = checksum(&frame[start..]);
                let value = if value == 0 { 0xffff } else { value };
                frame[field..field + 2].copy_from_slice(&value.to_be_bytes());
            }
            return Ok(vec![frame]);
        }
        let layout = Layout::parse(frame)?;
        let transport = layout.transport;
        let tcp = layout.protocol == 6;
        let payload = if tcp {
            let len = usize::from(*frame.get(transport + 12).ok_or_else(invalid)? >> 4) * 4;
            if len < 20 {
                return Err(invalid());
            }
            transport + len
        } else {
            transport + 8
        };
        if payload > frame.len() {
            return Err(invalid());
        }
        let budget = usize::from(mtu)
            .checked_sub(payload - layout.ip)
            .ok_or_else(invalid)?;
        let size = budget.min(usize::from(self.gso_size));
        if size == 0 {
            return Err(invalid());
        }
        let data = &frame[payload..];
        // USO boundaries are datagram boundaries. Reducing them changes the
        // application's messages, unlike reducing a TCP segment's MSS.
        if !tcp && usize::from(self.gso_size).min(data.len()) > budget {
            return Err(invalid());
        }
        let count = data.len().div_ceil(size).max(1);
        // Pathological extension headers plus gso_size=1 must not amplify one
        // 64 KiB input into hundreds of MiB of buffered output.
        if count
            .checked_mul(payload)
            .and_then(|headers| headers.checked_add(data.len()))
            .is_none_or(|bytes| bytes > 4 * 1024 * 1024)
        {
            return Err(invalid());
        }
        let mut result = Vec::with_capacity(count);
        for index in 0..count {
            let offset = index * size;
            let end = (offset + size).min(data.len());
            let mut part = frame[..payload].to_vec();
            part.extend_from_slice(&data[offset..end]);
            if layout.ipv4 {
                let length = u16::try_from(part.len() - layout.ip).map_err(|_| invalid())?;
                part[layout.ip + 2..layout.ip + 4].copy_from_slice(&length.to_be_bytes());
                let id = be16(frame, layout.ip + 4)?.wrapping_add(index as u16);
                part[layout.ip + 4..layout.ip + 6].copy_from_slice(&id.to_be_bytes());
                part[layout.ip + 10..layout.ip + 12].fill(0);
                let value = checksum(&part[layout.ip..transport]);
                part[layout.ip + 10..layout.ip + 12].copy_from_slice(&value.to_be_bytes());
            } else {
                let length = u16::try_from(part.len() - layout.ip - 40).map_err(|_| invalid())?;
                part[layout.ip + 4..layout.ip + 6].copy_from_slice(&length.to_be_bytes());
            }
            let field = if tcp {
                let sequence =
                    u32::from_be_bytes(frame[transport + 4..transport + 8].try_into().unwrap());
                part[transport + 4..transport + 8]
                    .copy_from_slice(&sequence.wrapping_add(offset as u32).to_be_bytes());
                if index + 1 != count {
                    part[transport + 13] &= !(1 | 8);
                }
                if index != 0 {
                    part[transport + 13] &= !128;
                }
                transport + 16
            } else {
                let length = (part.len() - transport) as u16;
                part[transport + 4..transport + 6].copy_from_slice(&length.to_be_bytes());
                transport + 6
            };
            part[field..field + 2].fill(0);
            let value = layout.transport_checksum(&part);
            let value = if !tcp && value == 0 { 0xffff } else { value };
            part[field..field + 2].copy_from_slice(&value.to_be_bytes());
            result.push(part);
        }
        Ok(result)
    }
}

struct Layout {
    ip: usize,
    transport: usize,
    protocol: u8,
    ipv4: bool,
}

impl Layout {
    fn parse(frame: &[u8]) -> io::Result<Self> {
        let mut ip = 14;
        let mut kind = be16(frame, 12)?;
        for _ in 0..2 {
            if !matches!(kind, 0x8100 | 0x88a8) {
                break;
            }
            kind = be16(frame, ip + 2)?;
            ip += 4;
        }
        let (transport, protocol, ipv4) = match kind {
            0x0800 => {
                let first = *frame.get(ip).ok_or_else(invalid)?;
                let len = usize::from(first & 15) * 4;
                if first >> 4 != 4 || len < 20 || be16(frame, ip + 6)? & 0x3fff != 0 {
                    return Err(invalid());
                }
                (ip + len, *frame.get(ip + 9).ok_or_else(invalid)?, true)
            }
            0x86dd => {
                if frame.get(ip).is_none_or(|first| first >> 4 != 6) {
                    return Err(invalid());
                }
                let mut protocol = *frame.get(ip + 6).ok_or_else(invalid)?;
                let mut at = ip + 40;
                for _ in 0..8 {
                    if !matches!(protocol, 0 | 43 | 60 | 51) {
                        break;
                    }
                    let next = *frame.get(at).ok_or_else(invalid)?;
                    let len = usize::from(*frame.get(at + 1).ok_or_else(invalid)?);
                    at += if protocol == 51 {
                        (len + 2) * 4
                    } else {
                        (len + 1) * 8
                    };
                    protocol = next;
                }
                (at, protocol, false)
            }
            _ => return Err(invalid()),
        };
        let min = match protocol {
            6 => 20,
            17 => 8,
            _ => return Err(invalid()),
        };
        if transport + min > frame.len() {
            return Err(invalid());
        }
        Ok(Self {
            ip,
            transport,
            protocol,
            ipv4,
        })
    }

    fn transport_checksum(&self, frame: &[u8]) -> u16 {
        let len = (frame.len() - self.transport) as u32;
        let mut sum = Sum::default();
        if self.ipv4 {
            sum.add(&frame[self.ip + 12..self.ip + 20]);
            sum.add(&[0, self.protocol]);
            sum.add(&(len as u16).to_be_bytes());
        } else {
            sum.add(&frame[self.ip + 8..self.ip + 40]);
            sum.add(&len.to_be_bytes());
            sum.add(&[0, 0, 0, self.protocol]);
        }
        sum.add(&frame[self.transport..]);
        sum.finish()
    }
}

#[derive(Default)]
struct Sum(u64);

impl Sum {
    fn add(&mut self, bytes: &[u8]) {
        let (words, odd) = bytes.as_chunks::<2>();
        for word in words {
            self.0 += u64::from(u16::from_be_bytes(*word));
        }
        if let Some(byte) = odd.first() {
            self.0 += u64::from(*byte) << 8;
        }
    }
    fn finish(mut self) -> u16 {
        while self.0 > 0xffff {
            self.0 = (self.0 & 0xffff) + (self.0 >> 16);
        }
        !(self.0 as u16)
    }
}

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = Sum::default();
    sum.add(bytes);
    sum.finish()
}

/// Stable, bidirectional flow hash for automatic receive steering.
pub fn flow_hash(frame: &[u8]) -> u32 {
    let mut hash = 2166136261u32;
    let mut add = |bytes: &[u8]| {
        for byte in bytes {
            hash = (hash ^ u32::from(*byte)).wrapping_mul(16777619);
        }
    };
    if let Ok(layout) = Layout::parse(frame) {
        let (source, destination) = if layout.ipv4 {
            (
                &frame[layout.ip + 12..layout.ip + 16],
                &frame[layout.ip + 16..layout.ip + 20],
            )
        } else {
            (
                &frame[layout.ip + 8..layout.ip + 24],
                &frame[layout.ip + 24..layout.ip + 40],
            )
        };
        let source_port = &frame[layout.transport..layout.transport + 2];
        let destination_port = &frame[layout.transport + 2..layout.transport + 4];
        if (source, source_port) <= (destination, destination_port) {
            add(source);
            add(source_port);
            add(destination);
            add(destination_port);
        } else {
            add(destination);
            add(destination_port);
            add(source);
            add(source_port);
        }
        add(&[layout.protocol]);
    } else {
        add(&frame[..frame.len().min(14)]);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(ipv4: bool, tcp: bool, len: usize) -> (Header, Vec<u8>) {
        let transport = if ipv4 { 34 } else { 54 };
        let end = transport + if tcp { 20 } else { 8 };
        let mut frame = vec![0; end + len];
        frame[12..14].copy_from_slice(&(if ipv4 { 0x0800u16 } else { 0x86ddu16 }).to_be_bytes());
        if ipv4 {
            frame[14] = 0x45;
            frame[23] = if tcp { 6 } else { 17 };
            frame[26..34].copy_from_slice(&[10, 0, 0, 1, 10, 0, 0, 2]);
        } else {
            frame[14] = 0x60;
            frame[20] = if tcp { 6 } else { 17 };
            frame[22] = 0x20;
            frame[38] = 0x30;
        }
        frame[transport..transport + 4].copy_from_slice(&[1, 2, 3, 4]);
        if tcp {
            frame[transport + 12] = 0x50;
            frame[transport + 13] = 0x99;
            frame[transport + 4..transport + 8].copy_from_slice(&0xfffffff0u32.to_be_bytes());
        }
        for (index, byte) in frame[end..].iter_mut().enumerate() {
            *byte = index as u8;
        }
        (
            Header {
                flags: NEEDS_CSUM,
                gso_type: if tcp {
                    if ipv4 { GSO_TCPV4 } else { GSO_TCPV6 }
                } else {
                    GSO_UDP
                },
                hdr_len: end as u16,
                gso_size: 1400,
                csum_start: transport as u16,
                csum_offset: if tcp { 16 } else { 6 },
            },
            frame,
        )
    }

    #[test]
    fn metadata_round_trip_and_rejects_malformed() {
        let (header, frame) = packet(true, true, 4000);
        let mut body = header.metadata(7).to_vec();
        body.extend_from_slice(&frame);
        assert_eq!(
            Header::from_metadata(&body).unwrap(),
            (header, 7, frame.as_slice())
        );
        for len in 0..METADATA_LEN {
            assert!(Header::from_metadata(&body[..len]).is_err());
        }
        body[1] = 2;
        assert!(Header::from_metadata(&body).is_err());
        let mut bad = header;
        bad.csum_offset = u16::MAX;
        assert!(bad.segments(&frame, 1500).is_err());
        bad = header;
        bad.gso_size = 0;
        assert!(bad.segments(&frame, 1500).is_err());
    }

    #[test]
    fn tso_and_uso_have_mtu_lengths_payload_and_checksums() {
        for ipv4 in [true, false] {
            for tcp in [true, false] {
                let (header, frame) = packet(ipv4, tcp, 9999);
                let parts = header.segments(&frame, 1500).unwrap();
                assert!(parts.len() > 1);
                let original = Layout::parse(&frame).unwrap();
                let payload = original.transport + if tcp { 20 } else { 8 };
                let mut received = Vec::new();
                for (index, part) in parts.iter().enumerate() {
                    assert!(part.len() - 14 <= 1500);
                    let layout = Layout::parse(part).unwrap();
                    assert_eq!(layout.transport_checksum(part), 0);
                    if ipv4 {
                        assert_eq!(checksum(&part[14..layout.transport]), 0);
                        assert_eq!(usize::from(be16(part, 16).unwrap()), part.len() - 14);
                    } else {
                        assert_eq!(usize::from(be16(part, 18).unwrap()), part.len() - 54);
                    }
                    if tcp {
                        let sequence = u32::from_be_bytes(
                            part[layout.transport + 4..layout.transport + 8]
                                .try_into()
                                .unwrap(),
                        );
                        assert_eq!(sequence, 0xfffffff0u32.wrapping_add(received.len() as u32));
                        assert_eq!(
                            part[layout.transport + 13] & 9,
                            if index + 1 == parts.len() { 9 } else { 0 }
                        );
                        assert_eq!(
                            part[layout.transport + 13] & 128,
                            if index == 0 { 128 } else { 0 }
                        );
                    } else {
                        assert_eq!(
                            usize::from(be16(part, layout.transport + 4).unwrap()),
                            part.len() - layout.transport
                        );
                    }
                    assert_eq!(flow_hash(part), flow_hash(&frame));
                    received.extend_from_slice(&part[payload..]);
                }
                assert_eq!(received, frame[payload..]);
            }
        }
    }

    #[test]
    fn partial_checksum_and_raw_frames() {
        let (mut header, mut frame) = packet(true, true, 101);
        header.gso_type = GSO_NONE;
        let layout = Layout::parse(&frame).unwrap();
        let mut pseudo = Sum::default();
        pseudo.add(&frame[26..34]);
        pseudo.add(&[0, 6]);
        pseudo.add(&((frame.len() - 34) as u16).to_be_bytes());
        frame[50..52].copy_from_slice(&(!pseudo.finish()).to_be_bytes());
        let completed = header.segments(&frame, 1500).unwrap();
        assert_eq!(layout.transport_checksum(&completed[0]), 0);
        assert_eq!(
            Header::default().segments(&completed[0], 1500).unwrap(),
            completed
        );
    }

    #[test]
    fn zero_partial_checksum_is_transmitted_as_ones() {
        let (mut header, mut frame) = packet(false, false, 2);
        header.gso_type = GSO_NONE;
        frame[22..38].copy_from_slice(&[0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        frame[38..54].copy_from_slice(&[0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        frame[54..].copy_from_slice(&[0x04, 0xd2, 0x10, 0xe1, 0, 0x0a, 0x5b, 0x90, 0x8e, 0xb2]);
        let parts = header.segments(&frame, 1500).unwrap();
        assert_eq!(&parts[0][60..62], &[0xff, 0xff]);
        assert_eq!(
            Layout::parse(&parts[0])
                .unwrap()
                .transport_checksum(&parts[0]),
            0
        );
    }

    #[test]
    fn uso_never_changes_datagram_boundaries_to_fit_smaller_mtu() {
        let (mut header, frame) = packet(true, false, 2000);
        header.gso_size = 1000;
        assert!(header.segments(&frame, 600).is_err());
        let parts = header.segments(&frame, 1500).unwrap();
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|part| part.len() == 1042));
    }

    #[test]
    fn segmentation_rejects_header_amplification_before_allocating_output() {
        let (mut header, mut frame) = packet(false, true, 60000);
        frame[20] = 0;
        frame.splice(
            54..54,
            [6, 255].into_iter().chain(std::iter::repeat_n(0, 2046)),
        );
        header.csum_start += 2048;
        header.gso_size = 1;
        assert!(header.validate(&frame).is_ok());
        assert!(header.segments(&frame, 65520).is_err());
    }
}
