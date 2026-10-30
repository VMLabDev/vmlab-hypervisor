//! Q35 chipset wiring and the running-time clock shared by its timers.

use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::time::Instant;

use kvm_ioctls::VmFd;
use parking_lot::Mutex;
use vmlab_hv_devices::chipset::{
    hpet::Hpet,
    isa::IsaDevices,
    lpc::{Lpc, PowerAction},
    ps2::{ControllerEvent, I8042},
    rtc::Rtc,
    smbus::Smbus,
};
use vmlab_hv_devices::{BusDevice, IrqLine};

use crate::{error::VmmError, layout::MemLayout, q35::PirqRouter};

struct IrqHub {
    vm: Arc<VmFd>,
    sources: Mutex<BTreeMap<(u32, u32), bool>>,
}

struct Source {
    hub: Arc<IrqHub>,
    id: u32,
    gsi: u32,
}

impl IrqLine for Source {
    fn set_level(&self, high: bool) {
        let mut sources = self.hub.sources.lock();
        sources.insert((self.id, self.gsi), high);
        let level = sources
            .iter()
            .any(|((_, gsi), active)| *gsi == self.gsi && *active);
        if let Err(e) = self.hub.vm.set_irq_line(self.gsi, level) {
            eprintln!("q35 IRQ {}: {e}", self.gsi);
        }
    }
}

impl IrqHub {
    fn line(self: &Arc<Self>, id: u32, gsi: u32) -> Arc<dyn IrqLine> {
        Arc::new(Source {
            hub: self.clone(),
            id,
            gsi,
        })
    }
}

struct RtcGate {
    enabled: AtomicBool,
    level: AtomicBool,
    output: Arc<dyn IrqLine>,
}
impl IrqLine for RtcGate {
    fn set_level(&self, high: bool) {
        self.level.store(high, Ordering::SeqCst);
        self.output
            .set_level(high && self.enabled.load(Ordering::SeqCst));
    }
}

pub struct Chipset {
    pub smi_enable: AtomicU32,
    pub smi_pending: AtomicBool,
    pub lpc: Arc<Mutex<Lpc>>,
    pub rtc: Arc<Mutex<Rtc>>,
    pub ps2: Arc<Mutex<I8042>>,
    pub hpet: Arc<Mutex<Hpet>>,
    pub isa: Arc<Mutex<IsaDevices>>,
    pub smbus: Arc<Mutex<Smbus>>,
    pub pirqs: Arc<Mutex<PirqRouter>>,
    pub power: Arc<Mutex<Option<PowerAction>>>,
    pub error: Arc<Mutex<Option<String>>>,
    clock: Mutex<(Instant, bool)>,
    vm: Arc<VmFd>,
}

pub struct ChipsetRegisters(pub Arc<Chipset>);
impl vmlab_hv_devices::Snapshot for ChipsetRegisters {
    fn state_id(&self) -> String {
        "q35-platform-registers".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, vmlab_hv_devices::DeviceError> {
        Ok(self
            .0
            .smi_enable
            .load(Ordering::SeqCst)
            .to_le_bytes()
            .to_vec())
    }
    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), vmlab_hv_devices::DeviceError> {
        let bytes: [u8; 4] = bytes
            .try_into()
            .map_err(|_| vmlab_hv_devices::DeviceError::State("invalid SMI enable state".into()))?;
        self.0
            .smi_enable
            .store(u32::from_le_bytes(bytes), Ordering::SeqCst);
        self.0.smi_pending.store(false, Ordering::SeqCst);
        Ok(())
    }
}

impl Chipset {
    pub fn new(vm: Arc<VmFd>, layout: &MemLayout, rtc_base: i64) -> Result<Arc<Self>, VmmError> {
        install_irq_routes(&vm)?;
        let hub = Arc::new(IrqHub {
            vm: vm.clone(),
            sources: Mutex::new(BTreeMap::new()),
        });
        let pirqs = PirqRouter::new(std::array::from_fn(|gsi| hub.line(0, gsi as u32)));
        let power = Arc::new(Mutex::new(None));
        let error = Arc::new(Mutex::new(None));
        let pending = power.clone();
        let lpc = Arc::new(Mutex::new(Lpc::new(
            hub.line(1, 9),
            Arc::new(move |action| *pending.lock() = Some(action)),
        )));
        let gate = Arc::new(RtcGate {
            enabled: AtomicBool::new(true),
            level: AtomicBool::new(false),
            output: hub.line(2, 8),
        });
        let mut rtc = Rtc::new(layout.ram, rtc_base, gate.clone());
        rtc.set_memory_layout(
            layout.low_ram_top(),
            layout.ram.saturating_sub(layout.low_ram_top()),
        );
        let pending = power.clone();
        let ps2 = Arc::new(Mutex::new(I8042::new(
            hub.line(3, 1),
            hub.line(3, 12),
            Arc::new(move |event| {
                if event == ControllerEvent::Reset {
                    *pending.lock() = Some(PowerAction::Reset);
                }
            }),
        )));
        let pit_vm = vm.clone();
        let errors = error.clone();
        let hpet = Arc::new(Mutex::new(Hpet::new(
            std::array::from_fn(|gsi| hub.line(4, gsi as u32)),
            Arc::new(move |legacy| {
                gate.enabled.store(!legacy, Ordering::SeqCst);
                gate.output
                    .set_level(!legacy && gate.level.load(Ordering::SeqCst));
                let result = pit_vm.get_pit2().and_then(|mut pit| {
                    if legacy {
                        pit.flags |= kvm_bindings::KVM_PIT_FLAGS_HPET_LEGACY;
                    } else {
                        pit.flags &= !kvm_bindings::KVM_PIT_FLAGS_HPET_LEGACY;
                    }
                    pit_vm.set_pit2(&pit)
                });
                if let Err(e) = result {
                    *errors.lock() = Some(format!("HPET legacy PIT gating: {e}"));
                }
            }),
        )));
        let smbus = Arc::new(Mutex::new(Smbus::new(
            "ich9-smbus",
            PirqRouter::line(
                &pirqs,
                vmlab_hv_proto::machine::PciAddress {
                    bus: 0,
                    device: 31,
                    function: 3,
                },
                1,
            )
            .map_err(VmmError::Other)?,
        )));
        Ok(Arc::new(Self {
            smi_enable: AtomicU32::new(0),
            smi_pending: AtomicBool::new(false),
            lpc,
            rtc: Arc::new(Mutex::new(rtc)),
            ps2,
            hpet,
            isa: Arc::new(Mutex::new(IsaDevices::new())),
            smbus,
            pirqs,
            power,
            error,
            clock: Mutex::new((Instant::now(), false)),
            vm,
        }))
    }

    pub fn advance(&self) {
        self.clock_step(None);
    }
    pub fn set_running(&self, running: bool) {
        self.clock_step(Some(running));
    }

    fn clock_step(&self, running: Option<bool>) {
        self.clock_step_at(Instant::now(), running);
    }

    fn clock_step_at(&self, now: Instant, running: Option<bool>) {
        let mut clock = self.clock.lock();
        if clock.1 {
            let ns = now.duration_since(clock.0).as_nanos().min(u64::MAX as u128) as u64;
            self.lpc.lock().advance(ns);
            self.rtc.lock().advance(ns);
            self.hpet.lock().advance(ns);
            self.isa.lock().advance(ns);
        }
        clock.0 = now;
        if let Some(running) = running {
            clock.1 = running;
        }
    }

    pub fn sync_pirqs(&self) {
        let lpc = self.lpc.lock();
        let mut router = self.pirqs.lock();
        for i in 0..8 {
            router.set_route(i, lpc.pirq_route(i).unwrap_or(0x80));
        }
    }

    pub fn io_read(&self, port: u16, data: &mut [u8]) -> bool {
        self.advance();
        if self.smi_register(port, data.len()) {
            let bytes = self.smi_enable.load(Ordering::SeqCst).to_le_bytes();
            let offset = port.wrapping_sub(self.lpc.lock().pm_base()) as usize - 0x30;
            data.copy_from_slice(&bytes[offset..offset + data.len()]);
            return true;
        }
        if self.lpc.lock().read_port(port, data) {
            return true;
        }
        match port {
            0x70..=0x77 => self.rtc.lock().read((port - 0x70) as u64, data),
            0x60 | 0x64 => self.ps2.lock().read((port - 0x60) as u64, data),
            0x61 | 0x00..=0x0f | 0x80..=0x8f | 0xc0..=0xdf | 0x378..=0x37f => {
                let mut isa = self.isa.lock();
                if port == 0x61 {
                    let pit = self.vm.get_pit2();
                    if let Ok(pit) = pit {
                        let channel = pit.channels[2];
                        // PIT2 channel-2 OUT for the firmware's calibration
                        // loop. KVM tracks the count load time in ns.
                        let now = host_monotonic_ns();
                        let ticks = now.saturating_sub(channel.count_load_time as u64) as u128
                            * 1_193_182
                            / 1_000_000_000;
                        let count = if channel.count == 0 {
                            65536
                        } else {
                            channel.count as u128
                        };
                        let high = match channel.mode {
                            0 => ticks >= count,
                            2 => ticks % count != count - 1,
                            3 => ticks % count < count.div_ceil(2),
                            _ => ticks >= count,
                        };
                        isa.set_pit2_output(high);
                    }
                }
                for (i, byte) in data.iter_mut().enumerate() {
                    *byte = isa.io_read(port.wrapping_add(i as u16));
                }
            }
            _ => return false,
        }
        true
    }

    pub fn io_write(&self, port: u16, data: &[u8]) -> bool {
        self.advance();
        if self.smi_register(port, data.len()) {
            let mut bytes = self.smi_enable.load(Ordering::SeqCst).to_le_bytes();
            let offset = port.wrapping_sub(self.lpc.lock().pm_base()) as usize - 0x30;
            bytes[offset..offset + data.len()].copy_from_slice(data);
            self.smi_enable
                .store(u32::from_le_bytes(bytes), Ordering::SeqCst);
            return true;
        }
        if self.lpc.lock().write_port(port, data) {
            if port == 0xb2 && self.smi_enable.load(Ordering::SeqCst) & 0x21 == 0x21 {
                self.smi_pending.store(true, Ordering::SeqCst);
            }
            return true;
        }
        match port {
            0x70..=0x77 => self.rtc.lock().write((port - 0x70) as u64, data),
            0x60 | 0x64 => self.ps2.lock().write((port - 0x60) as u64, data),
            0x61 | 0x00..=0x0f | 0x80..=0x8f | 0xc0..=0xdf | 0x378..=0x37f => {
                let mut isa = self.isa.lock();
                for (i, byte) in data.iter().enumerate() {
                    isa.io_write(port.wrapping_add(i as u16), *byte);
                }
                if port == 0x61 {
                    match self.vm.get_pit2().and_then(|mut pit| {
                        pit.channels[2].gate = u8::from(isa.pit2_gate());
                        self.vm.set_pit2(&pit)
                    }) {
                        Ok(()) => {}
                        Err(e) => *self.error.lock() = Some(format!("PIT channel 2 gate: {e}")),
                    }
                }
            }
            _ => return false,
        }
        true
    }

    fn smi_register(&self, port: u16, len: usize) -> bool {
        let lpc = self.lpc.lock();
        let offset = port.wrapping_sub(lpc.pm_base()) as usize;
        lpc.pm_enabled() && (0x30..0x34).contains(&offset) && len <= 0x34 - offset
    }
}

pub(crate) fn host_monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid writable timespec; CLOCK_MONOTONIC is supported
    // by Linux and matches the KVM PIT count-load clock.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn install_irq_routes(vm: &VmFd) -> Result<(), VmmError> {
    use kvm_bindings::*;
    #[repr(C)]
    struct Routes {
        header: kvm_irq_routing,
        entries: [kvm_irq_routing_entry; 40],
    }
    let mut routes = Box::new(Routes {
        header: kvm_irq_routing::default(),
        entries: [kvm_irq_routing_entry::default(); 40],
    });
    let mut add = |gsi, irqchip, pin| {
        let entry = &mut routes.entries[routes.header.nr as usize];
        entry.gsi = gsi;
        entry.type_ = KVM_IRQ_ROUTING_IRQCHIP;
        entry.u.irqchip = kvm_irq_routing_irqchip { irqchip, pin };
        routes.header.nr += 1;
    };
    for gsi in 0..24 {
        add(gsi, KVM_IRQCHIP_IOAPIC, if gsi == 0 { 2 } else { gsi });
    }
    for gsi in 0..16 {
        if gsi == 2 {
            continue;
        }
        add(
            gsi,
            if gsi < 8 {
                KVM_IRQCHIP_PIC_MASTER
            } else {
                KVM_IRQCHIP_PIC_SLAVE
            },
            gsi % 8,
        );
    }
    add(2, KVM_IRQCHIP_PIC_MASTER, 0);
    vm.set_gsi_routing(&routes.header)
        .map_err(|e| VmmError::ioctl("KVM_SET_GSI_ROUTING(q35)", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn platform_timers_freeze_between_pause_and_resume() {
        let kvm = match crate::kvm::KvmVm::new() {
            Ok(kvm) => kvm,
            Err(error) => {
                eprintln!("skipping timer clock test: {error}");
                return;
            }
        };
        let chipset = Chipset::new(kvm.vm, &MemLayout::q35(64).unwrap(), 0).unwrap();
        let base = chipset.clock.lock().0;
        chipset.clock_step_at(base, Some(true));
        chipset.clock_step_at(base + std::time::Duration::from_secs(1), Some(false));
        let mut bytes = [0; 4];
        chipset.lpc.lock().read(8, &mut bytes);
        assert_eq!(u32::from_le_bytes(bytes), 3_579_545);
        chipset.clock_step_at(base + std::time::Duration::from_secs(3), Some(true));
        chipset.lpc.lock().read(8, &mut bytes);
        assert_eq!(u32::from_le_bytes(bytes), 3_579_545);
        chipset.clock_step_at(base + std::time::Duration::from_secs(4), None);
        chipset.lpc.lock().read(8, &mut bytes);
        assert_eq!(u32::from_le_bytes(bytes), 7_159_090);
    }
}
