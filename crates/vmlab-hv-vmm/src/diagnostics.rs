//! Timer and interrupt state printed at exit when `VMLAB_HV_DIAGNOSTICS` is
//! set: the Hyper-V SynIC and synthetic timers, each local APIC timer, the
//! in-kernel PIT, PIC and IOAPIC, and the HPET.

use kvm_bindings::{
    KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER, KVM_IRQCHIP_PIC_SLAVE, Msrs, kvm_irqchip,
    kvm_msr_entry,
};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vmlab_hv_devices::BusDevice;

use crate::vm::VmShared;

const HV: u32 = 0x4000_0000;

fn lapic_u32(regs: &[std::os::raw::c_char], offset: usize) -> u32 {
    u32::from_le_bytes(std::array::from_fn(|i| regs[offset + i] as u8))
}

pub(crate) fn dump_timer_state(shared: &VmShared, mem: &GuestMemoryMmap) {
    let mut indices = vec![0x10, 0x1b, 0x6e0, HV + 0x20, HV + 0x21, HV + 0x80];
    indices.extend([HV + 0x82, HV + 0x83]);
    indices.extend((0x90..=0x9f).map(|n| HV + n));
    indices.extend((0xb0..=0xb7).map(|n| HV + n));
    for (id, fd) in shared.vcpus.lock().iter() {
        let fd = fd.lock();
        if let Ok(regs) = fd.get_regs() {
            eprintln!(
                "timer vcpu {id} registers: rip={:#x} rflags={:#x}",
                regs.rip, regs.rflags
            );
        }
        let mut found = Vec::new();
        for &index in &indices {
            let mut msrs = Msrs::from_entries(&[kvm_msr_entry {
                index,
                ..Default::default()
            }])
            .expect("one MSR");
            if fd.get_msrs(&mut msrs).is_ok_and(|n| n == 1) {
                found.push((index, msrs.as_slice()[0].data));
            }
        }
        let rendered: Vec<_> = found
            .iter()
            .filter(|(_, value)| *value != 0)
            .map(|(index, value)| format!("{index:#x}={value:#x}"))
            .collect();
        eprintln!("timer vcpu {id} MSRs: {}", rendered.join(" "));
        if let Ok(lapic) = fd.get_lapic() {
            let r = |offset| lapic_u32(&lapic.regs, offset);
            let bank = |base: usize| -> String {
                (0..8)
                    .rev()
                    .map(|i| format!("{:08x}", r(base + i * 0x10)))
                    .collect()
            };
            eprintln!(
                "timer vcpu {id} LAPIC: id={:#x} tpr={:#x} svr={:#x} lvtt={:#x} tmict={:#x} tmcct={:#x} tdcr={:#x} lint0={:#x} lint1={:#x} isr={} irr={}",
                r(0x20),
                r(0x80),
                r(0xf0),
                r(0x320),
                r(0x380),
                r(0x390),
                r(0x3e0),
                r(0x350),
                r(0x360),
                bank(0x100),
                bank(0x200),
            );
        }
        if let Ok(events) = fd.get_vcpu_events() {
            eprintln!(
                "timer vcpu {id} events: interrupt={:?} nmi={:?} exception={:?}",
                events.interrupt, events.nmi, events.exception
            );
        }
        if let Ok(mp) = fd.get_mp_state() {
            eprintln!("timer vcpu {id} mp={}", mp.mp_state);
        }
        // Message slot headers for each SINT in an enabled SIMP page.
        if let Some(&(_, simp)) = found.iter().find(|(index, _)| *index == HV + 0x83)
            && simp & 1 != 0
        {
            let mut slots = Vec::new();
            for sint in 0..16u64 {
                let mut header = [0u8; 40];
                if mem
                    .read_slice(&mut header, GuestAddress((simp & !0xfff) + sint * 256))
                    .is_ok()
                    && header != [0; 40]
                {
                    let expires = u64::from_le_bytes(header[24..32].try_into().unwrap());
                    let delivered = u64::from_le_bytes(header[32..40].try_into().unwrap());
                    slots.push(format!(
                        "{sint}:{:02x?} expires={expires} delivered={delivered}",
                        &header[..8]
                    ));
                }
            }
            eprintln!("timer vcpu {id} SIMP slots: {}", slots.join(" "));
        }
    }
    let vm = &shared.kvm.vm;
    if let Ok(pit) = vm.get_pit2() {
        eprintln!("timer PIT flags={:#x}", pit.flags);
        for (i, channel) in pit.channels.iter().enumerate() {
            eprintln!(
                "timer PIT {i}: count={} mode={} gate={} latched={}",
                channel.count, channel.mode, channel.gate, channel.count_latched
            );
        }
    }
    for (name, chip) in [
        ("master", KVM_IRQCHIP_PIC_MASTER),
        ("slave", KVM_IRQCHIP_PIC_SLAVE),
    ] {
        let mut state = kvm_irqchip {
            chip_id: chip,
            ..Default::default()
        };
        if vm.get_irqchip(&mut state).is_ok() {
            // SAFETY: the PIC chip ids select the pic member of the union.
            let pic = unsafe { state.chip.pic };
            eprintln!(
                "timer PIC {name}: irr={:#x} imr={:#x} isr={:#x} base={:#x} elcr={:#x}",
                pic.irr, pic.imr, pic.isr, pic.irq_base, pic.elcr
            );
        }
    }
    let mut state = kvm_irqchip {
        chip_id: KVM_IRQCHIP_IOAPIC,
        ..Default::default()
    };
    if vm.get_irqchip(&mut state).is_ok() {
        // SAFETY: KVM_IRQCHIP_IOAPIC selects the ioapic member of the union.
        let ioapic = unsafe { state.chip.ioapic };
        let pins: Vec<_> = ioapic
            .redirtbl
            .iter()
            .enumerate()
            // SAFETY: each redirection entry is a plain 64-bit register.
            .map(|(pin, entry)| (pin, unsafe { entry.bits }))
            .filter(|(_, bits)| bits & (1 << 16) == 0)
            .map(|(pin, bits)| format!("{pin}:{bits:#x}"))
            .collect();
        eprintln!(
            "timer IOAPIC irr={:#x} unmasked {}",
            ioapic.irr,
            pins.join(" ")
        );
    }
    if let Some(chipset) = shared.chipset.lock().as_ref() {
        let mut hpet = chipset.hpet.lock();
        let mut read = |offset| {
            let mut data = [0; 8];
            hpet.read(offset, &mut data);
            u64::from_le_bytes(data)
        };
        let timers: Vec<_> = (0..3)
            .map(|i| {
                format!(
                    "{i}:{:#x}/{:#x}",
                    read(0x100 + i * 0x20),
                    read(0x108 + i * 0x20)
                )
            })
            .collect();
        eprintln!(
            "timer HPET config={:#x} status={:#x} counter={:#x} timers {}",
            read(0x10),
            read(0x20),
            read(0xf0),
            timers.join(" ")
        );
    }
}

/// Investigation aid enabled by `VMLAB_HV_DIAG_TIME_SAMPLE_MS`: interrupts
/// each vCPU periodically and reports discontinuities between KVM's Hyper-V
/// reference counter, the guest's reference TSC page and the KVM clock, and
/// every change to the synthetic timer 0 and SynIC registers.
pub(crate) struct TimeSampler {
    id: u8,
    last: Option<[u64; 12]>,
    due: std::sync::Arc<std::sync::atomic::AtomicBool>,
    heartbeat: std::time::Instant,
    verbose_until: Option<std::time::Instant>,
    stop: std::sync::mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TimeSampler {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl TimeSampler {
    pub(crate) fn start(id: u8) -> Option<Self> {
        let period: u64 = std::env::var("VMLAB_HV_DIAG_TIME_SAMPLE_MS")
            .ok()?
            .parse()
            .ok()?;
        // SAFETY: pthread_self has no preconditions.
        let tid = unsafe { libc::pthread_self() } as usize;
        let due = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = due.clone();
        let (stop, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            loop {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                // SAFETY: the vCPU joins this worker before exiting, so its
                // pthread ID cannot be reused while signals are sent.
                unsafe { libc::pthread_kill(tid as libc::pthread_t, libc::SIGUSR1) };
                if receiver.recv_timeout(std::time::Duration::from_millis(period))
                    != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                {
                    break;
                }
            }
        });
        Some(Self {
            id,
            last: None,
            due,
            heartbeat: std::time::Instant::now(),
            verbose_until: None,
            stop,
            worker: Some(worker),
        })
    }

    pub(crate) fn sample(&mut self, fd: &kvm_ioctls::VcpuFd, shared: &VmShared) {
        if !self.due.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let indices = [
            0x10,
            HV + 0x20,
            HV + 0x21,
            HV + 0xb0,
            HV + 0xb1,
            HV + 0x83,
            HV + 0x93,
        ];
        let mut msrs = Msrs::from_entries(
            &indices
                .iter()
                .map(|&index| kvm_msr_entry {
                    index,
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )
        .expect("MSR list");
        if fd.get_msrs(&mut msrs).ok() != Some(indices.len()) {
            return;
        }
        let v: Vec<u64> = msrs.as_slice().iter().map(|e| e.data).collect();
        let (tsc, reference, page_msr) = (v[0], v[1], v[2]);
        let clock = shared.kvm.vm.get_clock().map(|c| c.clock).unwrap_or(0);
        let mut page = [0u8; 24];
        let mut page_ref = 0;
        let (mut seq, mut scale, mut offset) = (0, 0, 0);
        if page_msr & 1 != 0
            && let Some(ram) = shared.ram()
            && ram
                .mem
                .read_slice(&mut page, GuestAddress(page_msr & !0xfff))
                .is_ok()
        {
            seq = u32::from_le_bytes(page[0..4].try_into().unwrap()) as u64;
            scale = u64::from_le_bytes(page[8..16].try_into().unwrap());
            offset = u64::from_le_bytes(page[16..24].try_into().unwrap());
            page_ref = ((u128::from(tsc) * u128::from(scale)) >> 64) as u64 + offset;
        }
        let now = [
            tsc,
            reference,
            page_msr,
            v[3],
            v[4],
            v[5],
            v[6],
            clock / 100,
            seq,
            scale,
            offset,
            page_ref,
        ];
        let mut reasons = Vec::new();
        match self.last {
            None => reasons.push("first"),
            Some(last) => {
                if reference < last[1] {
                    reasons.push("reference went back");
                }
                let skew = |s: &[u64; 12]| s[1] as i64 - s[7] as i64;
                if (skew(&now) - skew(&last)).abs() > 10_000 {
                    reasons.push("reference/kvmclock skew moved");
                }
                if now[2..7] != last[2..7] {
                    reasons.push("register changed");
                }
                if now[8..11] != last[8..11] {
                    reasons.push("TSC page changed");
                }
            }
        }
        if seq != 0 && (page_ref as i64 - reference as i64).abs() > 10_000 {
            reasons.push("TSC page disagrees with reference");
        }
        if reasons.contains(&"reference went back") || reasons.contains(&"first") {
            self.verbose_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(12));
        }
        let verbose = self
            .verbose_until
            .is_some_and(|until| std::time::Instant::now() < until);
        if verbose {
            let regs = fd.get_regs().ok();
            let lapic = fd.get_lapic().ok();
            let mp = fd.get_mp_state().map(|m| m.mp_state).unwrap_or(99);
            let mut slot = [0u8; 40];
            if v[5] & 1 != 0
                && let Some(ram) = shared.ram()
            {
                let _ = ram
                    .mem
                    .read_slice(&mut slot, GuestAddress((v[5] & !0xfff) + 3 * 256));
            }
            let (tpr, isr, irr_d1) = lapic.map_or((0, false, false), |l| {
                let r = |o| lapic_u32(&l.regs, o);
                (
                    r(0x80),
                    (0..8).any(|i| r(0x100 + i * 0x10) != 0),
                    r(0x200 + 6 * 0x10) & (1 << 17) != 0,
                )
            });
            eprintln!(
                "time trace: vcpu={} host={:?} ref={reference} stimer0={:#x}/{} slot3={:02x?} delivered={} rip={:#x} if={} mp={mp} tpr={tpr:#x} isr={isr} irr_d1={irr_d1}",
                self.id,
                std::time::Instant::now(),
                v[3],
                v[4],
                &slot[..6],
                u64::from_le_bytes(slot[32..40].try_into().unwrap()),
                regs.map_or(0, |r| r.rip),
                regs.map_or(0, |r| (r.rflags >> 9) & 1),
            );
        }
        if reasons.is_empty() && self.heartbeat.elapsed() > std::time::Duration::from_secs(10) {
            reasons.push("heartbeat");
        }
        if !reasons.is_empty() {
            self.heartbeat = std::time::Instant::now();
            eprintln!(
                "time sample vcpu={} [{}]: tsc={tsc:#x} ref={reference} kvmclock/100={} skew={} refpage={page_msr:#x} stimer0={:#x}/{} simp={:#x} sint3={:#x} page seq={seq} scale={scale:#x} offset={offset:#x} pageref={page_ref}",
                self.id,
                reasons.join(", "),
                now[7],
                reference as i64 - now[7] as i64,
                now[3],
                now[4],
                now[5],
                now[6],
            );
        }
        self.last = Some(now);
    }
}
