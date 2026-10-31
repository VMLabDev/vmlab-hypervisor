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
                let mut header = [0u8; 8];
                if mem
                    .read_slice(&mut header, GuestAddress((simp & !0xfff) + sint * 256))
                    .is_ok()
                    && header != [0; 8]
                {
                    slots.push(format!("{sint}:{header:02x?}"));
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

/// Investigation aid: `VMLAB_HV_DIAG_RESUME_HOOK=eom` writes the SynIC EOM
/// register on every vCPU at resume; `stimer` rewrites each enabled
/// synthetic timer, restarting it from the current reference time.
pub(crate) fn resume_hook(shared: &VmShared) {
    let Some(hook) = std::env::var_os("VMLAB_HV_DIAG_RESUME_HOOK") else {
        return;
    };
    let one = |index, data| {
        Msrs::from_entries(&[kvm_msr_entry {
            index,
            data,
            ..Default::default()
        }])
        .expect("one MSR")
    };
    for (id, fd) in shared.vcpus.lock().iter() {
        let Some(fd) = fd.try_lock_for(std::time::Duration::from_secs(1)) else {
            eprintln!("resume hook: vcpu {id} is running");
            continue;
        };
        let mut reference = one(HV + 0x20, 0);
        let _ = fd.get_msrs(&mut reference);
        eprintln!(
            "resume hook {hook:?}: vcpu {id} reference time {:#x}",
            reference.as_slice()[0].data
        );
        if hook == "eom" {
            let result = fd.set_msrs(&one(HV + 0x84, 0));
            eprintln!("resume hook: vcpu {id} EOM {result:?}");
            continue;
        }
        for timer in 0..4 {
            let mut config = one(HV + 0xb0 + timer * 2, 0);
            let mut count = one(HV + 0xb1 + timer * 2, 0);
            if fd.get_msrs(&mut config).is_err() || fd.get_msrs(&mut count).is_err() {
                continue;
            }
            if config.as_slice()[0].data & 1 == 0 {
                continue;
            }
            let a = fd.set_msrs(&config);
            let b = fd.set_msrs(&count);
            eprintln!(
                "resume hook: vcpu {id} stimer {timer} config {:#x} count {:#x}: {a:?} {b:?}",
                config.as_slice()[0].data,
                count.as_slice()[0].data
            );
        }
    }
}

/// Investigation aid enabled by `VMLAB_HV_DIAG_TIME_SAMPLE_MS`: interrupts
/// vCPU 0 periodically and reports discontinuities between KVM's Hyper-V
/// reference counter, the guest's reference TSC page and the KVM clock, and
/// every change to the synthetic timer 0 and SynIC registers.
pub(crate) struct TimeSampler {
    last: Option<[u64; 12]>,
    due: std::sync::Arc<std::sync::atomic::AtomicBool>,
    heartbeat: std::time::Instant,
    verbose_until: Option<std::time::Instant>,
}

impl TimeSampler {
    pub(crate) fn start() -> Option<Self> {
        let period: u64 = std::env::var("VMLAB_HV_DIAG_TIME_SAMPLE_MS")
            .ok()?
            .parse()
            .ok()?;
        // SAFETY: pthread_self has no preconditions.
        let tid = unsafe { libc::pthread_self() } as usize;
        let due = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::downgrade(&due);
        std::thread::spawn(move || {
            while let Some(flag) = flag.upgrade() {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                drop(flag);
                // SAFETY: the vCPU thread owns the Arc; once it drops the
                // sampler the upgrade above fails before another signal.
                unsafe { libc::pthread_kill(tid as libc::pthread_t, libc::SIGUSR1) };
                std::thread::sleep(std::time::Duration::from_millis(period));
            }
        });
        Some(Self {
            last: None,
            due,
            heartbeat: std::time::Instant::now(),
            verbose_until: None,
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
            let mut slot = [0u8; 8];
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
                "time trace: host={:?} ref={reference} stimer0={:#x}/{} slot3={:02x?} rip={:#x} if={} mp={mp} tpr={tpr:#x} isr={isr} irr_d1={irr_d1}",
                std::time::Instant::now(),
                v[3],
                v[4],
                &slot[..6],
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
                "time sample [{}]: tsc={tsc:#x} ref={reference} kvmclock/100={} skew={} refpage={page_msr:#x} stimer0={:#x}/{} simp={:#x} sint3={:#x} page seq={seq} scale={scale:#x} offset={offset:#x} pageref={page_ref}",
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

/// Investigation aid enabled by `VMLAB_HV_DIAG_EXIT_STATS`: counts exits by
/// kind during the first two seconds of execution and prints the totals.
pub(crate) fn exit_stats(id: u8, result: &Result<kvm_ioctls::VcpuExit<'_>, kvm_ioctls::Error>) {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock};
    static ON: OnceLock<bool> = OnceLock::new();
    type Stats = (std::time::Instant, BTreeMap<String, u64>, u32);
    static STATS: Mutex<Option<Stats>> = Mutex::new(None);
    if !*ON.get_or_init(|| std::env::var_os("VMLAB_HV_DIAG_EXIT_STATS").is_some()) {
        return;
    }
    let key = match result {
        Ok(kvm_ioctls::VcpuExit::IoIn(port, _)) => format!("in {port:#x}"),
        Ok(kvm_ioctls::VcpuExit::IoOut(port, _)) => format!("out {port:#x}"),
        Ok(kvm_ioctls::VcpuExit::MmioRead(addr, _)) if *addr < 0xff00_0000 => {
            format!("mmio-r {addr:#x}")
        }
        Ok(kvm_ioctls::VcpuExit::MmioRead(addr, _)) => format!("mmio-r {:#x}", addr >> 16 << 16),
        Ok(kvm_ioctls::VcpuExit::MmioWrite(addr, _)) => format!("mmio-w {:#x}", addr >> 16 << 16),
        Ok(other) => format!("{other:?}").chars().take(12).collect(),
        Err(e) => format!("err {}", e.errno()),
    };
    let mut stats = STATS.lock().unwrap();
    let (start, map, printed) =
        stats.get_or_insert_with(|| (std::time::Instant::now(), BTreeMap::new(), 0));
    *map.entry(format!("cpu{id} {key}")).or_default() += 1;
    let elapsed = start.elapsed().as_millis() as u32;
    if elapsed / 100 > *printed && *printed < 12 {
        *printed = elapsed / 100;
        let snapshot = std::mem::take(map);
        let mut top: Vec<_> = snapshot.iter().collect();
        top.sort_by(|a, b| b.1.cmp(a.1));
        let top: Vec<_> = top
            .iter()
            .take(12)
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        eprintln!("exit stats at {elapsed} ms: {}", top.join(" "));
    }
}

/// Investigation aid enabled by `VMLAB_HV_DIAG_PS2`: logs i8042 accesses,
/// collapsing repeated identical status reads.
pub(crate) fn ps2_trace(port: u16, data: &[u8], write: bool) {
    use std::sync::{Mutex, OnceLock};
    static ON: OnceLock<Option<std::time::Instant>> = OnceLock::new();
    static LAST: Mutex<(String, u32)> = Mutex::new((String::new(), 0));
    let Some(start) =
        ON.get_or_init(|| std::env::var_os("VMLAB_HV_DIAG_PS2").map(|_| std::time::Instant::now()))
    else {
        return;
    };
    if !matches!(port, 0x60 | 0x64) {
        return;
    }
    let line = format!("{} {port:#x} {data:x?}", if write { "out" } else { "in" });
    let mut last = LAST.lock().unwrap();
    if last.0 == line {
        last.1 += 1;
        return;
    }
    if last.1 > 0 {
        eprintln!("ps2   (x{} more)", last.1);
    }
    eprintln!("ps2 {:>4} ms {line}", start.elapsed().as_millis());
    *last = (line, 0);
}
