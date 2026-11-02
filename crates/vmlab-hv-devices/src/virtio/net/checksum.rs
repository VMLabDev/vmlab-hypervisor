//! Internet checksums fused with copies between host and validated guest spans.

use std::ops::Range;

fn add(sum: u64, word: u64) -> u64 {
    let (sum, carry) = sum.overflowing_add(word);
    sum + u64::from(carry)
}

#[derive(Default)]
pub(super) struct Checksum {
    sum: u64,
    odd: Option<u8>,
}

impl Checksum {
    pub(super) fn update(&mut self, bytes: &[u8]) {
        // SAFETY: bytes is readable for its length; a null destination requests
        // checksum calculation without a copy.
        unsafe { self.copy(bytes.as_ptr(), std::ptr::null_mut(), bytes.len()) };
    }

    // SAFETY: source must be readable for len bytes. A non-null destination must
    // be writable for len bytes and must not overlap source. Guest mappings
    // must stay guarded for the duration of this call.
    unsafe fn copy(&mut self, source: *const u8, destination: *mut u8, len: usize) {
        let mut index = 0;
        // SAFETY: the caller retains both mappings for len bytes. Every access
        // below is bounded by len and uses unaligned loads and stores.
        unsafe {
            if let Some(previous) = self.odd.take() {
                if len == 0 {
                    self.odd = Some(previous);
                    return;
                }
                let byte = source.read();
                if !destination.is_null() {
                    destination.write(byte);
                }
                self.sum = add(self.sum, u64::from(u16::from_ne_bytes([previous, byte])));
                index = 1;
            }
            let mut sums = [self.sum, 0, 0, 0];
            while len - index >= 32 {
                for (lane, sum) in sums.iter_mut().enumerate() {
                    let offset = index + lane * 8;
                    let word = source.add(offset).cast::<u64>().read_unaligned();
                    if !destination.is_null() {
                        destination.add(offset).cast::<u64>().write_unaligned(word);
                    }
                    *sum = add(*sum, word);
                }
                index += 32;
            }
            self.sum = sums.into_iter().fold(0, add);
            while len - index >= 8 {
                let word = source.add(index).cast::<u64>().read_unaligned();
                if !destination.is_null() {
                    destination.add(index).cast::<u64>().write_unaligned(word);
                }
                self.sum = add(self.sum, word);
                index += 8;
            }
            while len - index >= 2 {
                let word = source.add(index).cast::<u16>().read_unaligned();
                if !destination.is_null() {
                    destination.add(index).cast::<u16>().write_unaligned(word);
                }
                self.sum = add(self.sum, u64::from(word));
                index += 2;
            }
            if index < len {
                let byte = source.add(index).read();
                if !destination.is_null() {
                    destination.add(index).write(byte);
                }
                self.odd = Some(byte);
            }
        }
    }

    pub(super) fn folded(&self) -> u16 {
        let mut sum = self.sum;
        if let Some(byte) = self.odd {
            sum = add(sum, u64::from(u16::from_ne_bytes([byte, 0])));
        }
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        (sum as u16).to_be()
    }
}

pub(super) struct CopyChecksum {
    range: Range<usize>,
    position: usize,
    checksum: Checksum,
}

impl CopyChecksum {
    pub(super) fn transmit(start: usize, offset: usize, len: usize) -> Option<Self> {
        let field = start.checked_add(offset)?;
        if start > len || field.checked_add(2)? > len {
            return None;
        }
        Some(Self {
            range: start..len,
            position: 0,
            checksum: Checksum::default(),
        })
    }

    pub(super) fn receive(frame: &[u8]) -> Option<Self> {
        Self::receive_header(frame, frame.len())
    }

    pub(super) fn receive_header(frame: &[u8], frame_len: usize) -> Option<Self> {
        let mut ip = 14usize;
        let mut kind = word(frame, 12)?;
        for _ in 0..2 {
            if !matches!(kind, 0x8100 | 0x88a8) {
                break;
            }
            kind = word(frame, ip + 2)?;
            ip += 4;
        }
        let (start, end, protocol, mut checksum, ipv4) = match kind {
            0x0800 => {
                let first = *frame.get(ip)?;
                let header = usize::from(first & 15) * 4;
                let len = usize::from(word(frame, ip + 2)?);
                if first >> 4 != 4
                    || header < 20
                    || len < header
                    || word(frame, ip + 6)? & 0x3fff != 0
                {
                    return None;
                }
                let end = ip.checked_add(len)?;
                if end > frame_len {
                    return None;
                }
                let mut header_sum = Checksum::default();
                header_sum.update(frame.get(ip..ip + header)?);
                if header_sum.folded() != 0xffff {
                    return None;
                }
                let protocol = *frame.get(ip + 9)?;
                let mut checksum = Checksum::default();
                if matches!(protocol, 6 | 17) {
                    checksum.update(frame.get(ip + 12..ip + 20)?);
                    checksum.update(&[0, protocol]);
                    checksum.update(&((len - header) as u16).to_be_bytes());
                }
                (ip + header, end, protocol, checksum, true)
            }
            0x86dd => {
                if frame.get(ip)? >> 4 != 6 {
                    return None;
                }
                let len = usize::from(word(frame, ip + 4)?);
                if len == 0 {
                    return None;
                }
                let end = ip.checked_add(40 + len)?;
                if end > frame_len {
                    return None;
                }
                let mut protocol = *frame.get(ip + 6)?;
                let mut start = ip + 40;
                for _ in 0..8 {
                    let length = match protocol {
                        0 | 60 => (usize::from(*frame.get(start + 1)?) + 1) * 8,
                        51 => (usize::from(*frame.get(start + 1)?) + 2) * 4,
                        44 => {
                            if word(frame, start + 2)? != 0 {
                                return None;
                            }
                            8
                        }
                        _ => break,
                    };
                    protocol = *frame.get(start)?;
                    start = start.checked_add(length)?;
                    if start > end {
                        return None;
                    }
                }
                let mut checksum = Checksum::default();
                checksum.update(frame.get(ip + 8..ip + 40)?);
                checksum.update(&((end - start) as u32).to_be_bytes());
                checksum.update(&[0, 0, 0, protocol]);
                (start, end, protocol, checksum, false)
            }
            _ => return None,
        };
        let bytes = frame.get(start..end.min(frame.len()))?;
        let transport_len = end - start;
        match protocol {
            6 => {
                let header = usize::from(*bytes.get(12)? >> 4) * 4;
                if bytes.len() < 20 || header < 20 || header > transport_len {
                    return None;
                }
            }
            17 => {
                if bytes.len() < 8
                    || usize::from(word(bytes, 4)?) != transport_len
                    || word(bytes, 6)? == 0
                {
                    return None;
                }
            }
            1 if ipv4 => {
                if bytes.len() < 4 {
                    return None;
                }
                checksum = Checksum::default();
            }
            58 if !ipv4 => {
                if bytes.len() < 4 {
                    return None;
                }
            }
            _ => return None,
        }
        Some(Self {
            range: start..end,
            position: 0,
            checksum,
        })
    }

    /// Copy one consecutive packet span while summing its transport bytes.
    ///
    /// # Safety
    /// Both pointers must remain valid for len non-overlapping bytes. A guest
    /// pointer must retain its mapping guard throughout this call.
    pub(super) unsafe fn copy(&mut self, source: *const u8, destination: *mut u8, len: usize) {
        let start = self.range.start.saturating_sub(self.position).min(len);
        let end = self
            .range
            .end
            .saturating_sub(self.position)
            .min(len)
            .max(start);
        // SAFETY: all subranges are within the caller's validated len-byte spans.
        unsafe {
            std::ptr::copy_nonoverlapping(source, destination, start);
            self.checksum
                .copy(source.add(start), destination.add(start), end - start);
            std::ptr::copy_nonoverlapping(source.add(end), destination.add(end), len - end);
        }
        self.position += len;
    }

    /// Sum a span filled by direct RX before publishing its used descriptor.
    ///
    /// # Safety
    /// source must stay guarded and readable for len bytes.
    pub(super) unsafe fn update_guest(&mut self, source: *const u8, len: usize) {
        let start = self.range.start.saturating_sub(self.position).min(len);
        let end = self
            .range
            .end
            .saturating_sub(self.position)
            .min(len)
            .max(start);
        // SAFETY: start and end are bounded by the caller's readable span.
        unsafe {
            self.checksum
                .copy(source.add(start), std::ptr::null_mut(), end - start)
        };
        self.position += len;
    }

    pub(super) fn valid(&self) -> bool {
        self.position >= self.range.end && self.checksum.folded() == 0xffff
    }

    pub(super) fn completed(&self) -> u16 {
        let value = !self.checksum.folded();
        if value == 0 { 0xffff } else { value }
    }
}

fn word(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(bytes: &[u8]) -> u16 {
        let mut sum = 0u32;
        for pair in bytes.chunks(2) {
            sum += u32::from(u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]));
        }
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        sum as u16
    }

    #[test]
    fn fused_copies_match_the_reference_at_every_short_split() {
        let bytes: Vec<_> = (0..65550).map(|n| ((n * 97 + 13) % 251) as u8).collect();
        for len in [0, 1, 2, 3, 7, 8, 9, 17, 128, 65549, 65550] {
            for chunk in 1..=33 {
                let mut checksum = Checksum::default();
                let mut copied = vec![0; len];
                for (source, target) in bytes[..len].chunks(chunk).zip(copied.chunks_mut(chunk)) {
                    // SAFETY: source and target are disjoint and have equal lengths.
                    unsafe { checksum.copy(source.as_ptr(), target.as_mut_ptr(), source.len()) };
                }
                assert_eq!(
                    checksum.folded(),
                    reference(&bytes[..len]),
                    "len={len}, chunk={chunk}"
                );
                assert_eq!(copied, bytes[..len]);
            }
        }
    }

    #[test]
    fn copy_range_excludes_headers_and_padding_across_odd_splits() {
        let bytes: Vec<_> = (0..157).map(|n| n as u8).collect();
        for chunk in 1..=33 {
            let mut copy = CopyChecksum {
                range: 13..142,
                position: 0,
                checksum: Checksum::default(),
            };
            let mut target = vec![0; bytes.len()];
            for (source, destination) in bytes.chunks(chunk).zip(target.chunks_mut(chunk)) {
                // SAFETY: source and destination are disjoint equal-length buffers.
                unsafe { copy.copy(source.as_ptr(), destination.as_mut_ptr(), source.len()) };
            }
            assert_eq!(copy.checksum.folded(), reference(&bytes[13..142]));
            assert_eq!(target, bytes);
        }
        assert!(CopyChecksum::transmit(2, usize::MAX, 100).is_none());
        assert!(CopyChecksum::transmit(99, 0, 100).is_none());
        assert!(CopyChecksum::transmit(2, 97, 100).is_none());
    }

    fn packet(ipv6: bool, vlan: bool, protocol: u8, payload: usize) -> (Vec<u8>, usize, usize) {
        let ethernet = if vlan { 18 } else { 14 };
        let network = if ipv6 { 40 } else { 20 };
        let transport = if protocol == 6 { 20 } else { 8 };
        let mut frame = vec![0; ethernet + network + transport + payload];
        let kind = if ipv6 { 0x86ddu16 } else { 0x0800u16 };
        if vlan {
            frame[12..14].copy_from_slice(&0x8100u16.to_be_bytes());
            frame[16..18].copy_from_slice(&kind.to_be_bytes());
        } else {
            frame[12..14].copy_from_slice(&kind.to_be_bytes());
        }
        let mut pseudo = Vec::new();
        let l4 = ethernet + network;
        let len = transport + payload;
        if ipv6 {
            frame[ethernet] = 0x60;
            frame[ethernet + 4..ethernet + 6].copy_from_slice(&(len as u16).to_be_bytes());
            frame[ethernet + 6] = protocol;
            for (n, byte) in frame[ethernet + 8..ethernet + 40].iter_mut().enumerate() {
                *byte = n as u8;
            }
            pseudo.extend_from_slice(&frame[ethernet + 8..ethernet + 40]);
            pseudo.extend_from_slice(&(len as u32).to_be_bytes());
            pseudo.extend_from_slice(&[0, 0, 0, protocol]);
        } else {
            frame[ethernet] = 0x45;
            frame[ethernet + 2..ethernet + 4].copy_from_slice(&((20 + len) as u16).to_be_bytes());
            frame[ethernet + 8] = 64;
            frame[ethernet + 9] = protocol;
            frame[ethernet + 12..ethernet + 20].copy_from_slice(&[192, 0, 2, 1, 198, 51, 100, 2]);
            pseudo.extend_from_slice(&frame[ethernet + 12..ethernet + 20]);
            pseudo.extend_from_slice(&[0, protocol]);
            pseudo.extend_from_slice(&(len as u16).to_be_bytes());
            let ip_sum = !reference(&frame[ethernet..ethernet + 20]);
            frame[ethernet + 10..ethernet + 12].copy_from_slice(&ip_sum.to_be_bytes());
        }
        let offset = if protocol == 6 {
            frame[l4 + 12] = 0x50;
            16
        } else {
            frame[l4 + 4..l4 + 6].copy_from_slice(&(len as u16).to_be_bytes());
            6
        };
        for (index, byte) in frame[l4 + transport..].iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        frame[l4 + offset..l4 + offset + 2].copy_from_slice(&reference(&pseudo).to_be_bytes());
        (frame, l4, offset)
    }

    fn complete(frame: &[u8], start: usize, offset: usize, chunk: usize) -> Vec<u8> {
        let mut target = vec![0; frame.len()];
        let mut checksum = CopyChecksum::transmit(start, offset, frame.len()).unwrap();
        for (source, destination) in frame.chunks(chunk).zip(target.chunks_mut(chunk)) {
            // SAFETY: source and destination have equal lengths and do not overlap.
            unsafe { checksum.copy(source.as_ptr(), destination.as_mut_ptr(), source.len()) };
        }
        target[start + offset..start + offset + 2]
            .copy_from_slice(&checksum.completed().to_be_bytes());
        target
    }

    fn verify(frame: &[u8], chunk: usize) -> bool {
        let Some(mut checksum) = CopyChecksum::receive(frame) else {
            return false;
        };
        let mut target = vec![0; frame.len()];
        for (source, destination) in frame.chunks(chunk).zip(target.chunks_mut(chunk)) {
            // SAFETY: source and destination have equal lengths and do not overlap.
            unsafe { checksum.copy(source.as_ptr(), destination.as_mut_ptr(), source.len()) };
        }
        assert_eq!(target, frame);
        checksum.valid()
    }

    #[test]
    fn completes_and_validates_ipv4_ipv6_tcp_udp_and_vlan_packets() {
        for ipv6 in [false, true] {
            for vlan in [false, true] {
                for protocol in [6, 17] {
                    for payload in [0, 1, 57, 128, 65000] {
                        let (frame, start, offset) = packet(ipv6, vlan, protocol, payload);
                        for chunk in [1, 3, 8, 17, 4096, 65550] {
                            let target = complete(&frame, start, offset, chunk);
                            assert!(
                                verify(&target, chunk),
                                "ipv6={ipv6}, vlan={vlan}, protocol={protocol}, payload={payload}, chunk={chunk}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_checksums_fragments_and_udp_opt_out_are_not_marked_valid() {
        let (frame, start, offset) = packet(false, false, 6, 57);
        let good = complete(&frame, start, offset, 17);
        let mut bad = good.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(!verify(&bad, 3));
        bad = good.clone();
        bad[24] ^= 1;
        assert!(CopyChecksum::receive(&bad).is_none());
        bad = good.clone();
        bad[20..22].copy_from_slice(&0x2000u16.to_be_bytes());
        bad[24..26].fill(0);
        let checksum = !reference(&bad[14..34]);
        bad[24..26].copy_from_slice(&checksum.to_be_bytes());
        assert!(CopyChecksum::receive(&bad).is_none());
        for len in 0..good.len() {
            assert!(CopyChecksum::receive(&good[..len]).is_none());
        }
        for ipv6 in [false, true] {
            let (frame, start, offset) = packet(ipv6, false, 17, 57);
            let mut frame = complete(&frame, start, offset, 3);
            frame[start + offset..start + offset + 2].fill(0);
            assert!(CopyChecksum::receive(&frame).is_none());
        }
    }

    #[test]
    fn ipv6_atomic_fragment_keeps_the_transport_pseudo_header() {
        let (frame, start, offset) = packet(true, false, 6, 57);
        let mut frame = complete(&frame, start, offset, 13);
        frame[20] = 44;
        let len = word(&frame, 18).unwrap() + 8;
        frame[18..20].copy_from_slice(&len.to_be_bytes());
        frame.splice(start..start, [6, 0, 0, 0, 0, 0, 0, 1]);
        assert!(verify(&frame, 7));
        frame[start + 3] = 1;
        assert!(CopyChecksum::receive(&frame).is_none());
    }

    #[test]
    fn direct_rx_validates_payload_using_a_bounded_header_copy() {
        for ipv6 in [false, true] {
            let (frame, start, offset) = packet(ipv6, true, 6, 65000);
            let frame = complete(&frame, start, offset, 13);
            let mut checksum = CopyChecksum::receive_header(&frame[..128], frame.len()).unwrap();
            for part in frame.chunks(1517) {
                // SAFETY: part is readable for its length throughout the call.
                unsafe { checksum.update_guest(part.as_ptr(), part.len()) };
            }
            assert!(checksum.valid());
        }
    }
}
