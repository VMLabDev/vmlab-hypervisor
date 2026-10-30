//! Synchronous 2D virtio-gpu, VIRTIO 1.2 section 5.7.
//!
//! `Gpu` clones share state. Keep a clone for frame and cursor publication
//! while the PCI transport owns another clone. No host GPU is required.

mod snapshot;
#[cfg(test)]
mod tests;

use crate::display::{Frame, Mode, PixelFormat};
use crate::{DeviceError, GuestMemory, Snapshot, VirtioDevice, VirtioInterrupt};
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    sync::Arc,
};
use virtio_queue::{DescriptorChain, Queue, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemory as _, Permissions};

const FEATURES: u64 = (1 << 32) | (1 << 29) | (1 << 28) | (1 << 1);
const MAX_BYTES: usize = 256 * 1024 * 1024;
const MAX_RESOURCES: usize = 4096;
const MAX_ENTRIES: usize = 16384;
const INVALID: u32 = 0x1205;
const RESOURCE: u32 = 0x1203;
const SCANOUT: u32 = 0x1202;
const NODATA: u32 = 0x1100;

/// Cursor pixels are little-endian premultiplied ARGB8888 (B, G, R, A).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub pixels: Vec<u8>,
    pub x: u32,
    pub y: u32,
    pub visible: bool,
    /// Changes on shape, position, visibility, reset and restore.
    pub generation: u64,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl Rect {
    fn fits(self, width: u32, height: u32) -> bool {
        self.w > 0
            && self.h > 0
            && self.x.checked_add(self.w).is_some_and(|v| v <= width)
            && self.y.checked_add(self.h).is_some_and(|v| v <= height)
    }
    fn overlaps(self, other: Self) -> bool {
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }
}

struct Resource {
    width: u32,
    height: u32,
    format: u32,
    pixels: Vec<u8>,
    backing: Vec<(u64, u32)>,
}

impl Resource {
    fn pixel(&self, offset: usize) -> [u8; 4] {
        let p = &self.pixels[offset..offset + 4];
        match self.format {
            1 => [p[0], p[1], p[2], p[3]],
            2 => [p[0], p[1], p[2], 255],
            3 => [p[3], p[2], p[1], p[0]],
            4 => [p[3], p[2], p[1], 255],
            67 => [p[2], p[1], p[0], p[3]],
            68 => [p[1], p[2], p[3], 255],
            121 => [p[1], p[2], p[3], p[0]],
            134 => [p[2], p[1], p[0], 255],
            _ => unreachable!("validated resource format"),
        }
    }
}

struct State {
    acked: u64,
    preferred: (u32, u32),
    events: u32,
    resources: BTreeMap<u32, Resource>,
    scanout: Option<(u32, Rect)>,
    native: bool,
    scanout_generation: u64,
    dirty: bool,
    cursor: Cursor,
    mem: Option<GuestMemory>,
    interrupt: Option<Arc<dyn VirtioInterrupt>>,
    queues: Vec<Queue>,
    pending: Vec<Queue>,
}

/// One output, two queues, no virgl, blobs or capability sets.
#[derive(Clone)]
pub struct Gpu {
    id: String,
    state: Arc<Mutex<State>>,
}

impl Gpu {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            state: Arc::new(Mutex::new(State {
                acked: 0,
                preferred: (1024, 768),
                events: 0,
                resources: BTreeMap::new(),
                scanout: None,
                native: false,
                scanout_generation: 0,
                dirty: true,
                cursor: Cursor::default(),
                mem: None,
                interrupt: None,
                queues: Vec::new(),
                pending: Vec::new(),
            })),
        }
    }

    pub fn native(&self) -> bool {
        self.state.lock().native
    }

    pub fn cursor(&self) -> Cursor {
        self.state.lock().cursor.clone()
    }

    /// Set the preferred EDID/display-info size and signal DISPLAY configuration change.
    pub fn request_size(&self, width: u32, height: u32) -> Result<(), DeviceError> {
        if !valid_size(width, height) {
            return Err(DeviceError::Other("invalid GPU preferred size".into()));
        }
        let mut s = self.state.lock();
        if s.preferred != (width, height) {
            s.preferred = (width, height);
            s.events |= 1;
            if let Some(irq) = &s.interrupt {
                irq.notify_config();
            }
        }
        Ok(())
    }

    /// `None` means VGA compatibility is active. Disabled native scanouts are black.
    pub fn mode(&self) -> Option<Mode> {
        self.state.lock().mode()
    }

    pub fn frame(&self) -> Option<Frame> {
        let mut s = self.state.lock();
        let mode = s.mode()?;
        let mut pixels = vec![0; mode.stride as usize * mode.height as usize];
        if let Some((id, r)) = s.scanout {
            let resource = &s.resources[&id];
            for y in 0..r.h {
                for x in 0..r.w {
                    let src = ((y + r.y) * resource.width + x + r.x) as usize * 4;
                    let dst = (y * r.w + x) as usize * 4;
                    let p = resource.pixel(src);
                    pixels[dst..dst + 3].copy_from_slice(&p[..3]);
                }
            }
        }
        Some(Frame {
            mode,
            pixels,
            dirty: std::mem::replace(&mut s.dirty, false),
        })
    }

    /// Runtime surface generation, including same-size replacement and reset.
    pub fn scanout_generation(&self) -> u64 {
        self.state.lock().scanout_generation
    }
}

fn valid_size(w: u32, h: u32) -> bool {
    w > 0 && h > 0 && w <= 3840 && h <= 2160
}
fn valid_format(f: u32) -> bool {
    matches!(f, 1..=4 | 67 | 68 | 121 | 134)
}

impl State {
    fn mode(&self) -> Option<Mode> {
        if !self.native {
            return None;
        }
        let (width, height) = self
            .scanout
            .map(|(_, r)| (r.w, r.h))
            .unwrap_or(self.preferred);
        Some(Mode {
            width,
            height,
            stride: width * 4,
            format: PixelFormat::Xrgb8888,
        })
    }

    fn command(
        &mut self,
        mem: &GuestMemory,
        q: usize,
        request: &[u8],
    ) -> Result<(u32, Vec<u8>), u32> {
        let mut d = Decoder::new(request);
        let kind = d.u32()?;
        d.take(20)?;
        if q == 1 {
            if !matches!(kind, 0x300 | 0x301) {
                return Err(INVALID);
            }
            return self
                .cursor_command(kind, &mut d)
                .map(|()| (NODATA, Vec::new()));
        }
        let mut out = Vec::new();
        let response = match kind {
            0x100 => {
                out.resize(16 * 24, 0);
                out[8..12].copy_from_slice(&self.preferred.0.to_le_bytes());
                out[12..16].copy_from_slice(&self.preferred.1.to_le_bytes());
                out[16..20].copy_from_slice(&1u32.to_le_bytes());
                0x1101
            }
            0x10a => {
                if self.acked & 2 == 0 {
                    return Err(INVALID);
                }
                if d.u32()? != 0 {
                    return Err(SCANOUT);
                }
                d.u32()?;
                out.resize(1032, 0);
                out[..4].copy_from_slice(&128u32.to_le_bytes());
                out[8..136].copy_from_slice(&edid(self.preferred.0, self.preferred.1));
                0x1104
            }
            0x101 => {
                let id = d.u32()?;
                let format = d.u32()?;
                let width = d.u32()?;
                let height = d.u32()?;
                if id == 0
                    || self.resources.contains_key(&id)
                    || !valid_format(format)
                    || !valid_size(width, height)
                {
                    return Err(INVALID);
                }
                let len = width as usize * height as usize * 4;
                if self.resources.len() >= MAX_RESOURCES
                    || self
                        .resources
                        .values()
                        .map(|r| r.pixels.len())
                        .sum::<usize>()
                        + len
                        > MAX_BYTES
                {
                    return Err(0x1201);
                }
                self.resources.insert(
                    id,
                    Resource {
                        width,
                        height,
                        format,
                        pixels: vec![0; len],
                        backing: Vec::new(),
                    },
                );
                NODATA
            }
            0x102 => {
                let id = d.u32()?;
                d.u32()?;
                self.resources.remove(&id).ok_or(RESOURCE)?;
                if self.scanout.is_some_and(|(i, _)| i == id) {
                    self.scanout = None;
                    self.scanout_generation = self.scanout_generation.wrapping_add(1);
                    self.dirty = true;
                }
                NODATA
            }
            0x103 => {
                let previous = (self.native, self.scanout);
                let rect = d.rect()?;
                if d.u32()? != 0 {
                    return Err(SCANOUT);
                }
                let id = d.u32()?;
                if id == 0 {
                    self.scanout = None;
                } else {
                    let r = self.resources.get(&id).ok_or(RESOURCE)?;
                    if !rect.fits(r.width, r.height) {
                        return Err(INVALID);
                    }
                    self.scanout = Some((id, rect));
                }
                self.native = true;
                if previous != (self.native, self.scanout) {
                    self.scanout_generation = self.scanout_generation.wrapping_add(1);
                }
                self.dirty = true;
                NODATA
            }
            0x104 => {
                let rect = d.rect()?;
                let id = d.u32()?;
                d.u32()?;
                let r = self.resources.get(&id).ok_or(RESOURCE)?;
                if !rect.fits(r.width, r.height) {
                    return Err(INVALID);
                }
                if self
                    .scanout
                    .is_some_and(|(i, s)| i == id && rect.overlaps(s))
                {
                    self.dirty = true;
                }
                NODATA
            }
            0x105 => {
                let rect = d.rect()?;
                let offset = d.u64()?;
                let id = d.u32()?;
                d.u32()?;
                let r = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                if !rect.fits(r.width, r.height) || r.backing.is_empty() {
                    return Err(INVALID);
                }
                let stride = r.width as usize * 4;
                // Read the entire rectangle before changing the resource on a failed DMA.
                let mut pixels = vec![0; rect.w as usize * rect.h as usize * 4];
                for row in 0..rect.h as usize {
                    let source = offset.checked_add((row * stride) as u64).ok_or(INVALID)?;
                    read_backing(
                        mem,
                        &r.backing,
                        source,
                        &mut pixels[row * rect.w as usize * 4..(row + 1) * rect.w as usize * 4],
                    )?;
                }
                for row in 0..rect.h as usize {
                    let dest = (rect.y as usize + row) * stride + rect.x as usize * 4;
                    let src = row * rect.w as usize * 4;
                    r.pixels[dest..dest + rect.w as usize * 4]
                        .copy_from_slice(&pixels[src..src + rect.w as usize * 4]);
                }
                NODATA
            }
            0x106 => {
                let id = d.u32()?;
                let n = d.u32()? as usize;
                if n == 0 || n > MAX_ENTRIES {
                    return Err(INVALID);
                }
                let r = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                if !r.backing.is_empty() {
                    return Err(INVALID);
                }
                let mut backing = Vec::with_capacity(n);
                for _ in 0..n {
                    let addr = d.u64()?;
                    let len = d.u32()?;
                    d.u32()?;
                    if len == 0
                        || !mem.check_range(GuestAddress(addr), len as usize, Permissions::Read)
                    {
                        return Err(INVALID);
                    }
                    backing.push((addr, len));
                }
                r.backing = backing;
                NODATA
            }
            0x107 => {
                let id = d.u32()?;
                d.u32()?;
                self.resources.get_mut(&id).ok_or(RESOURCE)?.backing.clear();
                NODATA
            }
            _ => return Err(INVALID),
        };
        Ok((response, out))
    }

    fn cursor_command(&mut self, kind: u32, d: &mut Decoder<'_>) -> Result<(), u32> {
        if d.u32()? != 0 {
            return Err(SCANOUT);
        }
        let x = d.u32()?;
        let y = d.u32()?;
        d.u32()?;
        let id = d.u32()?;
        let hot_x = d.u32()?;
        let hot_y = d.u32()?;
        d.u32()?;
        if kind == 0x300 {
            if id == 0 {
                self.cursor.visible = false;
            } else {
                let r = self.resources.get(&id).ok_or(RESOURCE)?;
                if r.width != 64 || r.height != 64 || hot_x >= 64 || hot_y >= 64 {
                    return Err(INVALID);
                }
                let mut pixels = Vec::with_capacity(64 * 64 * 4);
                for off in (0..r.pixels.len()).step_by(4) {
                    let mut p = r.pixel(off);
                    for c in 0..3 {
                        p[c] = (u16::from(p[c]) * u16::from(p[3]) / 255) as u8;
                    }
                    pixels.extend_from_slice(&p);
                }
                self.cursor.width = 64;
                self.cursor.height = 64;
                self.cursor.hot_x = hot_x;
                self.cursor.hot_y = hot_y;
                self.cursor.pixels = pixels;
                self.cursor.visible = true;
            }
        }
        self.cursor.x = x;
        self.cursor.y = y;
        self.cursor.generation = self.cursor.generation.wrapping_add(1);
        Ok(())
    }

    fn chain(&mut self, mem: &GuestMemory, q: usize, chain: DescriptorChain<&GuestMemory>) -> u32 {
        let Ok(mut reader) = chain.clone().reader(mem) else {
            return 0;
        };
        let len = reader.available_bytes();
        if !(24..=32 + MAX_ENTRIES * 16).contains(&len) {
            return 0;
        }
        let mut request = vec![0; len];
        if reader.read_exact(&mut request).is_err() {
            return 0;
        }
        let Ok(mut writer) = chain.writer(mem) else {
            return 0;
        };
        // Cursor drivers commonly provide no response buffer.
        if q == 0 && writer.available_bytes() < 24 {
            return 0;
        }
        let (kind, payload) = match self.command(mem, q, &request) {
            Ok(response) => response,
            Err(error) => (error, Vec::new()),
        };
        let mut response = vec![0; 24];
        response[..4].copy_from_slice(&kind.to_le_bytes());
        if request[4] & 1 != 0 {
            response[4] = 1;
            response[8..20].copy_from_slice(&request[8..20]);
        }
        response.extend_from_slice(&payload);
        if writer.available_bytes() < response.len() {
            return 0;
        }
        if writer.write_all(&response).is_err() {
            return 0;
        }
        response.len() as u32
    }
}

fn read_backing(
    mem: &GuestMemory,
    backing: &[(u64, u32)],
    mut offset: u64,
    mut dst: &mut [u8],
) -> Result<(), u32> {
    for &(addr, len) in backing {
        if offset >= u64::from(len) {
            offset -= u64::from(len);
            continue;
        }
        let n = dst.len().min((u64::from(len) - offset) as usize);
        mem.read_slice(
            &mut dst[..n],
            GuestAddress(addr.checked_add(offset).ok_or(INVALID)?),
        )
        .map_err(|_| INVALID)?;
        dst = &mut dst[n..];
        if dst.is_empty() {
            return Ok(());
        }
        offset = 0;
    }
    Err(INVALID)
}

impl VirtioDevice for Gpu {
    fn device_type(&self) -> u32 {
        16
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &[256, 64]
    }
    fn features(&self) -> u64 {
        FEATURES
    }
    fn ack_features(&mut self, features: u64) {
        self.state.lock().acked = features & FEATURES;
    }
    fn config_len(&self) -> u64 {
        16
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let s = self.state.lock();
        let mut config = [0; 16];
        config[..4].copy_from_slice(&s.events.to_le_bytes());
        config[8..12].copy_from_slice(&1u32.to_le_bytes());
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = offset
                .checked_add(i as u64)
                .and_then(|v| config.get(v as usize))
                .copied()
                .unwrap_or(0);
        }
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let mut clear = [0; 4];
        for (i, byte) in data.iter().enumerate() {
            if let Some(pos) = offset.checked_add(i as u64).filter(|v| (4..8).contains(v)) {
                clear[pos as usize - 4] = *byte;
            }
        }
        self.state.lock().events &= !u32::from_le_bytes(clear);
    }
    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Arc<dyn VirtioInterrupt>,
        mut queues: Vec<Queue>,
    ) -> Result<(), DeviceError> {
        let mut s = self.state.lock();
        if queues.len() != 2 || queues.iter().any(|q| q.ready() && !q.is_valid(&mem)) {
            return Err(DeviceError::Other("invalid GPU queues".into()));
        }
        if !s.pending.is_empty() {
            queues = std::mem::take(&mut s.pending);
        }
        if queues.iter().any(|q| q.ready() && !q.is_valid(&mem))
            || s.resources.values().any(|r| {
                r.backing
                    .iter()
                    .any(|&(a, n)| !mem.check_range(GuestAddress(a), n as usize, Permissions::Read))
            })
        {
            return Err(DeviceError::Other(
                "invalid restored GPU memory ranges".into(),
            ));
        }
        s.mem = Some(mem);
        s.interrupt = Some(interrupt);
        s.queues = queues;
        Ok(())
    }
    fn queue_notify(&mut self, index: u16) {
        let mut s = self.state.lock();
        let index = index as usize;
        let Some(mem) = s.mem.clone() else {
            return;
        };
        if index >= s.queues.len() || !s.queues[index].ready() {
            return;
        }
        let mut used = false;
        loop {
            while let Some(chain) = s.queues[index].pop_descriptor_chain(&mem) {
                let head = chain.head_index();
                let len = s.chain(&mem, index, chain);
                if s.queues[index].add_used(&mem, head, len).is_err() {
                    return;
                }
                used = true;
            }
            if !s.queues[index].enable_notification(&mem).unwrap_or(false) {
                break;
            }
        }
        if used
            && s.queues[index].needs_notification(&mem).unwrap_or(false)
            && let Some(irq) = &s.interrupt
        {
            irq.notify_queue(index as u16);
        }
    }
    fn reset(&mut self) {
        let mut s = self.state.lock();
        s.acked = 0;
        s.events = 0;
        s.resources.clear();
        s.scanout = None;
        s.native = false;
        s.scanout_generation = s.scanout_generation.wrapping_add(1);
        s.dirty = true;
        let generation = s.cursor.generation.wrapping_add(1);
        s.cursor = Cursor {
            generation,
            ..Cursor::default()
        };
        s.mem = None;
        s.interrupt = None;
        s.queues.clear();
        s.pending.clear();
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
}
impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], u32> {
        if n > self.bytes.len() {
            return Err(INVALID);
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }
    fn u32(&mut self) -> Result<u32, u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, u32> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn rect(&mut self) -> Result<Rect, u32> {
        Ok(Rect {
            x: self.u32()?,
            y: self.u32()?,
            w: self.u32()?,
            h: self.u32()?,
        })
    }
}

/// EDID 1.4 with a preferred 60 Hz detailed timing and no extensions.
fn edid(w: u32, h: u32) -> [u8; 128] {
    let mut e = [0; 128];
    e[..8].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 0]);
    e[8..18].copy_from_slice(&[0x59, 0xac, 1, 0, 0, 0, 0, 0, 1, 36]);
    e[18..25].copy_from_slice(&[1, 4, 0x80, 0, 0, 120, 0x06]);
    // sRGB chromaticities, in EDID's 10-bit fixed-point representation.
    let chroma = [655u16, 338, 307, 614, 154, 61, 320, 337];
    for (i, value) in chroma.iter().enumerate() {
        e[25 + i / 4] |= ((value & 3) as u8) << (6 - (i % 4) * 2);
        e[27 + i] = (value >> 2) as u8;
    }
    e[38..54].fill(1);
    let hb = 160u32;
    let vb = 35u32;
    let clock = ((w + hb) * (h + vb) * 60 / 10000) as u16;
    e[54..56].copy_from_slice(&clock.to_le_bytes());
    e[56] = w as u8;
    e[57] = hb as u8;
    e[58] = ((w >> 8) << 4) as u8;
    e[59] = h as u8;
    e[60] = vb as u8;
    e[61] = ((h >> 8) << 4) as u8;
    e[62..66].copy_from_slice(&[48, 32, 0x35, 0]);
    e[71] = 0x1a;
    e[72..90].copy_from_slice(b"\0\0\0\xfc\0vmlab screen\n");
    e[90..108].copy_from_slice(b"\0\0\0\xff\0vmlab-hv\n    ");
    e[108..113].copy_from_slice(&[0, 0, 0, 0x10, 0]);
    e[127] = 0u8.wrapping_sub(e[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    e
}
