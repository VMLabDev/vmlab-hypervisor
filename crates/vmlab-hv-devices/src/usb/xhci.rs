use std::{collections::VecDeque, sync::Arc, time::Instant};

use crate::{
    BarInfo, BarKind, BusDevice, DeviceError, GuestMemory, IrqLine, MsiMessage, MsiSink, PciDevice,
    PciSnapshot, Snapshot,
};

use super::{
    PointerEvent, SetupPacket, Tablet, UsbDevice, UsbSpeed, UsbTransfer,
    ring::{Ring, Trb, read, write},
    state::{Reader, Writer, invalid},
};

pub const BAR_SIZE: u64 = 0x4000;
/// Ports 1..4 are USB3; ports 5..8 are USB2, as on qemu-xhci.
pub const TABLET_PORT: u8 = 5;
const SLOTS: usize = 64;
const PORTS: usize = 8;
const OP: usize = 0x40;
const CMD: usize = OP;
const STS: usize = OP + 4;
const CRCR: usize = OP + 0x18;
const DCBAAP: usize = OP + 0x30;
const CONFIG: usize = OP + 0x38;
const PORT: usize = OP + 0x400;
const IMAN: usize = 0x1020;
const IMOD: usize = 0x1024;
const ERSTSZ: usize = 0x1028;
const ERSTBA: usize = 0x1030;
const ERDP: usize = 0x1038;
const MSIX: usize = 0x3000;
const PBA: usize = 0x3800;
const SUCCESS: u32 = 1;
const TRB_ERROR: u32 = 5;
const STALL: u32 = 6;
const CONTEXT_ERROR: u32 = 19;
const BUDGET: usize = 256;

#[derive(Clone, Default)]
struct Endpoint {
    context: [u32; 8],
    ring: Ring,
    kicked: bool,
}

#[derive(Clone)]
struct Slot {
    enabled: bool,
    port: usize,
    context: [u32; 8],
    endpoints: Vec<Endpoint>,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 0,
            context: [0; 8],
            endpoints: vec![Endpoint::default(); 32],
        }
    }
}

#[derive(Clone)]
struct ControllerState {
    pci: [u8; 256],
    regs: Vec<u32>,
    command: Ring,
    command_running: bool,
    slots: Vec<Slot>,
    event_segment: usize,
    event_index: u32,
    event_cycle: bool,
    event_used: usize,
    pending: VecDeque<Trb>,
    mfindex: u32,
    ehb: bool,
    interrupt_sent: bool,
    moderation_counter: u32,
}

/// qemu-xhci PCI presentation, one interrupter, 64 slots and eight root ports.
/// Call `poll()` on backend readiness and periodically while running. Calls
/// are serialized with MMIO and snapshots by the VMM's device lock.
pub struct XhciController {
    id: String,
    mem: GuestMemory,
    msi: Arc<dyn MsiSink>,
    intx: Option<Arc<dyn IrqLine>>,
    devices: Vec<Option<Box<dyn UsbDevice>>>,
    state: ControllerState,
    clock: Instant,
    moderation_clock: Instant,
}

impl XhciController {
    /// Construct a controller with one tablet on root port 5. The VMM routes
    /// PCI config and BAR0 through `PciDevice` and saves `Snapshot` state.
    pub fn new(
        id: impl Into<String>,
        mem: GuestMemory,
        msi: Arc<dyn MsiSink>,
        intx: Option<Arc<dyn IrqLine>>,
    ) -> Self {
        let mut devices: Vec<Option<Box<dyn UsbDevice>>> = (0..PORTS).map(|_| None).collect();
        devices[TABLET_PORT as usize - 1] = Some(Box::new(Tablet::default()));
        let mut this = Self {
            id: id.into(),
            mem,
            msi,
            intx,
            devices,
            state: Self::initial_state(),
            clock: Instant::now(),
            moderation_clock: Instant::now(),
        };
        this.reset_ports();
        this
    }
    fn initial_state() -> ControllerState {
        let mut pci = [0u8; 256];
        for (offset, value) in [
            (0, 0x000d1b36u32),
            (4, 0x00100000),
            (8, 0x0c033001),
            (0x10, 4),
            (0x2c, 0x11001af4),
            (0x34, 0x90),
            (0x3c, 0x100),
            (0x60, 0x30),
            (0x90, 0x00a011),
            (0x94, 0x3000),
            (0x98, 0x3800),
            (0xa0, 0x00920010),
            (0xa4, 0x8020),
            (0xac, 0x411),
            (0xb0, 0x110000),
            (0xc4, 0x300000),
        ] {
            pci[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        let mut state = ControllerState {
            pci,
            regs: vec![0; BAR_SIZE as usize / 4],
            command: Ring::default(),
            command_running: false,
            slots: vec![Slot::default(); SLOTS + 1],
            event_segment: 0,
            event_index: 0,
            event_cycle: true,
            event_used: 0,
            pending: VecDeque::new(),
            mfindex: 0,
            ehb: false,
            interrupt_sent: false,
            moderation_counter: 0,
        };
        state.regs[STS / 4] = 1;
        state.regs[MSIX / 4 + 3] = 1;
        state
    }
    fn reset_ports(&mut self) {
        for p in 0..PORTS {
            let mut value = 0x200 | (5 << 5);
            if let Some(d) = &self.devices[p] {
                value |= 1 | ((d.speed() as u32) << 10);
                if d.speed() == UsbSpeed::Super {
                    value = (value & !(15 << 5)) | 2;
                } else {
                    value &= !(15 << 5);
                }
            }
            self.set(PORT + p * 16, value);
        }
    }
    fn get(&self, off: usize) -> u32 {
        self.state.regs[off / 4]
    }
    fn set(&mut self, off: usize, value: u32) {
        self.state.regs[off / 4] = value;
    }
    fn get64(&self, off: usize) -> u64 {
        u64::from(self.get(off)) | (u64::from(self.get(off + 4)) << 32)
    }
    fn pci16(&self, off: usize) -> u16 {
        u16::from_le_bytes(self.state.pci[off..off + 2].try_into().unwrap())
    }
    fn running(&self) -> bool {
        self.get(CMD) & 1 != 0 && self.get(STS) & 0x1001 == 0 && self.pci16(4) & 4 != 0
    }
    fn dma_error(&mut self) {
        self.set(STS, self.get(STS) | 0x1001);
        self.set(CMD, self.get(CMD) & !1);
    }
    fn context_addr(&self, slot: usize) -> Result<u64, DeviceError> {
        let base = self.get64(DCBAAP) & !63;
        let addr = base.checked_add((slot * 8) as u64).ok_or_else(invalid)?;
        Ok(u64::from_le_bytes(read::<8>(&self.mem, addr)?) & !63)
    }
    fn load_context(&self, addr: u64) -> Result<[u32; 8], DeviceError> {
        let bytes = read::<32>(&self.mem, addr)?;
        Ok(std::array::from_fn(|i| {
            u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
        }))
    }
    fn store_context(&self, addr: u64, ctx: [u32; 8]) -> Result<(), DeviceError> {
        let bytes: Vec<u8> = ctx.into_iter().flat_map(u32::to_le_bytes).collect();
        write(&self.mem, addr, &bytes)
    }
    fn store_slot(&self, slot: usize) -> Result<(), DeviceError> {
        let base = self.context_addr(slot)?;
        self.store_context(base, self.state.slots[slot].context)?;
        for ep in 1..32 {
            self.store_endpoint(slot, ep)?;
        }
        Ok(())
    }
    fn store_endpoint(&self, slot: usize, ep: usize) -> Result<(), DeviceError> {
        let base = self
            .context_addr(slot)?
            .checked_add((ep * 32) as u64)
            .ok_or_else(invalid)?;
        let endpoint = &self.state.slots[slot].endpoints[ep];
        let mut context = endpoint.context;
        context[2] = endpoint.ring.value() as u32;
        context[3] = (endpoint.ring.value() >> 32) as u32;
        self.store_context(base, context)
    }
    /// Attach to a 1-based root port. USB2 devices belong on ports 5..8,
    /// SuperSpeed devices on ports 1..4. Occupied ports are never replaced.
    pub fn attach(&mut self, port: u8, device: Box<dyn UsbDevice>) -> Result<(), DeviceError> {
        let p = Self::port_index(port)?;
        if self.devices[p].is_some() {
            return Err(DeviceError::Other("USB port occupied".into()));
        }
        if (p < 4) != (device.speed() == UsbSpeed::Super) {
            return Err(DeviceError::Other("USB port protocol mismatch".into()));
        }
        let speed = device.speed();
        self.devices[p] = Some(device);
        let value = 0x20201 | ((speed as u32) << 10) | if p < 4 { 2 } else { 0 };
        self.set(PORT + p * 16, value);
        self.port_event(p);
        Ok(())
    }
    pub fn detach(&mut self, port: u8) -> Result<Box<dyn UsbDevice>, DeviceError> {
        let p = Self::port_index(port)?;
        let d = self.devices[p]
            .take()
            .ok_or_else(|| DeviceError::Other("USB port empty".into()))?;
        self.set(PORT + p * 16, 0x202a0);
        self.port_event(p);
        Ok(d)
    }
    fn port_index(port: u8) -> Result<usize, DeviceError> {
        if !(1..=PORTS as u8).contains(&port) {
            return Err(DeviceError::Other("invalid USB port".into()));
        }
        Ok(port as usize - 1)
    }
    pub fn inject_pointer(&mut self, event: PointerEvent) -> Result<(), DeviceError> {
        self.devices[TABLET_PORT as usize - 1]
            .as_mut()
            .ok_or_else(|| DeviceError::Other("USB tablet disconnected".into()))?
            .inject_pointer(event)?;
        self.poll();
        Ok(())
    }
    fn port_event(&mut self, p: usize) {
        self.set(STS, self.get(STS) | (1 << 4));
        if self.get(CMD) & 1 != 0 {
            self.event(Trb {
                parameter: ((p + 1) as u64) << 24,
                status: SUCCESS << 24,
                control: 34 << 10,
            });
        }
    }
    fn segments(&self) -> Result<Vec<(u64, u32)>, DeviceError> {
        let count = self.get(ERSTSZ) & 0xffff;
        if count == 0 {
            return Ok(vec![]);
        }
        if count > 16 {
            return Err(DeviceError::Other(
                "USB event segment count exceeds capability".into(),
            ));
        }
        let base = self.get64(ERSTBA) & !63;
        let mut segments = Vec::new();
        for n in 0..count {
            let addr = base.checked_add(u64::from(n) * 16).ok_or_else(invalid)?;
            let b = read::<16>(&self.mem, addr)?;
            let addr = u64::from_le_bytes(b[..8].try_into().unwrap());
            let size = u32::from_le_bytes(b[8..12].try_into().unwrap()) & 0xffff;
            if !(16..=4096).contains(&size) || addr & 63 != 0 {
                return Err(DeviceError::Other("invalid USB event segment".into()));
            }
            addr.checked_add(u64::from(size) * 16).ok_or_else(invalid)?;
            segments.push((addr, size));
        }
        Ok(segments)
    }
    fn event(&mut self, trb: Trb) {
        if self.state.pending.len() == 4096 {
            self.dma_error();
            return;
        }
        self.state.pending.push_back(trb);
        if self.flush_events().is_err() {
            self.dma_error();
        }
    }
    fn flush_events(&mut self) -> Result<(), DeviceError> {
        let segments = self.segments()?;
        if segments.is_empty() {
            return Ok(());
        }
        let capacity: usize = segments.iter().map(|(_, n)| *n as usize).sum();
        while self.state.event_used < capacity.saturating_sub(1) {
            let Some(mut event) = self.state.pending.pop_front() else {
                break;
            };
            let &(base, size) = segments.get(self.state.event_segment).ok_or_else(invalid)?;
            if self.state.event_index >= size {
                return Err(invalid());
            }
            let addr = base + u64::from(self.state.event_index) * 16;
            event.control = (event.control & !1) | u32::from(self.state.event_cycle);
            event.write(&self.mem, addr)?;
            self.state.event_used += 1;
            self.state.event_index += 1;
            if self.state.event_index == size {
                self.state.event_index = 0;
                self.state.event_segment += 1;
                if self.state.event_segment == segments.len() {
                    self.state.event_segment = 0;
                    self.state.event_cycle = !self.state.event_cycle;
                }
            }
            self.set(STS, self.get(STS) | 8);
            self.set(IMAN, self.get(IMAN) | 1);
        }
        self.update_interrupt();
        Ok(())
    }
    fn acknowledge_events(&mut self, old: u64, new: u64) -> Result<(), DeviceError> {
        let seg = self.segments()?;
        let index = |ptr: u64| -> Option<usize> {
            let mut n = 0;
            for &(base, size) in &seg {
                if ptr >= base
                    && ptr < base + u64::from(size) * 16
                    && (ptr - base).is_multiple_of(16)
                {
                    return Some(n + ((ptr - base) / 16) as usize);
                }
                n += size as usize;
            }
            None
        };
        if let (Some(old), Some(new)) = (index(old & !15), index(new & !15)) {
            let cap: usize = seg.iter().map(|(_, n)| *n as usize).sum();
            let used = (new + cap - old) % cap;
            self.state.event_used = self.state.event_used.saturating_sub(used);
        }
        self.flush_events()
    }
    fn update_interrupt(&mut self) {
        let active = self.get(IMAN) & 3 == 3 && self.get(CMD) & 4 != 0;
        let msix = self.pci16(0x92);
        let ready = self.moderation_counter() == 0;
        let intx_enabled = msix & 0x8000 == 0 && self.pci16(4) & 0x400 == 0;
        if active && intx_enabled && self.intx.is_some() && ready && !self.state.ehb {
            self.state.interrupt_sent = true;
            self.state.ehb = true;
            self.start_moderation();
        }
        if let Some(irq) = &self.intx {
            irq.set_level(active && intx_enabled && self.state.interrupt_sent);
        }
        if !active {
            self.state.interrupt_sent = false;
            return;
        }
        if msix & 0x8000 != 0 && !self.state.interrupt_sent && !self.state.ehb && ready {
            if msix & 0x4000 != 0 || self.get(MSIX + 12) & 1 != 0 {
                self.set(PBA, 1);
            } else {
                self.msi.signal(MsiMessage {
                    address: self.get64(MSIX),
                    data: self.get(MSIX + 8),
                });
                self.set(PBA, 0);
                self.state.interrupt_sent = true;
                self.state.ehb = true;
                self.start_moderation();
            }
        }
    }
    fn moderation_counter(&self) -> u32 {
        self.state.moderation_counter.saturating_sub(
            (self.moderation_clock.elapsed().as_nanos() / 250).min(u128::from(u32::MAX)) as u32,
        )
    }
    fn start_moderation(&mut self) {
        self.state.moderation_counter = self.get(IMOD) & 0xffff;
        self.moderation_clock = Instant::now();
    }
    /// Resume NAKed transfers and bounded ring work. No guest-controlled
    /// traversal can monopolize the VMM thread.
    pub fn poll(&mut self) {
        if !self.running() {
            return;
        }
        if self.flush_events().is_err() {
            self.dma_error();
            return;
        }
        if self.state.command_running && self.commands().is_err() {
            self.dma_error();
            return;
        }
        for slot in 1..=SLOTS {
            for ep in 1..32 {
                if self.state.slots[slot].endpoints[ep].kicked && self.transfers(slot, ep).is_err()
                {
                    self.dma_error();
                    return;
                }
            }
        }
    }
    fn commands(&mut self) -> Result<(), DeviceError> {
        for _ in 0..BUDGET {
            if self.state.pending.len() >= BUDGET {
                break;
            }
            let Some((addr, t)) = self.state.command.pop(&self.mem)? else {
                break;
            };
            let (cc, slot) = self.command(t)?;
            self.event(Trb {
                parameter: addr,
                status: cc << 24,
                control: (33 << 10) | ((slot as u32) << 24),
            });
            if !self.running() {
                break;
            }
        }
        Ok(())
    }
    fn command(&mut self, t: Trb) -> Result<(u32, usize), DeviceError> {
        let mut slot = (t.control >> 24) as usize;
        let kind = t.kind();
        if kind == 23 {
            return Ok((SUCCESS, 0));
        }
        if kind == 9 {
            let max = (self.get(CONFIG) & 255).min(SLOTS as u32) as usize;
            return Ok(match (1..=max).find(|&i| !self.state.slots[i].enabled) {
                Some(i) => {
                    self.state.slots[i].enabled = true;
                    slot = i;
                    (SUCCESS, slot)
                }
                None => (9, 0),
            });
        }
        if slot == 0 || slot > SLOTS || !self.state.slots[slot].enabled {
            return Ok((11, slot));
        }
        let ep = ((t.control >> 16) & 31) as usize;
        match kind {
            10 => {
                self.state.slots[slot] = Slot::default();
                Ok((SUCCESS, slot))
            }
            11..=13 => {
                let previous = self.state.slots[slot].clone();
                let result = self.input_context(t, slot);
                if !matches!(result, Ok(SUCCESS)) {
                    self.state.slots[slot] = previous;
                }
                result.map(|cc| (cc, slot))
            }
            14..=16 => {
                let e = &mut self.state.slots[slot].endpoints[ep];
                if ep == 0 || e.context[0] & 7 == 0 {
                    return Ok((12, slot));
                }
                let stopped = if kind == 15 && e.kicked && e.context[0] & 7 == 1 {
                    let mut ring = e.ring;
                    ring.pop(&self.mem)?
                } else {
                    None
                };
                match kind {
                    14 if e.context[0] & 7 != 2 => return Ok((CONTEXT_ERROR, slot)),
                    14 => e.context[0] = (e.context[0] & !7) | 3,
                    15 => {
                        e.context[0] = (e.context[0] & !7) | 3;
                        e.kicked = false;
                    }
                    16 if e.context[0] & 7 != 3 => return Ok((CONTEXT_ERROR, slot)),
                    16 => {
                        e.ring = Ring::new(t.parameter);
                        e.kicked = false;
                    }
                    _ => unreachable!(),
                }
                self.store_endpoint(slot, ep)?;
                if let Some((addr, trb)) = stopped {
                    self.transfer_event(slot, ep, addr, 26, trb.length() as u32, false);
                }
                Ok((SUCCESS, slot))
            }
            17 => {
                let port = self.state.slots[slot].port;
                if let Some(device) = port.checked_sub(1).and_then(|p| self.devices[p].as_mut()) {
                    device.reset();
                }
                for e in &mut self.state.slots[slot].endpoints[2..] {
                    *e = Endpoint::default();
                }
                self.state.slots[slot].context[3] =
                    (self.state.slots[slot].context[3] & 0x07ff_ff00) | (1 << 27);
                self.store_slot(slot)?;
                Ok((SUCCESS, slot))
            }
            _ => Ok((TRB_ERROR, slot)),
        }
    }
    fn input_context(&mut self, t: Trb, slot: usize) -> Result<u32, DeviceError> {
        let kind = t.kind();
        if kind == 12 && t.control & 0x200 != 0 {
            if self.state.slots[slot].context[3] >> 27 < 2 {
                return Ok(CONTEXT_ERROR);
            }
            for ep in &mut self.state.slots[slot].endpoints[2..] {
                *ep = Endpoint::default();
            }
            self.state.slots[slot].context[0] =
                (self.state.slots[slot].context[0] & 0x07ff_ffff) | (1 << 27);
            self.state.slots[slot].context[3] = (2 << 27) | slot as u32;
            self.store_slot(slot)?;
            return Ok(SUCCESS);
        }
        let base = t.parameter & !15;
        base.checked_add(33 * 32).ok_or_else(invalid)?;
        let input = self.load_context(base)?;
        let (drop, add) = (input[0], input[1]);
        if (kind == 11 && (drop != 0 || add != 3))
            || (kind == 12 && (drop & 3 != 0 || add & 2 != 0))
            || (kind == 13 && (drop != 0 || add & !3 != 0))
        {
            return Ok(TRB_ERROR);
        }
        // Validate all endpoint contexts before changing a slot or USB address.
        for ep in 1..32 {
            if add & (1 << ep) != 0 && kind != 13 {
                let ctx = self.load_context(base + ((ep + 1) * 32) as u64)?;
                let endpoint_type = (ctx[1] >> 3) & 7;
                if (ctx[1] >> 10) & 31 != 0
                    || ctx[1] >> 16 == 0
                    || (ep == 1 && endpoint_type != 4)
                    || (ep != 1 && !matches!(endpoint_type, 2 | 3 | 6 | 7))
                {
                    return Ok(TRB_ERROR);
                }
            }
        }
        if kind == 11 {
            if add & 3 != 3 || drop != 0 {
                return Ok(TRB_ERROR);
            }
            let context = self.load_context(base + 32)?;
            let port = ((context[1] >> 16) & 255) as usize;
            if port == 0 || port > PORTS || self.devices[port - 1].is_none() {
                return Ok(4);
            }
            if self.get(PORT + (port - 1) * 16) & 2 == 0 {
                return Ok(4);
            }
            if self
                .state
                .slots
                .iter()
                .enumerate()
                .any(|(i, s)| i != slot && s.enabled && s.context[3] >> 27 >= 1 && s.port == port)
            {
                return Ok(CONTEXT_ERROR);
            }
            self.state.slots[slot].context = context;
            self.state.slots[slot].port = port;
            let bsr = t.control & 0x200 != 0;
            if !bsr {
                let result = self.devices[port - 1].as_mut().unwrap().control(
                    SetupPacket {
                        request_type: 0,
                        request: 5,
                        value: slot as u16,
                        index: 0,
                        length: 0,
                    },
                    &[],
                );
                if !matches!(result, UsbTransfer::Ack(_)) {
                    return Ok(4);
                }
            }
            self.state.slots[slot].context[3] = if bsr {
                1 << 27
            } else {
                (2 << 27) | slot as u32
            };
        } else if self.state.slots[slot].context[3] >> 27 == 0 {
            return Ok(CONTEXT_ERROR);
        }
        {
            for ep in 1..32 {
                if drop & (1 << ep) != 0 {
                    self.state.slots[slot].endpoints[ep] = Endpoint::default();
                }
                if add & (1 << ep) != 0 {
                    let mut ctx = self.load_context(base + ((ep + 1) * 32) as u64)?;
                    if kind == 13 {
                        let e = &mut self.state.slots[slot].endpoints[ep];
                        e.context[1] = (e.context[1] & 0xffff) | (ctx[1] & 0xffff0000);
                        e.context[4] = ctx[4];
                    } else {
                        if (ctx[1] >> 10) & 31 != 0 {
                            return Ok(TRB_ERROR);
                        }
                        let ring = Ring::new(u64::from(ctx[2]) | (u64::from(ctx[3]) << 32));
                        ctx[0] = (ctx[0] & !7) | 1;
                        self.state.slots[slot].endpoints[ep] = Endpoint {
                            context: ctx,
                            ring,
                            kicked: false,
                        };
                    }
                }
            }
            if kind == 12 {
                if add & 1 != 0 {
                    self.state.slots[slot].context[0] = self.load_context(base + 32)?[0];
                }
                self.state.slots[slot].context[3] = (3 << 27) | slot as u32;
            }
            if kind == 13 && add & 1 != 0 {
                let ctx = self.load_context(base + 32)?;
                self.state.slots[slot].context[1] =
                    (self.state.slots[slot].context[1] & 0xffff0000) | (ctx[1] & 0xffff);
                self.state.slots[slot].context[2] = ctx[2];
            }
        }
        self.store_slot(slot)?;
        Ok(SUCCESS)
    }
    fn transfers(&mut self, slot: usize, ep: usize) -> Result<(), DeviceError> {
        for _ in 0..BUDGET {
            if self.state.pending.len() >= BUDGET {
                break;
            }
            let endpoint = &self.state.slots[slot].endpoints[ep];
            if !self.state.slots[slot].enabled || !matches!(endpoint.context[0] & 7, 1 | 3) {
                break;
            }
            let mut ring = endpoint.ring;
            let mut td = Vec::new();
            let control = ep == 1;
            let mut finished = false;
            for _ in 0..BUDGET {
                let Some((addr, t)) = ring.pop(&self.mem)? else {
                    break;
                };
                td.push((addr, t));
                if (control && t.kind() == 4 && t.control & 16 == 0)
                    || (!control && t.control & 16 == 0)
                    || t.kind() == 7
                {
                    finished = true;
                    break;
                }
            }
            if td.is_empty() {
                break;
            }
            if !finished {
                if td.len() < BUDGET {
                    break;
                }
                let context = endpoint.context[0];
                self.transfer_event(slot, ep, td[0].0, TRB_ERROR, 0, false);
                self.state.slots[slot].endpoints[ep].context[0] = (context & !7) | 2;
                self.store_endpoint(slot, ep)?;
                break;
            }
            let setup = if control {
                Some(SetupPacket::from_bytes(td[0].1.parameter.to_le_bytes()))
            } else {
                None
            };
            let input = setup.map_or(ep & 1 != 0, |s| s.request_type & 0x80 != 0);
            let mut buffers = Vec::new();
            let mut length = 0usize;
            let mut valid = true;
            for &(addr, t) in &td {
                let data = if control {
                    t.kind() == 3 || t.kind() == 1
                } else {
                    t.kind() == 1
                };
                if data {
                    length = length.saturating_add(t.length());
                    if t.control & 64 != 0 && (input || t.length() > 8) {
                        valid = false;
                    }
                    buffers.push((addr, t));
                } else if !matches!(t.kind(), 2 | 4 | 7 | 8) {
                    valid = false;
                }
            }
            if control
                && (td[0].1.kind() != 2
                    || td[0].1.control & 64 == 0
                    || td[0].1.length() != 8
                    || !td.iter().any(|(_, t)| t.kind() == 4))
            {
                valid = false;
            }
            if length > 1024 * 1024 {
                valid = false;
            }
            let mut data = Vec::new();
            if valid && !input {
                for &(_, t) in &buffers {
                    if t.control & 64 != 0 {
                        data.extend_from_slice(&t.parameter.to_le_bytes()[..t.length()]);
                    } else {
                        let start = data.len();
                        data.resize(start + t.length(), 0);
                        t.parameter
                            .checked_add(t.length() as u64)
                            .ok_or_else(invalid)?;
                        use vm_memory::Bytes;
                        self.mem
                            .read_slice(&mut data[start..], vm_memory::GuestAddress(t.parameter))
                            .map_err(|e| DeviceError::Memory(e.to_string()))?;
                    }
                }
            }
            let port = self.state.slots[slot].port;
            let outcome = if !valid {
                UsbTransfer::Stall
            } else if let Some(d) = port
                .checked_sub(1)
                .and_then(|p| self.devices.get_mut(p))
                .and_then(Option::as_mut)
            {
                if let Some(s) = setup {
                    d.control(s, &data)
                } else if td.iter().all(|(_, t)| t.kind() == 8) {
                    UsbTransfer::Ack(vec![])
                } else {
                    d.transfer(
                        ((ep / 2) as u8) | if input { 0x80 } else { 0 },
                        &data,
                        length,
                    )
                }
            } else {
                UsbTransfer::Stall
            };
            let (cc, actual) = match outcome {
                UsbTransfer::Nak => break,
                UsbTransfer::Stall => (if valid { STALL } else { TRB_ERROR }, 0),
                UsbTransfer::Ack(bytes) => {
                    let actual = if input {
                        bytes.len().min(length)
                    } else {
                        length
                    };
                    if input {
                        let mut offset = 0;
                        for &(_, t) in &buffers {
                            let n = t.length().min(actual - offset);
                            write(&self.mem, t.parameter, &bytes[offset..offset + n])?;
                            offset += n;
                        }
                    }
                    (SUCCESS, actual)
                }
            };
            self.state.slots[slot].endpoints[ep].ring = ring;
            if cc != SUCCESS {
                self.state.slots[slot].endpoints[ep].context[0] =
                    (self.state.slots[slot].endpoints[ep].context[0] & !7) | 2;
                self.state.slots[slot].endpoints[ep].kicked = false;
                self.transfer_event(slot, ep, td[0].0, cc, length as u32, false);
            } else {
                self.state.slots[slot].endpoints[ep].context[0] =
                    (self.state.slots[slot].endpoints[ep].context[0] & !7) | 1;
                let mut remaining = actual;
                let mut short_reported = false;
                for &(addr, t) in &td {
                    let is_data = buffers.iter().any(|(p, _)| *p == addr);
                    let n = if is_data {
                        t.length().min(remaining)
                    } else {
                        0
                    };
                    let short = is_data && n < t.length();
                    remaining -= n;
                    let event_data = t.kind() == 7;
                    if t.control & 32 != 0 || (short && t.control & 4 != 0 && !short_reported) {
                        let code = if short { 13 } else { SUCCESS };
                        let residual = if event_data {
                            actual as u32
                        } else if is_data {
                            (t.length() - n) as u32
                        } else {
                            0
                        };
                        self.transfer_event(
                            slot,
                            ep,
                            if event_data { t.parameter } else { addr },
                            code,
                            residual,
                            event_data,
                        );
                        if short {
                            short_reported = true;
                        }
                    }
                }
            }
            self.store_endpoint(slot, ep)?;
            if cc != SUCCESS || !self.running() {
                break;
            }
        }
        Ok(())
    }
    fn transfer_event(
        &mut self,
        slot: usize,
        ep: usize,
        pointer: u64,
        cc: u32,
        residual: u32,
        event_data: bool,
    ) {
        self.event(Trb {
            parameter: pointer,
            status: (cc << 24) | (residual & 0xff_ffff),
            control: (32 << 10)
                | ((slot as u32) << 24)
                | ((ep as u32) << 16)
                | (u32::from(event_data) << 2),
        });
    }
    fn microframe(&self) -> u32 {
        self.state.mfindex.wrapping_add(if self.get(CMD) & 1 != 0 {
            (self.clock.elapsed().as_micros() / 125) as u32
        } else {
            0
        }) & 0x3fff
    }
    fn read_reg(&self, off: usize) -> u32 {
        match off {
            0 => 0x01000040,
            4 => (8 << 24) | (1 << 8) | SLOTS as u32,
            8 => 0x4f,
            // Streams are optional and are not advertised by this controller.
            0x10 => 0x00080001,
            0x14 => 0x2000,
            0x18 => 0x1000,
            0x20 => 0x02000402,
            0x24 | 0x34 => 0x20425355,
            0x28 => 0x405,
            0x30 => 0x03000002,
            0x38 => 0x401,
            0x48 => 1,
            CRCR => (self.get(CRCR) & !0xe) | if self.state.command_running { 8 } else { 0 },
            ERDP => (self.get(ERDP) & !8) | (u32::from(self.state.ehb) << 3),
            0x1000 => self.microframe(),
            IMOD => (self.get(IMOD) & 0xffff) | (self.moderation_counter() << 16),
            _ => self.get(off),
        }
    }
    fn write_port(&mut self, off: usize, v: u32, mask: u32) {
        let p = (off - PORT) / 16;
        if !(off - PORT).is_multiple_of(16) {
            if (off - PORT) % 16 == 4 {
                self.set(off, (self.get(off) & !mask) | (v & mask));
            }
            return;
        }
        let old = self.get(off);
        let mut value = old & !(v & 0xfe0000);
        if v & 2 != 0 {
            value &= !2;
        }
        // PPC is zero, so port power is always on.
        let rw = mask & (7 << 25);
        value = (value & !rw) | (v & rw);
        if v & (1 << 16) != 0 {
            value = (value & !(15 << 5)) | (v & (15 << 5));
            if value & (15 << 5) != old & (15 << 5) {
                value |= 1 << 22;
            }
        }
        if v & ((1 << 4) | (1 << 31)) != 0
            && let Some(d) = &mut self.devices[p]
        {
            d.reset();
            value = (value & !((15 << 5) | (1 << 4) | (1 << 31))) | 2;
            value |= if v & (1 << 31) != 0 { 1 << 19 } else { 1 << 21 };
        }
        self.set(off, value);
        if (value & 0xfe0000) & !(old & 0xfe0000) != 0 {
            self.port_event(p);
        }
    }
    fn write_reg(&mut self, off: usize, v: u32, mask: u32) {
        let merged = (self.get(off) & !mask) | (v & mask);
        match off {
            CMD => {
                if v & 2 != 0 {
                    let pci = self.state.pci;
                    let msix = self.state.regs[MSIX / 4..MSIX / 4 + 4].to_vec();
                    self.state = Self::initial_state();
                    self.state.pci = pci;
                    self.state.regs[MSIX / 4..MSIX / 4 + 4].copy_from_slice(&msix);
                    self.reset_ports();
                    self.clock = Instant::now();
                    self.moderation_clock = Instant::now();
                    self.update_interrupt();
                    return;
                }
                let mf = self.microframe();
                self.set(CMD, merged & 0xc0d);
                self.state.mfindex = mf;
                self.clock = Instant::now();
                self.set(STS, (self.get(STS) & !1) | u32::from(merged & 1 == 0));
                if merged & 1 == 0 {
                    self.state.command_running = false;
                }
                self.poll();
                self.update_interrupt();
            }
            STS => {
                self.set(STS, self.get(STS) & !(v & 0x41c));
                self.update_interrupt();
            }
            CRCR => {
                if v & 6 != 0 && self.state.command_running {
                    self.state.command_running = false;
                    self.event(Trb {
                        parameter: self.state.command.pointer,
                        status: 24 << 24,
                        control: 33 << 10,
                    });
                } else if !self.state.command_running {
                    self.set(off, merged & !0x3e);
                    self.state.command = Ring::new(self.get64(CRCR));
                }
            }
            x if x == CRCR + 4 => {
                if !self.state.command_running {
                    self.set(off, merged);
                    self.state.command = Ring::new(self.get64(CRCR));
                }
            }
            DCBAAP => self.set(off, merged & !63),
            x if x == DCBAAP + 4 => self.set(off, merged),
            CONFIG => self.set(off, (merged & 255).min(SLOTS as u32)),
            IMAN => {
                self.set(off, ((self.get(off) & 1) & !(v & 1)) | (merged & 2));
                if v & 1 != 0 {
                    self.state.interrupt_sent = false;
                }
                self.update_interrupt();
            }
            IMOD => {
                let current = self.read_reg(IMOD);
                let merged = (current & !mask) | (v & mask);
                self.set(IMOD, merged & 0xffff);
                self.state.moderation_counter = merged >> 16;
                self.moderation_clock = Instant::now();
                self.update_interrupt();
            }
            ERSTSZ => self.set(off, merged & 0xffff),
            ERSTBA => {
                self.set(off, merged & !63);
                self.reset_event_ring();
            }
            x if x == ERSTBA + 4 => {
                self.set(off, merged);
                self.reset_event_ring();
            }
            ERDP => {
                let old = self.get64(ERDP);
                self.set(off, merged & !15);
                if v & 8 != 0 {
                    self.state.ehb = false;
                    self.state.interrupt_sent = false;
                }
                if self.acknowledge_events(old, self.get64(ERDP)).is_err() {
                    self.dma_error();
                }
                self.update_interrupt();
            }
            x if x == ERDP + 4 => {
                let old = self.get64(ERDP);
                self.set(off, merged);
                if self.acknowledge_events(old, self.get64(ERDP)).is_err() {
                    self.dma_error();
                }
            }
            x if (PORT..PORT + PORTS * 16).contains(&x) => self.write_port(off, v, mask),
            x if (0x2000..0x2000 + (SLOTS + 1) * 4).contains(&x) => {
                if !self.running() {
                    return;
                }
                let slot = (off - 0x2000) / 4;
                if slot == 0 {
                    self.state.command_running = true;
                } else {
                    let ep = (v & 255) as usize;
                    if ep == 0 || ep > 31 || v >> 16 != 0 {
                        return;
                    }
                    self.state.slots[slot].endpoints[ep].kicked = true;
                }
                self.poll();
            }
            x if (MSIX..MSIX + 16).contains(&x) => {
                self.set(off, if off == MSIX + 12 { merged & 1 } else { merged });
                self.update_interrupt();
            }
            _ => {}
        }
    }
    fn reset_event_ring(&mut self) {
        self.state.event_segment = 0;
        self.state.event_index = 0;
        self.state.event_cycle = true;
        self.state.event_used = 0;
    }
}

impl BusDevice for XhciController {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        let Ok(mut offset) = usize::try_from(offset) else {
            return;
        };
        let mut copied = 0;
        while copied < data.len() && offset < BAR_SIZE as usize {
            let bytes = self.read_reg(offset & !3).to_le_bytes();
            let n = (4 - (offset & 3)).min(data.len() - copied);
            data[copied..copied + n].copy_from_slice(&bytes[offset & 3..(offset & 3) + n]);
            offset += n;
            copied += n;
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        let Ok(mut offset) = usize::try_from(offset) else {
            return;
        };
        let mut data = data;
        while !data.is_empty() && offset < BAR_SIZE as usize {
            let n = (4 - (offset & 3)).min(data.len());
            let mut v = 0u32;
            let mut mask = 0u32;
            for (i, b) in data[..n].iter().enumerate() {
                let shift = ((offset & 3) + i) * 8;
                v |= u32::from(*b) << shift;
                mask |= 255 << shift;
            }
            self.write_reg(offset & !3, v, mask);
            offset += n;
            data = &data[n..];
        }
    }
}

impl PciDevice for XhciController {
    fn config_read(&mut self, reg: usize) -> u32 {
        reg.checked_mul(4)
            .and_then(|off| self.state.pci.get(off..off.checked_add(4)?))
            .map_or(u32::MAX, |b| u32::from_le_bytes(b.try_into().unwrap()))
    }
    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        let Some(start) = reg
            .checked_mul(4)
            .and_then(|r| r.checked_add(usize::try_from(offset).ok()?))
        else {
            return;
        };
        for (i, &b) in data.iter().enumerate() {
            let Some(off) = start.checked_add(i).filter(|&off| off < 256) else {
                break;
            };
            let mask = match off {
                4 => 7,
                5 => 4,
                0xc | 0xd | 0x3c => 255,
                0x10 => 0,
                0x11 => 0xc0,
                0x12..=0x17 => 255,
                0x93 => 0xc0,
                0xa8 | 0xa9 => 0xff,
                _ => 0,
            };
            self.state.pci[off] = (self.state.pci[off] & !mask) | (b & mask);
        }
        self.update_interrupt();
    }
    fn bars(&self) -> Vec<BarInfo> {
        vec![BarInfo {
            index: 0,
            size: BAR_SIZE,
            kind: BarKind::Mmio64 {
                prefetchable: false,
            },
        }]
    }
    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        if bar == 0 {
            self.read(offset, data);
        } else {
            data.fill(0xff);
        }
    }
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if bar == 0 {
            self.write(offset, data);
        }
    }
}

impl Snapshot for XhciController {
    fn state_id(&self) -> String {
        self.id.clone()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let s = &self.state;
        let mut w = Writer(b"VHVXHC01".to_vec());
        w.bytes(&s.pci);
        for &v in &s.regs {
            w.u64(u64::from(v));
        }
        s.command.save(&mut w);
        w.u64(u64::from(s.command_running));
        for slot in &s.slots {
            w.u64(u64::from(slot.enabled));
            w.u64(slot.port as u64);
            for &v in &slot.context {
                w.u64(u64::from(v));
            }
            for ep in &slot.endpoints {
                for &v in &ep.context {
                    w.u64(u64::from(v));
                }
                ep.ring.save(&mut w);
                w.u64(u64::from(ep.kicked));
            }
        }
        w.u64(s.event_segment as u64);
        w.u64(u64::from(s.event_index));
        w.u64(u64::from(s.event_cycle));
        w.u64(s.event_used as u64);
        w.u64(s.pending.len() as u64);
        for t in &s.pending {
            w.u64(t.parameter);
            w.u64(u64::from(t.status));
            w.u64(u64::from(t.control));
        }
        w.u64(u64::from(self.microframe()));
        w.u64(u64::from(s.ehb));
        w.u64(u64::from(s.interrupt_sent));
        w.u64(u64::from(self.moderation_counter()));
        for d in &self.devices {
            w.u64(u64::from(d.is_some()));
            if let Some(d) = d {
                w.bytes(d.state_id().as_bytes());
                w.bytes(&d.save_state()?);
            }
        }
        Ok(w.0)
    }
    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        let mut r = Reader(bytes);
        if r.take(8)? != b"VHVXHC01" {
            return Err(invalid());
        }
        let mut state = Self::initial_state();
        state.pci = r.bytes(256)?.try_into().map_err(|_| invalid())?;
        for v in &mut state.regs {
            *v = r.u32()?;
        }
        state.command = Ring::restore(&mut r)?;
        state.command_running = r.bool()?;
        for slot in &mut state.slots {
            slot.enabled = r.bool()?;
            slot.port = usize::try_from(r.u64()?).map_err(|_| invalid())?;
            if slot.port > PORTS {
                return Err(invalid());
            }
            for v in &mut slot.context {
                *v = r.u32()?;
            }
            for ep in &mut slot.endpoints {
                for v in &mut ep.context {
                    *v = r.u32()?;
                }
                ep.ring = Ring::restore(&mut r)?;
                ep.kicked = r.bool()?;
            }
        }
        state.event_segment = usize::try_from(r.u64()?).map_err(|_| invalid())?;
        state.event_index = r.u32()?;
        state.event_cycle = r.bool()?;
        state.event_used = usize::try_from(r.u64()?).map_err(|_| invalid())?;
        if state.event_segment >= 16 || state.event_index >= 4096 || state.event_used > 65536 {
            return Err(invalid());
        }
        let count = r.u64()?;
        if count > 4096 {
            return Err(invalid());
        }
        for _ in 0..count {
            state.pending.push_back(Trb {
                parameter: r.u64()?,
                status: r.u32()?,
                control: r.u32()?,
            });
        }
        state.mfindex = r.u32()? & 0x3fff;
        state.ehb = r.bool()?;
        state.interrupt_sent = r.bool()?;
        state.moderation_counter = r.u32()?;
        if state.moderation_counter > 0xffff {
            return Err(invalid());
        }
        let mut device_states = Vec::new();
        for d in &self.devices {
            if r.bool()? != d.is_some() {
                return Err(DeviceError::State("USB snapshot topology differs".into()));
            }
            if let Some(d) = d {
                if r.bytes(4096)? != d.state_id().as_bytes() {
                    return Err(DeviceError::State("USB snapshot backend differs".into()));
                }
                device_states.push(Some(r.bytes(16 * 1024 * 1024)?.to_vec()));
            } else {
                device_states.push(None);
            }
        }
        r.end()?;
        let previous: Vec<_> = self
            .devices
            .iter()
            .map(|d| d.as_ref().map(|d| d.save_state()).transpose())
            .collect::<Result<_, _>>()?;
        for (p, bytes) in device_states.iter().enumerate() {
            if let (Some(d), Some(bytes)) = (&mut self.devices[p], bytes)
                && let Err(error) = d.restore_state(bytes)
            {
                for (d, bytes) in self.devices.iter_mut().zip(&previous) {
                    if let (Some(d), Some(bytes)) = (d, bytes) {
                        d.restore_state(bytes)?;
                    }
                }
                return Err(error);
            }
        }
        self.state = state;
        self.clock = Instant::now();
        self.moderation_clock = Instant::now();
        self.update_interrupt();
        Ok(())
    }
}

// xHCI is a single PCI function. Its snapshot includes all root-port devices;
// there is no separate transport-wrapped device section.
impl PciSnapshot for XhciController {
    fn snapshot_id(&self) -> String {
        self.state_id()
    }
    fn save_function_state(&self) -> Result<Vec<u8>, DeviceError> {
        self.save_state()
    }
    fn save_wrapped_state(&self) -> Result<Vec<u8>, DeviceError> {
        Ok(Vec::new())
    }
    fn restore_function_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        self.restore_state(state)
    }
    fn restore_wrapped_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.is_empty() {
            Ok(())
        } else {
            Err(invalid())
        }
    }
}
