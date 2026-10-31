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
