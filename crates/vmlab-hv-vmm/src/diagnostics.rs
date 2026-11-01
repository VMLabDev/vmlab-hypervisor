//! Crash reports, log and vCPU flight recorders, and timer diagnostics.

use kvm_bindings::{
    KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER, KVM_IRQCHIP_PIC_SLAVE, Msrs, kvm_irqchip,
    kvm_msr_entry,
};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vmlab_hv_devices::BusDevice;

use crate::vm::VmShared;

pub(crate) mod flight;
pub mod report;
pub mod signal;

/// Best-effort state inspection. Busy locks are represented by null rather
/// than waiting for a thread which may itself be reporting a panic.
pub(crate) fn vm_state(shared: &VmShared) -> serde_json::Value {
    use serde_json::json;
    let vcpus = shared.vcpus.try_lock().map(|fds| {
        fds.iter()
            .map(|(id, fd)| {
                let registers = fd.try_lock().map(|mut fd| {
                    json!({
                        "regs":fd.get_regs().ok().map(registers), "sregs":fd.get_sregs().ok().map(special_registers),
                        "mp_state":fd.get_mp_state().ok().map(|state| state.mp_state),
                        "sync_valid_regs":fd.get_kvm_run().kvm_valid_regs,
                    })
                });
                let exits = shared
                    .exit_rings
                    .get(*id as usize)
                    .and_then(|ring| ring.get())
                    .and_then(|ring| ring.snapshot());
                json!({"id":id, "registers":registers, "exits":exits})
            })
            .collect::<Vec<_>>()
    });
    let bars = shared
        .pci_root
        .try_lock()
        .map(|root| root.save_bar_states());
    let timers = shared
        .ram
        .try_lock()
        .and_then(|r| r.clone())
        .map(|ram| timer_state(shared, &ram.mem));
    let smram = shared.mch.try_lock().and_then(|mch| {
        mch.as_ref()
            .and_then(|mch| mch.try_lock().and_then(|mch| mch.diagnostic_smram()))
    });
    let exit_rip = json!({
        "requested":report::capture_exit_rip(),
        "supported":shared.kvm.kvm.check_extension_int(kvm_ioctls::Cap::SyncRegs)
            & kvm_bindings::KVM_SYNC_X86_REGS as i32 != 0,
    });
    json!({"vcpus":vcpus, "exit_rip":exit_rip, "pci_bars":bars, "timers":timers, "mch_smram":smram,
        "usb_attachments":shared.usb_attachments.try_lock().map(|a| a.iter().map(|a| a.id.clone()).collect::<Vec<_>>())})
}

const HV: u32 = 0x4000_0000;

// kvm-bindings serializes its C ABI structures as byte arrays for snapshots.
// Crash reports use named fields so they can be read without an ABI decoder.
fn registers(r: kvm_bindings::kvm_regs) -> serde_json::Value {
    serde_json::json!({
        "rax":r.rax, "rbx":r.rbx, "rcx":r.rcx, "rdx":r.rdx,
        "rsi":r.rsi, "rdi":r.rdi, "rsp":r.rsp, "rbp":r.rbp,
        "r8":r.r8, "r9":r.r9, "r10":r.r10, "r11":r.r11,
        "r12":r.r12, "r13":r.r13, "r14":r.r14, "r15":r.r15,
        "rip":r.rip, "rflags":r.rflags,
    })
}

fn segment(r: kvm_bindings::kvm_segment) -> serde_json::Value {
    serde_json::json!({
        "base":r.base, "limit":r.limit, "selector":r.selector, "type":r.type_,
        "present":r.present, "dpl":r.dpl, "db":r.db, "s":r.s,
        "l":r.l, "g":r.g, "avl":r.avl, "unusable":r.unusable,
    })
}

fn special_registers(r: kvm_bindings::kvm_sregs) -> serde_json::Value {
    serde_json::json!({
        "cs":segment(r.cs), "ds":segment(r.ds), "es":segment(r.es),
        "fs":segment(r.fs), "gs":segment(r.gs), "ss":segment(r.ss),
        "tr":segment(r.tr), "ldt":segment(r.ldt),
        "gdt":{"base":r.gdt.base, "limit":r.gdt.limit},
        "idt":{"base":r.idt.base, "limit":r.idt.limit},
        "cr0":r.cr0, "cr2":r.cr2, "cr3":r.cr3, "cr4":r.cr4, "cr8":r.cr8,
        "efer":r.efer, "apic_base":r.apic_base, "interrupt_bitmap":r.interrupt_bitmap,
    })
}

fn lapic_u32(regs: &[std::os::raw::c_char], offset: usize) -> u32 {
    u32::from_le_bytes(std::array::from_fn(|i| regs[offset + i] as u8))
}

pub(crate) fn timer_state(shared: &VmShared, mem: &GuestMemoryMmap) -> Vec<String> {
    let mut lines = Vec::new();
    macro_rules! record {
        ($($args:tt)*) => { lines.push(format!($($args)*)) };
    }
    let Some(vcpus) = shared.vcpus.try_lock() else {
        return vec!["vCPU registry busy".into()];
    };
    let mut indices = vec![0x10, 0x1b, 0x6e0, HV + 0x20, HV + 0x21, HV + 0x80];
    indices.extend([HV + 0x82, HV + 0x83]);
    indices.extend((0x90..=0x9f).map(|n| HV + n));
    indices.extend((0xb0..=0xb7).map(|n| HV + n));
    for (id, fd) in vcpus.iter() {
        let Some(fd) = fd.try_lock() else {
            lines.push(format!("vcpu {id}: busy"));
            continue;
        };
        if let Ok(regs) = fd.get_regs() {
            record!(
                "timer vcpu {id} registers: rip={:#x} rflags={:#x}",
                regs.rip,
                regs.rflags
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
        record!("timer vcpu {id} MSRs: {}", rendered.join(" "));
        if let Ok(lapic) = fd.get_lapic() {
            let r = |offset| lapic_u32(&lapic.regs, offset);
            let bank = |base: usize| -> String {
                (0..8)
                    .rev()
                    .map(|i| format!("{:08x}", r(base + i * 0x10)))
                    .collect()
            };
            record!(
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
            record!(
                "timer vcpu {id} events: interrupt={:?} nmi={:?} exception={:?}",
                events.interrupt,
                events.nmi,
                events.exception
            );
        }
        if let Ok(mp) = fd.get_mp_state() {
            record!("timer vcpu {id} mp={}", mp.mp_state);
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
            record!("timer vcpu {id} SIMP slots: {}", slots.join(" "));
        }
    }
    let vm = &shared.kvm.vm;
    if let Ok(pit) = vm.get_pit2() {
        record!("timer PIT flags={:#x}", pit.flags);
        for (i, channel) in pit.channels.iter().enumerate() {
            record!(
                "timer PIT {i}: count={} mode={} gate={} latched={}",
                channel.count,
                channel.mode,
                channel.gate,
                channel.count_latched
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
            record!(
                "timer PIC {name}: irr={:#x} imr={:#x} isr={:#x} base={:#x} elcr={:#x}",
                pic.irr,
                pic.imr,
                pic.isr,
                pic.irq_base,
                pic.elcr
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
        record!(
            "timer IOAPIC irr={:#x} unmasked {}",
            ioapic.irr,
            pins.join(" ")
        );
    }
    if let Some(chipset_guard) = shared.chipset.try_lock()
        && let Some(chipset) = chipset_guard.as_ref()
        && let Some(mut hpet) = chipset.hpet.try_lock()
    {
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
        record!(
            "timer HPET config={:#x} status={:#x} counter={:#x} timers {}",
            read(0x10),
            read(0x20),
            read(0xf0),
            timers.join(" ")
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_have_named_fields_instead_of_abi_byte_arrays() {
        let regs = registers(kvm_bindings::kvm_regs {
            rip: 0xfff0,
            rax: 0x1234,
            ..Default::default()
        });
        assert_eq!(regs["rip"], 0xfff0);
        assert_eq!(regs["rax"], 0x1234);
        assert_eq!(regs.as_object().unwrap().len(), 18);
        let sregs = special_registers(kvm_bindings::kvm_sregs {
            cr0: 0x6000_0010,
            cs: kvm_bindings::kvm_segment {
                base: 0xffff_0000,
                selector: 0xf000,
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(sregs["cr0"], 0x6000_0010u64);
        assert_eq!(sregs["cs"]["base"], 0xffff_0000u64);
        assert_eq!(sregs["cs"]["selector"], 0xf000);
        assert!(sregs["gdt"].is_object());
    }
}
