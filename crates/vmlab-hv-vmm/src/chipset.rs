//! Q35 chipset wiring and the running-time clock shared by its timers.

use std::collections::BTreeMap;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

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
use vmlab_hv_devices::{BusDevice, IrqLine, PciDevice};

use crate::{error::VmmError, layout::MemLayout, q35::PirqRouter};

struct IrqHub {
    deliver: Box<dyn Fn(u32, bool) + Send + Sync>,
    sources: Mutex<BTreeMap<(u32, u32), bool>>,
    lines: Mutex<BTreeMap<(u32, u32), Weak<Source>>>,
}

struct Source {
    hub: Arc<IrqHub>,
    id: u32,
    gsi: u32,
    level: AtomicBool,
}

impl IrqLine for Source {
    fn set_level(&self, high: bool) {
        if self.level.load(Ordering::Acquire) == high {
            return;
        }
        let mut sources = self.hub.sources.lock();
        if sources.get(&(self.id, self.gsi)).copied().unwrap_or(false) == high {
            self.level.store(high, Ordering::Release);
            return;
        }
        let previous = sources
            .iter()
            .any(|((_, gsi), active)| *gsi == self.gsi && *active);
        sources.insert((self.id, self.gsi), high);
        let level = sources
            .iter()
            .any(|((_, gsi), active)| *gsi == self.gsi && *active);
        if previous != level {
            (self.hub.deliver)(self.gsi, level);
        }
        // Publish only after the wired-OR output has been delivered. A
        // repeated level can then return without taking the shared lock.
        self.level.store(high, Ordering::Release);
    }
}

impl IrqHub {
    fn line(self: &Arc<Self>, id: u32, gsi: u32) -> Arc<dyn IrqLine> {
        let mut lines = self.lines.lock();
        if let Some(line) = lines.get(&(id, gsi)).and_then(Weak::upgrade) {
            return line;
        }
        let level = self
            .sources
            .lock()
            .get(&(id, gsi))
            .copied()
            .unwrap_or(false);
        let line = Arc::new(Source {
            hub: self.clone(),
            id,
            gsi,
            level: AtomicBool::new(level),
        });
        lines.insert((id, gsi), Arc::downgrade(&line));
        line
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
    hub: Arc<IrqHub>,
    pub bridge: Option<Arc<Mutex<crate::pc::Piix3>>>,
    pub a20: Option<Arc<Mutex<crate::a20::A20>>>,
    fast_a20: Mutex<u8>,
    pub smi: Mutex<crate::smm::SmiControl>,
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
    /// The machine's audio device, when it has one: advanced by the same
    /// running-time clock as the platform timers, so its guest-visible
    /// position never runs ahead of real playback and freezes on pause.
    /// HDA and virtio-snd both sit here.
    pub audio: Mutex<Option<Arc<Mutex<dyn vmlab_hv_devices::audio::AudioClock>>>>,
    secure_smm: bool,
    clock: Mutex<(Instant, bool)>,
    vm: Arc<VmFd>,
}

pub struct ChipsetRegisters(pub Arc<Chipset>);
impl vmlab_hv_devices::Snapshot for ChipsetRegisters {
    fn state_id(&self) -> String {
        if self.0.bridge.is_some() {
            "pc-platform-registers"
        } else {
            "q35-platform-registers"
        }
        .into()
    }
    fn save_state(&self) -> Result<Vec<u8>, vmlab_hv_devices::DeviceError> {
        let mut bytes = self.0.smi.lock().save_state()?;
        if self.0.bridge.is_some() {
            bytes.push(*self.0.fast_a20.lock());
        }
        Ok(bytes)
    }
    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), vmlab_hv_devices::DeviceError> {
        let smi = if self.0.bridge.is_some() {
            if bytes.len() != 13 {
                return Err(vmlab_hv_devices::DeviceError::State(
                    "invalid pc platform state".into(),
                ));
            }
            *self.0.fast_a20.lock() = bytes[12];
            &bytes[..12]
        } else {
            bytes
        };
        self.0.smi.lock().restore_state(smi)?;
        self.0.smi_pending.store(false, Ordering::SeqCst);
        Ok(())
    }
}

impl Chipset {
    pub fn new(
        vm: Arc<VmFd>,
        layout: &MemLayout,
        rtc_base: i64,
        secure_smm: bool,
    ) -> Result<Arc<Self>, VmmError> {
        Self::build(vm, layout, rtc_base, secure_smm, None)
    }

    pub fn pc(
        vm: Arc<VmFd>,
        layout: &MemLayout,
        rtc_base: i64,
        a20: Arc<Mutex<crate::a20::A20>>,
    ) -> Result<Arc<Self>, VmmError> {
        Self::build(vm, layout, rtc_base, false, Some(a20))
    }

    fn build(
        vm: Arc<VmFd>,
        layout: &MemLayout,
        rtc_base: i64,
        secure_smm: bool,
        a20: Option<Arc<Mutex<crate::a20::A20>>>,
    ) -> Result<Arc<Self>, VmmError> {
        install_irq_routes(&vm)?;
        let irq_vm = vm.clone();
        let hub = Arc::new(IrqHub {
            deliver: Box::new(move |gsi, level| {
                if let Err(error) = irq_vm.set_irq_line(gsi, level) {
                    eprintln!("q35 IRQ {gsi}: {error}");
                }
            }),
            sources: Mutex::new(BTreeMap::new()),
            lines: Mutex::new(BTreeMap::new()),
        });
        let outputs = std::array::from_fn(|gsi| hub.line(0, gsi as u32));
        let pirqs = if a20.is_some() {
            PirqRouter::pc(outputs)
        } else {
            PirqRouter::new(outputs)
        };
        let power = Arc::new(Mutex::new(None));
        let error = Arc::new(Mutex::new(None));
        let pending = power.clone();
        let action: Arc<dyn Fn(PowerAction) + Send + Sync> =
            Arc::new(move |action| *pending.lock() = Some(action));
        let lpc = Arc::new(Mutex::new(if a20.is_some() {
            Lpc::piix4(hub.line(1, 9), action)
        } else {
            Lpc::new(hub.line(1, 9), action)
        }));
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
        let a20_output = a20.clone();
        let a20_errors = error.clone();
        let ps2 = Arc::new(Mutex::new(I8042::new(
            hub.line(3, 1),
            hub.line(3, 12),
            Arc::new(move |event| {
                if event == ControllerEvent::Reset {
                    *pending.lock() = Some(PowerAction::Reset);
                }
                if let ControllerEvent::A20(enabled) = event
                    && let Some(a20) = &a20_output
                    && let Err(error) = a20.lock().set_enabled(enabled)
                {
                    *a20_errors.lock() = Some(error.to_string());
                }
            }),
        )));
        let pit_vm = vm.clone();
        let errors = error.clone();
        let hpet = Arc::new(Mutex::new(Hpet::new(
            // HPET indexes name IOAPIC pins. KVM's timer route is GSI 0,
            // shared by PIC IRQ0 and IOAPIC pin 2.
            std::array::from_fn(|gsi| hub.line(4, timer_gsi(gsi as u32))),
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
            if a20.is_some() {
                "piix4-smbus"
            } else {
                "ich9-smbus"
            },
            if a20.is_some() {
                hub.line(5, 9)
            } else {
                PirqRouter::line(
                    &pirqs,
                    vmlab_hv_proto::machine::PciAddress {
                        bus: 0,
                        device: 31,
                        function: 3,
                    },
                    1,
                )
                .map_err(VmmError::Other)?
            },
        )));
        Ok(Arc::new(Self {
            hub,
            bridge: a20
                .as_ref()
                .map(|_| Arc::new(Mutex::new(crate::pc::Piix3::default()))),
            a20,
            fast_a20: Mutex::new(2),
            smi: Mutex::new(crate::smm::SmiControl::default()),
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
            audio: Mutex::new(None),
            secure_smm,
            clock: Mutex::new((Instant::now(), false)),
            vm,
        }))
    }

    pub fn advance(&self) {
        self.clock_step(None);
    }

    pub fn irq_source(&self, id: u32, gsi: u32) -> Arc<dyn IrqLine> {
        self.hub.line(id, gsi)
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
            let audio = self.audio.lock().clone();
            if let Some(device) = audio.as_ref() {
                // A host-stream failure stops that stream (the device
                // raises its error state for the guest); it never stops
                // the machine.
                if let Err(e) = device.lock().advance(Duration::from_nanos(ns)) {
                    eprintln!("vmlab-hv: audio stream error: {e}");
                }
            }
        }
        clock.0 = now;
        if let Some(running) = running
            && running != clock.1
            && let Some(device) = self.audio.lock().as_ref()
        {
            device.lock().set_paused(!running);
        }
        if let Some(running) = running {
            clock.1 = running;
        }
    }

    pub fn sync_pirqs(&self) {
        if let Some(bridge) = &self.bridge {
            let bridge = bridge.lock();
            let mut router = self.pirqs.lock();
            for pirq in 0..4 {
                router.set_route(pirq, bridge.route(pirq));
            }
            return;
        }
        let mut lpc = self.lpc.lock();
        if lpc.config_read(0xa0 / 4) & (1 << 4) != 0 {
            self.smi.lock().lock();
        }
        let mut router = self.pirqs.lock();
        for i in 0..8 {
            router.set_route(i, lpc.pirq_route(i).unwrap_or(0x80));
        }
    }

    pub fn io_read(&self, port: u16, data: &mut [u8]) -> bool {
        self.advance();
        if self.bridge.is_some() {
            if port == 0x92 && data.len() == 1 {
                data[0] = *self.fast_a20.lock();
                return true;
            }
            if let Some(base) = self.lpc.lock().smbus_base()
                && port.wrapping_sub(base) < 16
            {
                self.smbus.lock().read(u64::from(port - base), data);
                return true;
            }
        }
        if let Some((register, offset)) = self.smi_register(port, data.len()) {
            let bytes = self.smi.lock().read(register).unwrap().to_le_bytes();
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
        if let Some(a20) = &self.a20 {
            if port == 0x92 && data.len() == 1 {
                let previous = *self.fast_a20.lock();
                *self.fast_a20.lock() = data[0] & !1;
                if let Err(error) = a20.lock().set_enabled(data[0] & 2 != 0) {
                    *self.error.lock() = Some(error.to_string());
                }
                if previous & 1 == 0 && data[0] & 1 != 0 {
                    *self.power.lock() = Some(PowerAction::Reset);
                }
                return true;
            }
            if let Some(base) = self.lpc.lock().smbus_base()
                && port.wrapping_sub(base) < 16
            {
                let mut smbus = self.smbus.lock();
                smbus.config_write(1, 0, &[1]);
                smbus.config_write(0x40 / 4, 0, &[1]);
                smbus.write(u64::from(port - base), data);
                return true;
            }
            if matches!(port, 0xb2 | 0xb3) && data.len() == 1 {
                let mut lpc = self.lpc.lock();
                lpc.write_port(port, data);
                if port == 0xb2 && lpc.piix4_smi_enabled() {
                    self.smi_pending.store(true, Ordering::SeqCst);
                }
                return true;
            }
        }
        if let Some((register, offset)) = self.smi_register(port, data.len()) {
            if register == 0xb2 && !self.secure_smm {
                self.lpc.lock().write_port(port, data);
            }
            let mut smi = self.smi.lock();
            let mut bytes = if register == 0x34 {
                [0; 4]
            } else {
                smi.read(register).unwrap().to_le_bytes()
            };
            bytes[offset..offset + data.len()].copy_from_slice(data);
            if smi.write(register, u32::from_le_bytes(bytes)) {
                self.smi_pending.store(true, Ordering::SeqCst);
            }
            return true;
        }
        if self.lpc.lock().write_port(port, data) {
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

    fn smi_register(&self, port: u16, len: usize) -> Option<(u16, usize)> {
        if self.bridge.is_some() {
            return None;
        }
        if matches!(port, 0xb2 | 0xb3) && len == 1 {
            return Some((port, 0));
        }
        let lpc = self.lpc.lock();
        let offset = port.wrapping_sub(lpc.pm_base()) as usize;
        if lpc.pm_enabled() && (0x30..0x38).contains(&offset) && len <= 4 - (offset & 3) {
            Some(((offset & !3) as u16, offset & 3))
        } else {
            None
        }
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

fn timer_gsi(pin: u32) -> u32 {
    match pin {
        0 => 2,
        2 => 0,
        _ => pin,
    }
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
        // Each irqchip pin must have a unique GSI for KVM's reverse
        // acknowledgement lookup. Aliasing PIC IRQ0 to GSI 2 loses PIT
        // acknowledgements on GSI 0 and stops delivery after the first tick.
        add(gsi, KVM_IRQCHIP_IOAPIC, timer_gsi(gsi));
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
    let mut table = KvmIrqRouting::new(routes.header.nr as usize)
        .map_err(|e| VmmError::Other(e.to_string()))?;
    table
        .as_mut_slice()
        .copy_from_slice(&routes.entries[..routes.header.nr as usize]);
    vm.set_gsi_routing(&table)
        .map_err(|e| VmmError::ioctl("KVM_SET_GSI_ROUTING(q35)", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_irq_levels_do_not_take_the_hub_lock() {
        let hub = Arc::new(IrqHub {
            deliver: Box::new(|_, _| {}),
            sources: Mutex::new(BTreeMap::new()),
            lines: Mutex::new(BTreeMap::new()),
        });
        let line = hub.line(1, 9);
        for level in [false, true] {
            line.set_level(level);
            let guard = hub.sources.lock();
            let sender = line.clone();
            let (sent, received) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                sender.set_level(level);
                sent.send(()).unwrap();
            });
            let completed = received.recv_timeout(Duration::from_millis(100)).is_ok();
            drop(guard);
            thread.join().unwrap();
            assert!(completed, "unchanged IRQ level waited for the hub lock");
        }
    }

    #[test]
    fn routed_pci_irq_and_uart_share_the_physical_line() {
        let edges = Arc::new(Mutex::new(Vec::new()));
        let sink = edges.clone();
        let hub = Arc::new(IrqHub {
            deliver: Box::new(move |gsi, level| sink.lock().push((gsi, level))),
            sources: Mutex::new(BTreeMap::new()),
            lines: Mutex::new(BTreeMap::new()),
        });
        let uart = hub.line(6, 4);
        let router = PirqRouter::pc(std::array::from_fn(|gsi| hub.line(0, gsi as u32)));
        router.lock().set_route(1, 4);
        let pci = PirqRouter::line(
            &router,
            vmlab_hv_proto::machine::PciAddress {
                bus: 0,
                device: 2,
                function: 0,
            },
            1,
        )
        .unwrap();
        uart.set_level(true);
        pci.set_level(true);
        uart.set_level(false);
        assert_eq!(*edges.lock(), vec![(4, true)]);
        pci.set_level(false);
        assert_eq!(*edges.lock(), vec![(4, true), (4, false)]);
    }

    #[test]
    fn apmc_enables_acpi_without_smm() {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping APMC routing test: /dev/kvm is unavailable");
            return;
        }
        let kvm = crate::kvm::KvmVm::new().unwrap();
        let chipset = Chipset::new(kvm.vm, &MemLayout::q35(64).unwrap(), 0, false).unwrap();
        chipset
            .lpc
            .lock()
            .config_write(0x40 / 4, 0, &0x601u32.to_le_bytes());
        chipset.lpc.lock().config_write(0x44 / 4, 0, &[0x80]);
        assert!(chipset.io_write(0x630, &0x21u32.to_le_bytes()));
        assert!(chipset.io_write(0xb2, &[2]));
        assert!(chipset.smi_pending.load(Ordering::SeqCst));
        let mut control = [0; 2];
        assert!(chipset.io_read(0x604, &mut control));
        assert_eq!(u16::from_le_bytes(control) & 1, 1);
        assert!(chipset.io_write(0xb2, &[3]));
        assert!(chipset.io_read(0x604, &mut control));
        assert_eq!(u16::from_le_bytes(control) & 1, 0);
    }

    #[test]
    fn apmc_leaves_acpi_enable_to_smm_when_available() {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping APMC routing test: /dev/kvm is unavailable");
            return;
        }
        let kvm = crate::kvm::KvmVm::new().unwrap();
        let chipset = Chipset::new(kvm.vm, &MemLayout::q35(64).unwrap(), 0, true).unwrap();
        chipset
            .lpc
            .lock()
            .config_write(0x40 / 4, 0, &0x601u32.to_le_bytes());
        chipset.lpc.lock().config_write(0x44 / 4, 0, &[0x80]);
        assert!(chipset.io_write(0x630, &0x21u32.to_le_bytes()));
        assert!(chipset.io_write(0xb2, &[2]));
        assert!(chipset.smi_pending.load(Ordering::SeqCst));
        let mut command = [0];
        assert!(chipset.io_read(0xb2, &mut command));
        assert_eq!(command, [2]);
        let mut control = [0; 2];
        assert!(chipset.io_read(0x604, &mut control));
        assert_eq!(u16::from_le_bytes(control) & 1, 0);
    }

    #[test]
    fn hpet_legacy_timer_uses_pic_irq0_and_ioapic_pin2() {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping HPET routing test: /dev/kvm is unavailable");
            return;
        }
        let kvm = crate::kvm::KvmVm::new().unwrap();
        let chipset = Chipset::new(kvm.vm.clone(), &MemLayout::q35(64).unwrap(), 0, false).unwrap();
        let mut hpet = chipset.hpet.lock();
        hpet.write(0x100, &4u64.to_le_bytes());
        hpet.write(0x108, &100u64.to_le_bytes());
        hpet.write(0x10, &3u64.to_le_bytes());
        assert_ne!(
            kvm.vm.get_pit2().unwrap().flags & kvm_bindings::KVM_PIT_FLAGS_HPET_LEGACY,
            0
        );
        hpet.advance(1000);
        let mut pic = kvm_bindings::kvm_irqchip {
            chip_id: kvm_bindings::KVM_IRQCHIP_PIC_MASTER,
            ..Default::default()
        };
        kvm.vm.get_irqchip(&mut pic).unwrap();
        // SAFETY: chip_id selects the PIC member of the KVM irqchip union.
        assert_ne!(unsafe { pic.chip.pic.irr } & 1, 0);
        hpet.write(0x10, &0u64.to_le_bytes());
        assert_eq!(
            kvm.vm.get_pit2().unwrap().flags & kvm_bindings::KVM_PIT_FLAGS_HPET_LEGACY,
            0
        );
        assert!(chipset.error.lock().is_none());
        assert_eq!(timer_gsi(2), 0);
        assert_eq!(timer_gsi(0), 2);
    }

    #[test]
    fn pit_keeps_delivering_after_pic_acknowledgements() {
        use kvm_ioctls::VcpuExit;
        use vm_memory::{Bytes, GuestAddress};

        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping PIT delivery test: /dev/kvm is unavailable");
            return;
        }
        let kvm = crate::kvm::KvmVm::new().unwrap();
        let layout = MemLayout::q35(64).unwrap();
        let ram = crate::memory::GuestRam::anonymous(layout.clone()).unwrap();
        crate::memory::register_memslots(&kvm.vm, &ram).unwrap();
        let _chipset = Chipset::new(kvm.vm.clone(), &layout, 0, false).unwrap();
        let mut vcpu = kvm.create_vcpu(0).unwrap();
        let mut sregs = vcpu.get_sregs().unwrap();
        sregs.cs.base = 0;
        sregs.cs.selector = 0;
        vcpu.set_sregs(&sregs).unwrap();
        vcpu.set_regs(&kvm_bindings::kvm_regs {
            rip: 0x1000,
            rsp: 0x8000,
            rflags: 2,
            ..Default::default()
        })
        .unwrap();
        // Initialize both PICs, unmask IRQ0, and program PIT0 for 100 Hz.
        // Polling I/O bounds the host wait even if IRQ0 stops arriving.
        let code = [
            0xfa, 0xb0, 0x11, 0xe6, 0x20, 0xe6, 0xa0, 0xb0, 0x08, 0xe6, 0x21, 0xb0, 0x70, 0xe6,
            0xa1, 0xb0, 0x04, 0xe6, 0x21, 0xb0, 0x02, 0xe6, 0xa1, 0xb0, 0x01, 0xe6, 0x21, 0xe6,
            0xa1, 0xb0, 0xfe, 0xe6, 0x21, 0xb0, 0xff, 0xe6, 0xa1, 0xb0, 0x34, 0xe6, 0x43, 0xb0,
            0x9c, 0xe6, 0x40, 0xb0, 0x2e, 0xe6, 0x40, 0xfb, 0xa0, 0x00, 0x05, 0xe6, 0xe9, 0xeb,
            0xf9,
        ];
        ram.mem.write_slice(&code, GuestAddress(0x1000)).unwrap();
        ram.mem
            .write_slice(&[0x00, 0x11, 0, 0], GuestAddress(8 * 4))
            .unwrap();
        ram.mem
            .write_slice(
                &[
                    0xfe, 0x06, 0x00, 0x05, // inc byte [0x500]
                    0xb0, 0x20, 0xe6, 0x20, 0xcf, // EOI; iret
                ],
                GuestAddress(0x1100),
            )
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        let mut ticks = 0;
        while Instant::now() < deadline && ticks < 5 {
            match vcpu.run().unwrap() {
                VcpuExit::IoOut(0xe9, bytes) => ticks = bytes[0],
                exit => panic!("unexpected PIT test exit: {exit:?}"),
            }
        }
        assert!(
            ticks >= 5,
            "PIT stopped after {ticks} acknowledged interrupts"
        );
    }
    #[test]
    fn unchanged_levels_do_not_repeat_kvm_irq_delivery() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let output = events.clone();
        let hub = Arc::new(IrqHub {
            deliver: Box::new(move |gsi, level| output.lock().push((gsi, level))),
            sources: Mutex::new(BTreeMap::new()),
            lines: Mutex::new(BTreeMap::new()),
        });
        let first = hub.line(1, 9);
        let alias = hub.line(1, 9);
        assert!(Arc::ptr_eq(&first, &alias));
        let second = hub.line(2, 9);
        for _ in 0..65536 {
            first.set_level(false);
        }
        assert!(hub.sources.lock().is_empty());
        first.set_level(true);
        alias.set_level(false);
        alias.set_level(true);
        second.set_level(true);
        first.set_level(false);
        second.set_level(false);
        assert_eq!(
            *events.lock(),
            [(9, true), (9, false), (9, true), (9, false)]
        );
    }
    #[test]
    fn platform_timers_freeze_between_pause_and_resume() {
        let kvm = match crate::kvm::KvmVm::new() {
            Ok(kvm) => kvm,
            Err(error) => {
                eprintln!("skipping timer clock test: {error}");
                return;
            }
        };
        let chipset = Chipset::new(kvm.vm, &MemLayout::q35(64).unwrap(), 0, false).unwrap();
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

    /// A null host and a sink MSI, for driving the HDA model headlessly.
    struct SilentHost;
    impl vmlab_hv_devices::audio::HostAudio for SilentHost {
        fn name(&self) -> &'static str {
            "silent"
        }
        fn open_playback(
            &self,
            _: vmlab_hv_devices::audio::AudioFormat,
        ) -> Result<Box<dyn vmlab_hv_devices::audio::PlaybackSink>, vmlab_hv_devices::DeviceError>
        {
            Ok(Box::new(SilentSink))
        }
        fn open_capture(
            &self,
            _: vmlab_hv_devices::audio::AudioFormat,
        ) -> Result<Box<dyn vmlab_hv_devices::audio::CaptureSource>, vmlab_hv_devices::DeviceError>
        {
            Ok(Box::new(SilentSource))
        }
    }
    struct SilentSink;
    impl vmlab_hv_devices::audio::PlaybackSink for SilentSink {
        fn write_period(&mut self, _: &[u8]) {}
        fn latency_bytes(&self) -> usize {
            0
        }
    }
    struct SilentSource;
    impl vmlab_hv_devices::audio::CaptureSource for SilentSource {
        fn read_period(&mut self, out: &mut [u8]) {
            out.fill(0);
        }
        fn latency_bytes(&self) -> usize {
            0
        }
    }
    struct NoMsi;
    impl vmlab_hv_devices::MsiSink for NoMsi {
        fn signal(&self, _: vmlab_hv_devices::MsiMessage) {}
    }

    #[test]
    fn audio_clock_freezes_between_pause_and_resume() {
        let kvm = match crate::kvm::KvmVm::new() {
            Ok(kvm) => kvm,
            Err(error) => {
                eprintln!("skipping audio clock test: {error}");
                return;
            }
        };
        let chipset = Chipset::new(kvm.vm, &MemLayout::q35(64).unwrap(), 0, false).unwrap();
        let memory =
            vmlab_hv_devices::GuestMemory::from_ranges(&[(vm_memory::GuestAddress(0), 0x1000)])
                .unwrap();
        let hda = Arc::new(Mutex::new(vmlab_hv_devices::hda::Hda::new(
            "hda",
            memory,
            Arc::new(SilentHost),
            Arc::new(NoMsi),
            false,
        )));
        use vmlab_hv_devices::BusDevice;
        hda.lock().write(8, &[1]); // GCTL: the controller leaves reset.
        chipset.audio.lock().replace(hda.clone());
        let wall = || {
            let mut bytes = [0; 4];
            hda.lock().read(0x30, &mut bytes);
            u32::from_le_bytes(bytes)
        };
        let base = chipset.clock.lock().0;
        chipset.clock_step_at(base, Some(true));
        chipset.clock_step_at(base + Duration::from_secs(1), Some(false));
        // The 24 MHz wall clock advanced exactly one running second and
        // then froze with the machine.
        assert_eq!(wall(), 24_000_000);
        chipset.clock_step_at(base + Duration::from_secs(3), Some(true));
        assert_eq!(wall(), 24_000_000);
        chipset.clock_step_at(base + Duration::from_secs(4), None);
        assert_eq!(wall(), 48_000_000);
    }
}
