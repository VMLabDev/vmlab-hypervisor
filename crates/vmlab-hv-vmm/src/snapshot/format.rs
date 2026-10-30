//! The vmstate stream format: a header, keyed sections, then RAM.
//!
//! ```text
//! header:   u32 magic "VLHV" | u16 major | u16 minor |
//!           u8 run state (0 paused, 1 running, 2 suspended) |
//!           u16 len + vmlab-hv version | u16 len + machine digest |
//!           u32 section count
//! section:  u16 id len | id bytes | u64 body len | body
//! RAM:      u32 region count |
//!           per region: u64 base | u64 len, then records until covered:
//!             tag 0: u64 pages    (that many all-zero pages)
//!             tag 1: u64 pages | pages * 4096 bytes
//! ```
//!
//! All integers are little-endian. RAM is last and self-delimiting so it can
//! stream: pages are examined one at a time and all-zero pages collapse
//! into run markers, so a fresh machine costs kilobytes, not its RAM size.

/// Stream magic: `"VLHV"`.
pub const MAGIC: u32 = 0x564C_4856;

/// Device-state format version this build writes. A different major version
/// refuses to restore; minors are best effort before 1.0 compatibility
/// freezes (D32).
pub const DEVICE_STATE_MAJOR_VERSION: u16 = 1;
pub const DEVICE_STATE_MINOR_VERSION: u16 = 0;

/// RAM page granularity of the run-length coding.
pub const PAGE_SIZE: u64 = 4096;

/// Most pages one data record buffers before it is flushed (1 MiB).
pub const MAX_DATA_PAGES: u64 = 256;

/// Run states as recorded in the header.
pub const RUN_PAUSED: u8 = 0;
pub const RUN_RUNNING: u8 = 1;
pub const RUN_SUSPENDED: u8 = 2;

/// What the header says, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub major: u16,
    pub minor: u16,
    pub run_state: u8,
    pub hv_version: String,
    pub digest: [u8; 32],
    pub section_count: u32,
}

/// Why a stream cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    /// The magic or structure is not a vmlab-hv vmstate stream at all.
    #[error("not a vmlab-hv snapshot stream: {0}")]
    NotVmstate(String),
    /// The device-state major version differs; both are named.
    #[error(
        "snapshot device-state version {saved_major}.{saved_minor} is not compatible with this vmlab-hv ({DEVICE_STATE_MAJOR_VERSION}.{DEVICE_STATE_MINOR_VERSION})"
    )]
    VersionMismatch { saved_major: u16, saved_minor: u16 },
    /// The machine-description digest differs; both are named.
    #[error("machine description digest {saved} does not match this machine ({current})")]
    DigestMismatch { saved: String, current: String },
    /// The stream is structurally broken.
    #[error("corrupt vmstate stream: {0}")]
    Corrupt(String),
}

/// Hex, without a dependency for thirty-two bytes.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

// ---- encoding helpers -------------------------------------------------

pub(crate) fn push_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

pub(crate) fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    push_u16(out, bytes.len() as u16);
    out.extend_from_slice(bytes);
}

/// A checked little-endian reader over a byte slice.
#[derive(Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, pos: 0 }
    }

    pub fn take(&mut self, len: usize) -> Result<&'a [u8], FormatError> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| FormatError::Corrupt("stream ends early".into()))?;
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, FormatError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, FormatError> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes(s.try_into().expect("2 bytes")))
    }

    pub fn u32(&mut self) -> Result<u32, FormatError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes(s.try_into().expect("4 bytes")))
    }

    pub fn u64(&mut self) -> Result<u64, FormatError> {
        let s = self.take(8)?;
        Ok(u64::from_le_bytes(s.try_into().expect("8 bytes")))
    }

    pub fn done(&self) -> bool {
        self.pos == self.bytes.len()
    }

    pub fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.pos..]
    }
}

/// Encode the header and every section. RAM is appended by the caller
/// through [`RamEncoder`].
pub fn encode_header_sections(
    run_state: u8,
    hv_version: &str,
    digest: &[u8; 32],
    sections: &[(String, Vec<u8>)],
) -> Vec<u8> {
    let mut out = Vec::new();
    push_u32(&mut out, MAGIC);
    push_u16(&mut out, DEVICE_STATE_MAJOR_VERSION);
    push_u16(&mut out, DEVICE_STATE_MINOR_VERSION);
    push_u8(&mut out, run_state);
    push_bytes(&mut out, hv_version.as_bytes());
    push_bytes(&mut out, digest);
    push_u32(&mut out, sections.len() as u32);
    for (id, body) in sections {
        push_bytes(&mut out, id.as_bytes());
        push_u64(&mut out, body.len() as u64);
        out.extend_from_slice(body);
    }
    out
}

/// Decode and validate the header against this build and this machine.
pub fn decode_header(bytes: &[u8]) -> Result<(Header, Reader<'_>), FormatError> {
    let mut r = Reader::new(bytes);
    if r.u32()? != MAGIC {
        return Err(FormatError::NotVmstate("bad magic".into()));
    }
    let major = r.u16()?;
    let minor = r.u16()?;
    if major != DEVICE_STATE_MAJOR_VERSION {
        return Err(FormatError::VersionMismatch {
            saved_major: major,
            saved_minor: minor,
        });
    }
    let run_state = r.u8()?;
    let hv_len = r.u16()? as usize;
    let hv_version = String::from_utf8(r.take(hv_len)?.to_vec())
        .map_err(|_| FormatError::Corrupt("version is not UTF-8".into()))?;
    let digest_len = r.u16()? as usize;
    let digest: [u8; 32] = r
        .take(digest_len)?
        .to_vec()
        .try_into()
        .map_err(|_| FormatError::Corrupt("machine digest is not 32 bytes".into()))?;
    let section_count = r.u32()?;
    let header = Header {
        major,
        minor,
        run_state,
        hv_version,
        digest,
        section_count,
    };
    Ok((header, r))
}

/// Read one section from the reader positioned at a section header.
pub fn read_section(reader: &mut Reader<'_>) -> Result<(String, Vec<u8>), FormatError> {
    let id_len = reader.u16()? as usize;
    let id = String::from_utf8(reader.take(id_len)?.to_vec())
        .map_err(|_| FormatError::Corrupt("section id is not UTF-8".into()))?;
    let len = reader.u64()? as usize;
    let body = reader.take(len)?.to_vec();
    Ok((id, body))
}

// ---- RAM run-length coding --------------------------------------------

/// Incremental RAM-region encoder: feed pages in order, it yields record
/// bytes to stream out. Zero runs grow without bound; data runs flush at
/// [`MAX_DATA_PAGES`] pages.
pub struct RamEncoder {
    zero_pages: u64,
    data: Vec<u8>,
}

impl RamEncoder {
    pub fn new() -> Self {
        RamEncoder {
            zero_pages: 0,
            data: Vec::new(),
        }
    }

    fn flush_zero(&mut self, out: &mut Vec<u8>) {
        if self.zero_pages > 0 {
            push_u8(out, 0);
            push_u64(out, self.zero_pages);
            self.zero_pages = 0;
        }
    }

    fn flush_data(&mut self, out: &mut Vec<u8>) {
        let pages = (self.data.len() as u64) / PAGE_SIZE;
        if pages > 0 {
            push_u8(out, 1);
            push_u64(out, pages);
            out.append(&mut self.data);
        }
        self.data.clear();
    }

    /// Feed one page; appends any completed records to `out`.
    pub fn page(&mut self, page: &[u8], out: &mut Vec<u8>) {
        if page.iter().all(|&b| b == 0) {
            self.flush_data(out);
            self.zero_pages += 1;
        } else {
            self.flush_zero(out);
            self.data.extend_from_slice(page);
            if self.data.len() as u64 >= MAX_DATA_PAGES * PAGE_SIZE {
                self.flush_data(out);
            }
        }
    }

    /// End of a region: flush what is pending.
    pub fn finish_region(&mut self, out: &mut Vec<u8>) {
        self.flush_zero(out);
        self.flush_data(out);
    }
}

impl Default for RamEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode whole regions from an iterator of `(base, len)` and a page
/// reader, for tests and in-memory uses.
pub fn encode_ram_regions<'a>(
    regions: impl Iterator<Item = (u64, u64)> + 'a,
    mut read_page: impl FnMut(u64, &mut [u8]) -> Result<(), String> + 'a,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let regions: Vec<(u64, u64)> = regions.collect();
    push_u32(&mut out, regions.len() as u32);
    let mut enc = RamEncoder::new();
    let mut page = vec![0u8; PAGE_SIZE as usize];
    for (base, len) in regions {
        push_u64(&mut out, base);
        push_u64(&mut out, len);
        if len % PAGE_SIZE != 0 {
            return Err(format!("RAM region {base:#x}+{len:#x} is not page aligned"));
        }
        let mut off = 0;
        while off < len {
            read_page(base + off, &mut page).map_err(|e| format!("page {:#x}: {e}", base + off))?;
            enc.page(&page, &mut out);
            off += PAGE_SIZE;
        }
        enc.finish_region(&mut out);
    }
    Ok(out)
}

/// Decode the RAM region and stream pages back out through `write_pages`
/// (guest physical base of the run, then the run's bytes).
pub fn decode_ram_regions<'a>(
    reader: &mut Reader<'a>,
    mut write_pages: impl FnMut(u64, &[u8]) -> Result<(), String>,
) -> Result<Vec<(u64, u64)>, FormatError> {
    let count = reader.u32()? as usize;
    let mut regions = Vec::with_capacity(count);
    for _ in 0..count {
        let base = reader.u64()?;
        let len = reader.u64()?;
        if len % PAGE_SIZE != 0 {
            return Err(FormatError::Corrupt(format!(
                "RAM region {base:#x}+{len:#x} is not page aligned"
            )));
        }
        regions.push((base, len));
    }
    for (base, len) in &regions {
        let mut covered = 0u64;
        while covered < *len {
            match reader.u8()? {
                0 => covered += reader.u64()? * PAGE_SIZE,
                1 => {
                    let pages = reader.u64()?;
                    let bytes = pages
                        .checked_mul(PAGE_SIZE)
                        .ok_or_else(|| FormatError::Corrupt("run overflows".into()))?;
                    let run = reader.take(bytes as usize)?;
                    write_pages(base + covered, run).map_err(FormatError::Corrupt)?;
                    covered += bytes;
                }
                tag => {
                    return Err(FormatError::Corrupt(format!(
                        "unknown RAM record tag {tag}"
                    )));
                }
            }
        }
        if covered > *len {
            return Err(FormatError::Corrupt(format!(
                "RAM region {base:#x} overruns its length"
            )));
        }
    }
    Ok(regions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_sections() -> Vec<(String, Vec<u8>)> {
        vec![
            ("pic-master".to_string(), vec![1, 2, 3]),
            ("vcpu0/regs".to_string(), vec![0xA5; 40]),
        ]
    }

    fn sample_stream() -> Vec<u8> {
        let mut out = encode_header_sections(RUN_RUNNING, "0.1.0", &[7u8; 32], &sample_sections());
        out.extend_from_slice(&sample_ram());
        out
    }

    /// 4 MiB: zeros, two data pages, zeros.
    fn sample_ram() -> Vec<u8> {
        encode_ram_regions(
            [(0x1000_0000u64, 4 * 1024 * 1024)].into_iter(),
            |addr, buf| {
                buf.fill(0);
                let page = (addr - 0x1000_0000) / PAGE_SIZE;
                if page == 1 || page == 2 {
                    buf.fill((page + 1) as u8);
                }
                Ok(())
            },
        )
        .unwrap()
    }

    #[test]
    fn header_and_sections_round_trip() {
        let bytes = sample_stream();
        let (header, mut r) = decode_header(&bytes).unwrap();
        assert_eq!(header.major, DEVICE_STATE_MAJOR_VERSION);
        assert_eq!(header.minor, DEVICE_STATE_MINOR_VERSION);
        assert_eq!(header.run_state, RUN_RUNNING);
        assert_eq!(header.hv_version, "0.1.0");
        assert_eq!(header.digest, [7u8; 32]);
        assert_eq!(header.section_count, 2);
        let sections: Vec<(String, Vec<u8>)> = (0..header.section_count)
            .map(|_| read_section(&mut r).unwrap())
            .collect();
        assert_eq!(sections, sample_sections());
        // The rest is RAM and decodes cleanly.
        let mut written: Vec<(u64, Vec<u8>)> = Vec::new();
        let regions = decode_ram_regions(&mut r, |base, bytes| {
            written.push((base, bytes.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(regions, [(0x1000_0000, 4 * 1024 * 1024)]);
        // Exactly the two data pages came back, merged into one run at the
        // right offset.
        let mut expected = vec![2u8; PAGE_SIZE as usize];
        expected.extend_from_slice(&vec![3u8; PAGE_SIZE as usize]);
        assert_eq!(written, vec![(0x1000_0000 + PAGE_SIZE, expected)]);
    }

    #[test]
    fn major_version_refusal_names_both() {
        let mut bytes = sample_stream();
        // Patch the major version in place.
        let major_at = 4; // after the magic
        let bumped = DEVICE_STATE_MAJOR_VERSION + 1;
        bytes[major_at..major_at + 2].copy_from_slice(&bumped.to_le_bytes());
        let err = decode_header(&bytes).unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("{bumped}.{}", DEVICE_STATE_MINOR_VERSION)),
            "message names the saved version: {err}"
        );
        assert!(
            err.to_string().contains(&format!(
                "{}.{}",
                DEVICE_STATE_MAJOR_VERSION, DEVICE_STATE_MINOR_VERSION
            )),
            "message names this build's version: {err}"
        );
    }

    #[test]
    fn minor_version_difference_is_accepted() {
        let mut bytes = sample_stream();
        let minor_at = 6;
        bytes[minor_at..minor_at + 2]
            .copy_from_slice(&(DEVICE_STATE_MINOR_VERSION + 1).to_le_bytes());
        assert!(decode_header(&bytes).is_ok());
    }

    #[test]
    fn bad_magic_refused() {
        let mut bytes = sample_stream();
        bytes[0] ^= 0xFF;
        assert!(matches!(
            decode_header(&bytes),
            Err(FormatError::NotVmstate(_))
        ));
    }

    #[test]
    fn zero_pages_collapse_to_run_markers() {
        let ram = sample_ram();
        // 4 MiB of RAM with two data pages: two records plus headers, not
        // 4 MiB of bytes.
        assert!(ram.len() < 4 * PAGE_SIZE as usize, "ram len {}", ram.len());
    }

    #[test]
    fn truncated_stream_refused() {
        let bytes = sample_stream();
        // Cuts inside the header must fail to decode.
        let (_, reader) = decode_header(&bytes).unwrap();
        let consumed = bytes.len() - reader.remaining().len();
        for cut in 1..consumed {
            assert!(decode_header(&bytes[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn data_runs_flush_at_capacity() {
        let regions = [(0u64, MAX_DATA_PAGES * PAGE_SIZE + PAGE_SIZE)];
        let ram = encode_ram_regions(regions.into_iter(), |_, buf| {
            buf.fill(0xEE);
            Ok(())
        })
        .unwrap();
        // (MAX_DATA_PAGES + 1) data pages in two records plus the region
        // header: 4 + 16 + (1 + 8 + MAX*4096) + (1 + 8 + 4096).
        let expected = 4 + 16 + 9 + (MAX_DATA_PAGES * PAGE_SIZE) as usize + 9 + 4096;
        assert_eq!(ram.len(), expected);
        let mut r = Reader::new(&ram);
        let mut pages = 0;
        decode_ram_regions(&mut r, |_, bytes| {
            assert!(bytes.iter().all(|&b| b == 0xEE));
            pages += (bytes.len() as u64) / PAGE_SIZE;
            Ok(())
        })
        .unwrap();
        assert_eq!(pages, MAX_DATA_PAGES + 1);
    }
}
