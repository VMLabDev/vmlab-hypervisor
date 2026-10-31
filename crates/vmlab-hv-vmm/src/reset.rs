//! In-place reset, serviced outside vCPU threads after deferred I/O completes.

use std::sync::Arc;

use parking_lot::Mutex;
use vmlab_hv_devices::{DeviceError, Resettable, Snapshot};
use vmlab_hv_proto::machine::{Boot, MachineDescription};

use crate::snapshot::{DeviceNode, SavedSections};
use crate::vm::VmShared;

struct PowerOnDevice {
    device: Arc<Mutex<dyn Snapshot>>,
    state: Vec<u8>,
}

impl Resettable for PowerOnDevice {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.device.lock().restore_state(&self.state)
    }
}

enum ResetNode {
    Plain(PowerOnDevice),
    Native(Arc<Mutex<dyn crate::snapshot::ResetSnapshot>>),
    Pci(Arc<Mutex<dyn vmlab_hv_devices::PciSnapshot>>),
}

impl Resettable for ResetNode {
    fn reset(&mut self) -> Result<(), DeviceError> {
        match self {
            Self::Plain(device) => device.reset(),
            Self::Native(device) => device.lock().reset(),
            Self::Pci(device) => device.lock().reset(),
        }
    }
}

pub(crate) struct ResetState {
    kernel: SavedSections,
    devices: Vec<ResetNode>,
    extra: Vec<(u8, kvm_bindings::kvm_debugregs, kvm_bindings::kvm_xcrs)>,
    msrs: Vec<(u8, Vec<kvm_bindings::kvm_msr_entry>)>,
}

impl ResetState {
    pub(crate) fn capture(shared: &VmShared) -> Result<Self, String> {
        let mut sections = Vec::new();
        crate::snapshot::save_kernel_state(shared, &mut sections).map_err(|e| e.to_string())?;
        let mut kernel = SavedSections::default();
        for (key, bytes) in sections {
            // Reset starts a new TSC epoch, rather than migrating the saved
            // TSC/reference-clock relation. Its MSR writes synchronize at zero.
            if key != "tsc-offsets" {
                kernel.insert(key, bytes);
            }
        }
        let mut devices = Vec::new();
        if let Some(registry) = shared.snapshot_registry() {
            for node in &registry.nodes {
                devices.push(match node {
                    DeviceNode::Plain(device) => ResetNode::Plain(PowerOnDevice {
                        state: device.lock().save_state().map_err(|e| e.to_string())?,
                        device: device.clone(),
                    }),
                    DeviceNode::Resettable(device) => ResetNode::Native(device.clone()),
                    DeviceNode::Virtio(device) => ResetNode::Pci(device.clone()),
                });
            }
        }
        let mut extra = Vec::new();
        let mut msrs = Vec::new();
        let indices = shared
            .kvm
            .kvm
            .get_msr_index_list()
            .map_err(|e| e.to_string())?;
        for (id, fd) in shared.vcpus.lock().iter() {
            let fd = fd.lock();
            extra.push((
                *id,
                fd.get_debug_regs().map_err(|e| e.to_string())?,
                fd.get_xcrs().map_err(|e| e.to_string())?,
            ));
            let mut initial = Vec::new();
            for &index in indices.as_slice() {
                let mut entry = kvm_bindings::Msrs::from_entries(&[kvm_bindings::kvm_msr_entry {
                    index,
                    ..Default::default()
                }])
                .map_err(|e| e.to_string())?;
                // KVM's system-wide list includes optional MSRs unavailable
                // under this vCPU's CPUID policy. A short read denotes those.
                if fd.get_msrs(&mut entry).map_err(|e| e.to_string())? == 1 {
                    initial.push(entry.as_slice()[0]);
                }
            }
            msrs.push((*id, initial));
        }
        Ok(Self {
            kernel,
            devices,
            extra,
            msrs,
        })
    }

    pub(crate) fn reset(
        &mut self,
        shared: &VmShared,
        desc: Option<&MachineDescription>,
    ) -> Result<(), String> {
        for node in &mut self.devices {
            node.reset().map_err(|e| e.to_string())?;
        }
        for device in shared.vfio.lock().iter() {
            device.lock().reset()?;
        }
        shared.reset_platform();
        if let Some(MachineDescription {
            boot:
                Boot::Kernel {
                    kernel,
                    initrd,
                    cmdline,
                },
            vcpus,
            ..
        }) = desc
        {
            let ram = shared.ram().ok_or("reset has no guest RAM")?;
            let layout = ram.layout();
            let acpi = crate::acpi::install(
                &ram.mem,
                &crate::acpi::AcpiConfig {
                    vcpus: *vcpus,
                    layout,
                },
            )
            .map_err(|e| e.to_string())?;
            crate::loader::load(
                &ram.mem,
                kernel,
                initrd.as_deref(),
                cmdline,
                &crate::e820::build_e820(layout),
                Some(acpi.rsdp),
                layout.low_ram_top(),
            )
            .map_err(|e| e.to_string())?;
        }
        // RTC time is battery-backed. Keep the KVM clock monotonic rather than
        // rolling it back to the construction instant on every reboot.
        let clock = shared.kvm.vm.get_clock().map_err(|e| e.to_string())?;
        self.kernel.insert(
            "kvmclock".into(),
            serde_json::to_vec(&clock).map_err(|e| e.to_string())?,
        );
        for (_, fd) in shared.vcpus.lock().iter() {
            let fd = fd.lock();
            let mut sregs = fd.get_sregs().map_err(|e| e.to_string())?;
            if sregs.cr0 & 1 == 0 {
                // An AMD hardware SHUTDOWN can leave cached execution state
                // behind despite GET_SREGS already showing reset CR0. Force a
                // mode transition before installing the saved boot state.
                sregs.cr0 |= 1;
                fd.set_sregs(&sregs).map_err(|e| e.to_string())?;
            }
        }
        for (id, entries) in &self.msrs {
            let cpus = shared.vcpus.lock();
            let fd = cpus
                .iter()
                .find(|(vcpu, _)| vcpu == id)
                .ok_or("reset lost vCPU")?
                .1
                .lock();
            let state = kvm_bindings::Msrs::from_entries(entries).map_err(|e| e.to_string())?;
            let written = fd.set_msrs(&state).map_err(|e| e.to_string())?;
            if written != entries.len() {
                return Err(format!(
                    "vcpu {id}: resetting MSR {:#x} failed",
                    entries[written].index
                ));
            }
        }
        crate::snapshot::restore_kernel_state(shared, &self.kernel).map_err(|e| e.to_string())?;
        shared.restore_pci_bars(self.kernel.get("pci-root").ok_or("reset lacks PCI state")?)?;
        for (id, debug, xcrs) in &self.extra {
            let fds = shared.vcpus.lock();
            let fd = fds
                .iter()
                .find(|(vcpu, _)| vcpu == id)
                .ok_or("reset lost vCPU")?
                .1
                .lock();
            fd.set_debug_regs(debug).map_err(|e| e.to_string())?;
            fd.set_xcrs(xcrs).map_err(|e| e.to_string())?;
            let mp = serde_json::from_slice(
                self.kernel
                    .get(&format!("vcpu{id}/mp_state"))
                    .ok_or("reset lacks MP state")?,
            )
            .map_err(|e| e.to_string())?;
            fd.set_mp_state(mp).map_err(|e| e.to_string())?;
        }
        shared.sync_vga_mapping().map_err(|e| e.to_string())?;
        Ok(())
    }
}
