//! Snapshot orchestration (D14, D31, D32): the machine state the hypervisor
//! saves beside a qcow2 internal snapshot, and the save/load/delete
//! operations behind the control commands.
//!
//! The vmstate stream itself lives in [`format`]. This module owns what a
//! built machine registers for snapshots: the writable disks (every one
//! takes the internal snapshot; the one marked `vmstate: true` carries RAM
//! and device state) and every device with guest-visible state, in machine
//! order.
//!
//! Save quiesces the machine, captures device sections (a vhost-user-fs
//! device quiesces its daemon in its own save), the in-kernel irqchip, PIT
//! and clock, the PCI root's BAR state, every vCPU, and then streams RAM
//! into the vmstate disk's vmstate area — run-length coded, so nothing
//! large is buffered on the host. Load reverts every writable disk, reads
//! the vmstate back in one piece (its size is recorded in the snapshot),
//! restores RAM, then kernel state, then devices, and leaves the machine in
//! the run state the snapshot recorded.

pub mod format;

use std::sync::Arc;
use std::time::Duration;

use kvm_bindings::{
    KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER, KVM_IRQCHIP_PIC_SLAVE, Msrs, kvm_clock_data,
    kvm_fpu, kvm_irqchip, kvm_lapic_state, kvm_mp_state, kvm_msr_entry, kvm_pit_state2, kvm_regs,
    kvm_sregs, kvm_vcpu_events, kvm_xsave,
};
use kvm_ioctls::VcpuFd;
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use vm_memory::{Bytes, GuestAddress};
use vmlab_disk::Error as DiskError;
use vmlab_hv_devices::{PciSnapshot, Snapshot};

use crate::snapshot::format::FormatError;
use crate::vm::VmHandle;

/// How long a snapshot waits for every vCPU to leave KVM_RUN.
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(10);

/// Architectural and KVM clock MSRs migrated through KVM_GET_MSRS.
/// EFER, the APIC base and the PAT ride in `sregs`. Restoring IA32_TSC and
/// the pvclock registrations keeps guest TSC interpolation on the saved
/// timeline after RAM and KVM_SET_CLOCK have been restored.
const MIGRATABLE_MSRS: &[u32] = &[
    0x0000_0010, // IA32_TSC
    0x0000_0175, // IA32_SYSENTER_ESP
    0x0000_0176, // IA32_SYSENTER_EIP
    0xC000_0081, // IA32_STAR
    0xC000_0082, // IA32_LSTAR
    0xC000_0083, // IA32_CSTAR
    0xC000_0084, // IA32_FMASK
    0xC000_0100, // IA32_FS_BASE
    0xC000_0101, // IA32_GS_BASE
    0xC000_0102, // IA32_KERNEL_GS_BASE
    0xC000_0103, // IA32_TSC_AUX
    0x4b56_4d00, // MSR_KVM_WALL_CLOCK_NEW
    0x4b56_4d01, // MSR_KVM_SYSTEM_TIME_NEW
];

/// Why a snapshot operation failed, mapped onto control error codes.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Incompatible(String),
    #[error("{0}")]
    Internal(String),
}

impl SnapshotError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, SnapshotError::NotFound(_))
    }

    fn from_disk(op: &str, disk: &str, err: DiskError) -> Self {
        match err {
            DiskError::SnapshotNotFound(name) => {
                SnapshotError::NotFound(format!("disk `{disk}` has no snapshot {name:?}"))
            }
            DiskError::Unsupported(reason) => {
                SnapshotError::Unsupported(format!("disk `{disk}`: {reason}"))
            }
            other => SnapshotError::Internal(format!("{op} on disk `{disk}`: {other}")),
        }
    }

    fn from_format(err: FormatError) -> Self {
        match err {
            FormatError::VersionMismatch { .. } | FormatError::DigestMismatch { .. } => {
                SnapshotError::Incompatible(err.to_string())
            }
            FormatError::NotVmstate(msg) => SnapshotError::Incompatible(msg),
            FormatError::Corrupt(msg) => SnapshotError::Internal(msg),
        }
    }
}

/// One keyed section of the vmstate stream.
type Section = (String, Vec<u8>);

// ---- the registry ------------------------------------------------------

/// One disk of the machine, with the shared image handle. The block device
/// runs on the same handle (through [`crate::factory::SharedDisk`]), so
/// guest I/O and snapshot operations serialise on one open image.
pub struct DiskNode {
    pub id: String,
    pub writable: bool,
    /// The disk whose snapshot carries RAM and device state.
    pub vmstate: bool,
    pub image: crate::factory::SharedImage,
}

/// One snapshottable device, registered at build time.
pub enum DeviceNode {
    /// A single-section device: the UART.
    Plain(Arc<Mutex<dyn Snapshot>>),
    /// A PCI function with layered state: the function (a virtio transport)
    /// is saved and restored before the device it wraps.
    Virtio(Arc<Mutex<dyn PciSnapshot>>),
}

impl DeviceNode {
    /// The sections this node contributes, in save order: `(key, bytes)`.
    pub fn save(&self) -> Result<Vec<Section>, String> {
        match self {
            DeviceNode::Plain(dev) => {
                let dev = dev.lock();
                let state = dev
                    .save_state()
                    .map_err(|e| format!("{}: {e}", dev.state_id()))?;
                Ok(vec![(dev.state_id(), state)])
            }
            DeviceNode::Virtio(dev) => {
                let dev = dev.lock();
                let id = dev.snapshot_id();
                let function = dev
                    .save_function_state()
                    .map_err(|e| format!("virtio-pci/{id}: {e}"))?;
                let wrapped = dev
                    .save_wrapped_state()
                    .map_err(|e| format!("virtio-dev/{id}: {e}"))?;
                Ok(vec![
                    (format!("virtio-pci/{id}"), function),
                    (format!("virtio-dev/{id}"), wrapped),
                ])
            }
        }
    }

    /// Restore this node's sections from a saved map. Missing keys are an
    /// error: a machine whose digest matched must find every section.
    pub fn restore(&self, sections: &SavedSections) -> Result<(), String> {
        match self {
            DeviceNode::Plain(dev) => {
                let mut dev = dev.lock();
                let key = dev.state_id();
                let state = sections
                    .get(&key)
                    .ok_or_else(|| format!("snapshot lacks section {key:?}"))?;
                dev.restore_state(state)
                    .map_err(|e| format!("{}: {e}", key))
            }
            DeviceNode::Virtio(dev) => {
                let mut dev = dev.lock();
                let id = dev.snapshot_id();
                let function_key = format!("virtio-pci/{id}");
                let wrapped_key = format!("virtio-dev/{id}");
                let function = sections
                    .get(&function_key)
                    .ok_or_else(|| format!("snapshot lacks section {function_key:?}"))?;
                let wrapped = sections
                    .get(&wrapped_key)
                    .ok_or_else(|| format!("snapshot lacks section {wrapped_key:?}"))?;
                dev.restore_function_state(function)
                    .map_err(|e| format!("{function_key}: {e}"))?;
                dev.restore_wrapped_state(wrapped)
                    .map_err(|e| format!("{wrapped_key}: {e}"))
            }
        }
    }
}

/// Device sections of one snapshot, keyed as [`DeviceNode::save`] writes
/// them. Unknown keys are kept but ignored on restore (best effort within a
/// major version, D32).
#[derive(Default)]
pub struct SavedSections {
    map: std::collections::BTreeMap<String, Vec<u8>>,
}

impl SavedSections {
    fn get(&self, key: &str) -> Option<&[u8]> {
        self.map.get(key).map(|v| v.as_slice())
    }

    fn insert(&mut self, key: String, value: Vec<u8>) {
        self.map.insert(key, value);
    }
}

/// What a built machine registers for snapshot orchestration.
pub struct SnapshotRegistry {
    /// Digest of the serialised machine description; a restore of a
    /// different machine refuses (D32).
    pub digest: [u8; 32],
    pub disks: Vec<DiskNode>,
    pub nodes: Vec<DeviceNode>,
}

/// The registry of a machine that can snapshot.
fn registry(handle: &VmHandle) -> Result<Arc<SnapshotRegistry>, SnapshotError> {
    handle
        .shared()
        .snapshot_registry()
        .ok_or_else(|| SnapshotError::Internal("machine has no snapshot registry".into()))
}

/// The one writable disk marked `vmstate: true`.
fn vmstate_disk(reg: &SnapshotRegistry) -> Result<&DiskNode, SnapshotError> {
    let marked: Vec<&DiskNode> = reg
        .disks
        .iter()
        .filter(|d| d.vmstate && d.writable)
        .collect();
    match marked.len() {
        1 => Ok(marked[0]),
        0 => Err(SnapshotError::Unsupported(
            "online snapshots need one writable disk marked vmstate: true; this machine has none"
                .into(),
        )),
        n => Err(SnapshotError::Unsupported(format!(
            "online snapshots need one writable disk marked vmstate: true; this machine has {n}"
        ))),
    }
}

fn writable_disks(reg: &SnapshotRegistry) -> impl Iterator<Item = &DiskNode> {
    reg.disks.iter().filter(|d| d.writable)
}

// ---- save --------------------------------------------------------------

/// `snapshot_save {name}`: quiesce, capture, stream into the vmstate disk,
/// write the internal snapshot on every writable disk, and leave the machine
/// in the run state it had.
pub fn save(handle: &VmHandle, name: &str) -> Result<(), SnapshotError> {
    let shared = handle.shared();
    let _op = shared.begin_snapshot();
    // Refuse what cannot work before touching the run state.
    let reg = registry(handle)?;
    let vmstate_disk = vmstate_disk(&reg)?;
    let was = shared
        .stop_for_snapshot(QUIESCE_TIMEOUT)
        .map_err(SnapshotError::Internal)?;

    let captured = capture_and_write(shared, &reg, vmstate_disk, name, was);
    // The machine goes back to the run state it had, error or not.
    shared.finish_snapshot(run_code(was));
    captured
}

/// The stopped part of a save: sections, RAM stream, disk snapshots.
fn capture_and_write(
    shared: &crate::vm::VmShared,
    reg: &SnapshotRegistry,
    vmstate_disk: &DiskNode,
    name: &str,
    was: vmlab_hv_proto::control::RunState,
) -> Result<(), SnapshotError> {
    let mut sections = Vec::new();
    // Devices first: their save quiesces any host-side worker (a
    // vhost-user-fs device stops its daemon here) before RAM is read.
    for node in &reg.nodes {
        sections.extend(node.save().map_err(SnapshotError::Internal)?);
    }
    save_kernel_state(shared, &mut sections)?;
    let head = format::encode_header_sections(
        run_code(was),
        env!("CARGO_PKG_VERSION"),
        &reg.digest,
        &sections,
    );

    // Stream into the vmstate area, RAM last and run-length coded.
    let mut offset: u64 = 0;
    let mut total: u64 = 0;
    {
        let mut image = vmstate_disk.image.lock();
        let mut write = |bytes: &[u8]| -> Result<(), SnapshotError> {
            image
                .write_vmstate(offset, bytes)
                .map_err(|e| SnapshotError::from_disk("write_vmstate", &vmstate_disk.id, e))?;
            offset += bytes.len() as u64;
            total += bytes.len() as u64;
            Ok(())
        };
        write(&head)?;
        let ram = shared
            .ram()
            .ok_or_else(|| SnapshotError::Internal("machine has no RAM".into()))?;
        stream_ram_out(&ram, &mut write)?;
    }

    // The internal snapshot on every writable disk, under the same name.
    for disk in writable_disks(reg) {
        let mut image = disk.image.lock();
        let size = if disk.vmstate { total } else { 0 };
        image
            .snapshot_create(name, size, 0)
            .map_err(|e| SnapshotError::from_disk("snapshot_create", &disk.id, e))?;
    }
    Ok(())
}

/// Walk guest RAM page by page, emitting run-length records.
fn stream_ram_out(
    ram: &crate::memory::GuestRam,
    write: &mut dyn FnMut(&[u8]) -> Result<(), SnapshotError>,
) -> Result<(), SnapshotError> {
    let regions = &ram.layout().regions;
    let mut region_header = Vec::new();
    format::push_u32(&mut region_header, regions.len() as u32);
    for &(base, len) in regions {
        format::push_u64(&mut region_header, base);
        format::push_u64(&mut region_header, len);
    }
    write(&region_header)?;
    let mut enc = format::RamEncoder::new();
    let mut page = vec![0u8; format::PAGE_SIZE as usize];
    for (base, len) in regions {
        let (base, len) = (*base, *len);
        if len % format::PAGE_SIZE != 0 {
            return Err(SnapshotError::Internal(format!(
                "RAM region {base:#x}+{len:#x} is not page aligned"
            )));
        }
        let mut chunk = Vec::new();
        let mut off = 0;
        while off < len {
            ram.mem
                .read_slice(&mut page, GuestAddress(base + off))
                .map_err(|e| SnapshotError::Internal(format!("reading guest page: {e}")))?;
            enc.page(&page, &mut chunk);
            if !chunk.is_empty() {
                write(&chunk)?;
                chunk.clear();
            }
            off += format::PAGE_SIZE;
        }
        enc.finish_region(&mut chunk);
        if !chunk.is_empty() {
            write(&chunk)?;
        }
    }
    Ok(())
}

/// The in-kernel and per-vCPU state, as sections.
fn save_kernel_state(
    shared: &crate::vm::VmShared,
    sections: &mut Vec<Section>,
) -> Result<(), SnapshotError> {
    let vm = &shared.kvm.vm;
    let io = |e: kvm_ioctls::Error| SnapshotError::Internal(format!("KVM ioctl: {e}"));

    for (id, chip) in [
        ("pic-master", KVM_IRQCHIP_PIC_MASTER),
        ("pic-slave", KVM_IRQCHIP_PIC_SLAVE),
        ("ioapic", KVM_IRQCHIP_IOAPIC),
    ] {
        let mut irqchip = kvm_irqchip {
            chip_id: chip,
            ..Default::default()
        };
        vm.get_irqchip(&mut irqchip).map_err(io)?;
        sections.push((id.into(), to_json(&irqchip)?));
    }

    let pit: kvm_pit_state2 = vm.get_pit2().map_err(io)?;
    sections.push(("pit".into(), to_json(&pit)?));
    let clock: kvm_clock_data = vm.get_clock().map_err(io)?;
    sections.push(("kvmclock".into(), to_json(&clock)?));

    sections.push(("pci-root".into(), to_json(&shared.save_pci_bars())?));

    let vcpus = shared.vcpus.lock().clone();
    for (id, fd) in &vcpus {
        let fd = fd.lock();
        let p = |s: &str| format!("vcpu{id}/{s}");
        sections.push((p("regs"), to_json(&fd.get_regs().map_err(io)?)?));
        sections.push((p("sregs"), to_json(&fd.get_sregs().map_err(io)?)?));
        sections.push((p("fpu"), encode_fpu(&fd.get_fpu().map_err(io)?)));
        sections.push((p("xsave"), encode_xsave(&fd.get_xsave().map_err(io)?)));
        let mut indices = MIGRATABLE_MSRS.to_vec();
        indices.extend(
            crate::hyperv::snapshot_msr_indices(&shared.kvm.kvm, &fd)
                .map_err(|e| SnapshotError::Internal(e.to_string()))?,
        );
        if shared.chipset.lock().is_some() {
            indices.push(0x9e);
        } // SMBASE
        sections.push((p("msrs"), encode_msrs(&fd, &indices)?));
        sections.push((p("lapic"), encode_lapic(&fd.get_lapic().map_err(io)?)));
        sections.push((p("events"), to_json(&fd.get_vcpu_events().map_err(io)?)?));
        sections.push((p("mp_state"), to_json(&fd.get_mp_state().map_err(io)?)?));
    }
    Ok(())
}

// ---- load --------------------------------------------------------------

/// `snapshot_load {name}`: quiesce, revert every writable disk, validate the
/// vmstate header (major version and machine digest), restore RAM then
/// kernel then devices, and leave the machine in the run state the snapshot
/// recorded.
pub fn load(handle: &VmHandle, name: &str) -> Result<(), SnapshotError> {
    let shared = handle.shared();
    let _op = shared.begin_snapshot();
    // Refuse what cannot work before touching the run state.
    let reg = registry(handle)?;
    let vmstate_disk = vmstate_disk(&reg)?;
    shared
        .stop_for_snapshot(QUIESCE_TIMEOUT)
        .map_err(SnapshotError::Internal)?;

    match restore(shared, &reg, vmstate_disk, name) {
        Ok(run_code) => {
            shared.finish_snapshot(run_code);
            Ok(())
        }
        // Leave the machine paused: after a half-applied restore, resuming
        // a corrupted guest is worse than waiting for a host decision.
        Err(e) => {
            shared.finish_snapshot(format::RUN_PAUSED);
            Err(e)
        }
    }
}

/// The stopped part of a load: revert the disks, validate the stream,
/// restore everything. Returns the recorded run state on success.
fn restore(
    shared: &crate::vm::VmShared,
    reg: &SnapshotRegistry,
    vmstate_disk: &DiskNode,
    name: &str,
) -> Result<u8, SnapshotError> {
    // Revert every writable disk first: switching the vmstate disk to its
    // snapshot is what makes the vmstate area readable again. The applied
    // info carries the recorded vmstate size.
    let mut vmstate_size = None;
    for disk in writable_disks(reg) {
        let mut image = disk.image.lock();
        let info = image
            .snapshot_apply(name)
            .map_err(|e| SnapshotError::from_disk("snapshot_apply", &disk.id, e))?;
        if disk.vmstate {
            vmstate_size = Some(info.vm_state_size);
        }
    }
    let vmstate_size = vmstate_size.unwrap_or(0);
    if vmstate_size == 0 {
        return Err(SnapshotError::Incompatible(format!(
            "snapshot {name:?} on disk `{}` carries no machine state (an offline snapshot)",
            vmstate_disk.id
        )));
    }

    let mut image = vmstate_disk.image.lock();
    let mut bytes = vec![0u8; vmstate_size as usize];
    image
        .read_vmstate(0, &mut bytes)
        .map_err(|e| SnapshotError::from_disk("read_vmstate", &vmstate_disk.id, e))?;
    drop(image);

    let (header, mut reader) = format::decode_header(&bytes).map_err(SnapshotError::from_format)?;
    if header.digest != reg.digest {
        return Err(SnapshotError::from_format(FormatError::DigestMismatch {
            saved: format::hex(&header.digest),
            current: format::hex(&reg.digest),
        }));
    }

    let mut sections = SavedSections::default();
    for _ in 0..header.section_count {
        let (id, body) = format::read_section(&mut reader).map_err(SnapshotError::from_format)?;
        sections.insert(id, body);
    }
    let ram = shared
        .ram()
        .ok_or_else(|| SnapshotError::Internal("machine has no RAM".into()))?;

    // Zero records are sparse in the stream. An in-place load must clear
    // pages that the guest has dirtied since capture before applying data.
    let zeros = [0u8; 64 * 1024];
    for &(base, len) in ram.layout().regions.iter() {
        let mut offset = 0;
        while offset < len {
            let count = (len - offset).min(zeros.len() as u64) as usize;
            ram.mem
                .write_slice(&zeros[..count], GuestAddress(base + offset))
                .map_err(|e| SnapshotError::Internal(format!("clearing restored RAM: {e}")))?;
            offset += count as u64;
        }
    }

    let regions = format::decode_ram_regions(&mut reader, |base, run| {
        ram.mem
            .write_slice(run, GuestAddress(base))
            .map_err(|e| format!("writing guest page {base:#x}: {e}"))
    })
    .map_err(SnapshotError::from_format)?;
    if !reader.done() {
        return Err(SnapshotError::from_format(FormatError::Corrupt(
            "trailing bytes after RAM".into(),
        )));
    }
    if regions != *ram.layout().regions {
        return Err(SnapshotError::Incompatible(format!(
            "snapshot RAM layout {regions:#x?} does not match this machine's {:?}",
            ram.layout().regions
        )));
    }

    // Replacing RAM also replaces page tables. Removing the slots forces
    // KVM to invalidate translations even when the restored CR3 is unchanged.
    for slot in 0..ram.layout().regions.len() {
        let region = kvm_bindings::kvm_userspace_memory_region {
            slot: slot as u32,
            ..Default::default()
        };
        // SAFETY: all vCPUs are parked and this only removes a RAM slot.
        unsafe { shared.kvm.vm.set_user_memory_region(region) }
            .map_err(|e| SnapshotError::Internal(format!("invalidating restored RAM: {e}")))?;
    }
    crate::memory::register_memslots(&shared.kvm.vm, &ram)
        .map_err(|e| SnapshotError::Internal(format!("registering restored RAM: {e}")))?;
    restore_kernel_state(shared, &sections)?;
    for node in &reg.nodes {
        node.restore(&sections).map_err(SnapshotError::Internal)?;
    }
    let pci_bars = sections
        .get("pci-root")
        .ok_or_else(|| SnapshotError::Internal("snapshot lacks section \"pci-root\"".into()))?;
    shared
        .restore_pci_bars(pci_bars)
        .map_err(SnapshotError::Internal)?;
    shared
        .sync_vga_mapping()
        .map_err(|e| SnapshotError::Internal(e.to_string()))?;
    Ok(header.run_state)
}

// ---- delete ------------------------------------------------------------

/// `snapshot_delete {name}`: remove the internal snapshot from every
/// writable disk. Missing on any disk is an error naming it; the others are
/// still cleaned.
pub fn delete(handle: &VmHandle, name: &str) -> Result<(), SnapshotError> {
    let reg = registry(handle)?;
    let mut first_err = None;
    for disk in writable_disks(&reg) {
        let mut image = disk.image.lock();
        if let Err(e) = image.snapshot_delete(name) {
            let err = SnapshotError::from_disk("snapshot_delete", &disk.id, e);
            if first_err.is_none() {
                first_err = Some(err);
            }
        }
    }
    match first_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

// ---- run-state codes ---------------------------------------------------

use vmlab_hv_proto::control::RunState;

fn run_code(state: RunState) -> u8 {
    match state {
        RunState::Running => format::RUN_RUNNING,
        RunState::Suspended => format::RUN_SUSPENDED,
        RunState::Paused | RunState::ShuttingDown => format::RUN_PAUSED,
    }
}

// ---- codec helpers -----------------------------------------------------

fn to_json<T: Serialize>(value: &T) -> Result<Vec<u8>, SnapshotError> {
    serde_json::to_vec(value).map_err(|e| SnapshotError::Internal(format!("encoding state: {e}")))
}

fn from_json<T: DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T, SnapshotError> {
    serde_json::from_slice(bytes)
        .map_err(|e| SnapshotError::Internal(format!("decoding {what}: {e}")))
}

fn encode_fpu(fpu: &kvm_fpu) -> Vec<u8> {
    let mut out = Vec::with_capacity(416);
    for reg in &fpu.fpr {
        out.extend_from_slice(reg);
    }
    out.extend_from_slice(&fpu.fcw.to_le_bytes());
    out.extend_from_slice(&fpu.fsw.to_le_bytes());
    out.push(fpu.ftwx);
    out.push(fpu.pad1);
    out.extend_from_slice(&fpu.last_opcode.to_le_bytes());
    out.extend_from_slice(&fpu.last_ip.to_le_bytes());
    out.extend_from_slice(&fpu.last_dp.to_le_bytes());
    for reg in &fpu.xmm {
        out.extend_from_slice(reg);
    }
    out.extend_from_slice(&fpu.mxcsr.to_le_bytes());
    out.extend_from_slice(&fpu.pad2.to_le_bytes());
    out
}

fn decode_fpu(bytes: &[u8]) -> Result<kvm_fpu, SnapshotError> {
    let bad = || SnapshotError::Internal("bad fpu state".into());
    let mut r = format::Reader::new(bytes);
    let mut fpu = kvm_fpu::default();
    for reg in &mut fpu.fpr {
        *reg = r.take(16).map_err(|_| bad())?.try_into().unwrap();
    }
    fpu.fcw = u16::from_le_bytes(r.take(2).map_err(|_| bad())?.try_into().unwrap());
    fpu.fsw = u16::from_le_bytes(r.take(2).map_err(|_| bad())?.try_into().unwrap());
    fpu.ftwx = r.u8().map_err(|_| bad())?;
    fpu.pad1 = r.u8().map_err(|_| bad())?;
    fpu.last_opcode = u16::from_le_bytes(r.take(2).map_err(|_| bad())?.try_into().unwrap());
    fpu.last_ip = u64::from_le_bytes(r.take(8).map_err(|_| bad())?.try_into().unwrap());
    fpu.last_dp = u64::from_le_bytes(r.take(8).map_err(|_| bad())?.try_into().unwrap());
    for reg in &mut fpu.xmm {
        *reg = r.take(16).map_err(|_| bad())?.try_into().unwrap();
    }
    fpu.mxcsr = u32::from_le_bytes(r.take(4).map_err(|_| bad())?.try_into().unwrap());
    fpu.pad2 = u32::from_le_bytes(r.take(4).map_err(|_| bad())?.try_into().unwrap());
    if !r.done() {
        return Err(bad());
    }
    Ok(fpu)
}

fn encode_xsave(xsave: &kvm_xsave) -> Vec<u8> {
    let mut out = Vec::with_capacity(4096);
    for word in &xsave.region {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out
}

fn decode_xsave(bytes: &[u8]) -> Result<kvm_xsave, SnapshotError> {
    let mut xsave = kvm_xsave::default();
    if bytes.len() != 4096 {
        return Err(SnapshotError::Internal("bad xsave state".into()));
    }
    for (i, word) in xsave.region.iter_mut().enumerate() {
        *word = u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
    }
    Ok(xsave)
}

fn encode_msrs(fd: &VcpuFd, indices: &[u32]) -> Result<Vec<u8>, SnapshotError> {
    let entries: Vec<kvm_msr_entry> = indices
        .iter()
        .map(|&index| kvm_msr_entry {
            index,
            reserved: 0,
            data: 0,
        })
        .collect();
    let mut msrs = Msrs::from_entries(&entries)
        .map_err(|e| SnapshotError::Internal(format!("building the MSR list: {e}")))?;
    let filled = fd
        .get_msrs(&mut msrs)
        .map_err(|e| SnapshotError::Internal(format!("KVM_GET_MSRS: {e}")))?;
    if filled != indices.len()
        && indices
            .iter()
            .any(|n| (0x4000_0000..0x4000_0200).contains(n))
    {
        return Err(SnapshotError::Internal(format!(
            "KVM_GET_MSRS: saved {filled} of {} MSRs, including Hyper-V state",
            indices.len()
        )));
    }
    let mut out = Vec::with_capacity(4 + filled * 12);
    format::push_u32(&mut out, filled as u32);
    for entry in &msrs.as_slice()[..filled] {
        format::push_u32(&mut out, entry.index);
        format::push_u64(&mut out, entry.data);
    }
    Ok(out)
}

fn decode_msrs(bytes: &[u8]) -> Result<Vec<kvm_msr_entry>, SnapshotError> {
    let bad = || SnapshotError::Internal("bad msr state".into());
    let mut r = format::Reader::new(bytes);
    let count = r.u32().map_err(|_| bad())? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push(kvm_msr_entry {
            index: r.u32().map_err(|_| bad())?,
            reserved: 0,
            data: r.u64().map_err(|_| bad())?,
        });
    }
    if !r.done() {
        return Err(bad());
    }
    Ok(entries)
}

fn encode_lapic(lapic: &kvm_lapic_state) -> Vec<u8> {
    lapic.regs.iter().map(|&b| b as u8).collect()
}

fn decode_lapic(bytes: &[u8]) -> Result<kvm_lapic_state, SnapshotError> {
    if bytes.len() != 1024 {
        return Err(SnapshotError::Internal("bad lapic state".into()));
    }
    let mut lapic = kvm_lapic_state::default();
    for (reg, byte) in lapic.regs.iter_mut().zip(bytes) {
        *reg = *byte as std::os::raw::c_char;
    }
    Ok(lapic)
}

/// Restore the in-kernel and per-vCPU state from the saved sections, in the
/// order the kernel expects: irqchip, timers, clock, then each vCPU.
fn restore_kernel_state(
    shared: &crate::vm::VmShared,
    sections: &SavedSections,
) -> Result<(), SnapshotError> {
    let vm = &shared.kvm.vm;
    let io = |e: kvm_ioctls::Error| SnapshotError::Internal(format!("KVM ioctl: {e}"));
    let missing = |key: &str| SnapshotError::Internal(format!("snapshot lacks section {key:?}"));

    for (id, chip) in [
        ("pic-master", KVM_IRQCHIP_PIC_MASTER),
        ("pic-slave", KVM_IRQCHIP_PIC_SLAVE),
        ("ioapic", KVM_IRQCHIP_IOAPIC),
    ] {
        let state: kvm_irqchip = from_json(sections.get(id).ok_or_else(|| missing(id))?, id)?;
        // The saved union came from the same ioctl with this chip_id; the
        // copy is by value.
        let restored = kvm_irqchip {
            chip_id: chip,
            chip: state.chip,
            pad: Default::default(),
        };
        vm.set_irqchip(&restored).map_err(io)?;
    }

    let pit: kvm_pit_state2 = from_json(sections.get("pit").ok_or_else(|| missing("pit"))?, "pit")?;
    vm.set_pit2(&pit).map_err(io)?;
    let mut clock: kvm_clock_data = from_json(
        sections
            .get("kvmclock")
            .ok_or_else(|| missing("kvmclock"))?,
        "kvmclock",
    )?;
    // Restore the captured instant. KVM_CLOCK_REALTIME would add host time
    // elapsed since capture, defeating clock rollback on online restore.
    clock.flags = 0;
    vm.set_clock(&clock).map_err(io)?;

    let vcpus = shared.vcpus.lock().clone();
    for (id, fd) in &vcpus {
        let fd = fd.lock();
        let p = |s: &str| format!("vcpu{id}/{s}");
        let get = |s: &str| -> Result<&[u8], SnapshotError> {
            sections.get(&p(s)).ok_or_else(|| missing(&p(s)))
        };
        let mp_state: kvm_mp_state = from_json(get("mp_state")?, "mp_state")?;
        fd.set_mp_state(mp_state).map_err(io)?;
        let sregs: kvm_sregs = from_json(get("sregs")?, "sregs")?;
        fd.set_sregs(&sregs).map_err(io)?;
        let regs: kvm_regs = from_json(get("regs")?, "regs")?;
        fd.set_regs(&regs).map_err(io)?;
        let fpu = decode_fpu(get("fpu")?)?;
        fd.set_fpu(&fpu).map_err(io)?;
        let xsave = decode_xsave(get("xsave")?)?;
        // SAFETY: the value is a zero-initialised kvm_xsave with only the
        // fixed 4096-byte region filled; the ioctl's size argument is that
        // same 4096-byte struct, so the kernel never reads the tail.
        unsafe {
            fd.set_xsave(&xsave).map_err(io)?;
        }
        let entries = decode_msrs(get("msrs")?)?;
        let msrs = Msrs::from_entries(&entries)
            .map_err(|e| SnapshotError::Internal(format!("building the MSR list: {e}")))?;
        let written = fd
            .set_msrs(&msrs)
            .map_err(|e| SnapshotError::Internal(format!("KVM_SET_MSRS: {e}")))?;
        if written != entries.len() {
            return Err(SnapshotError::Internal(format!(
                "vcpu {id}: restored {written} of {} MSRs",
                entries.len()
            )));
        }
        let lapic = decode_lapic(get("lapic")?)?;
        fd.set_lapic(&lapic).map_err(io)?;
        let events: kvm_vcpu_events = from_json(get("events")?, "events")?;
        fd.set_vcpu_events(&events).map_err(io)?;
    }
    Ok(())
}
