//! Opt-in, stopped-state KVM snapshot differential records.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use kvm_bindings::{KVM_MAX_CPUID_ENTRIES, Msrs, kvm_msr_entry};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};
use zerocopy::IntoBytes;

use super::SnapshotError;

pub(super) fn write(shared: &crate::vm::VmShared, phase: &str) {
    if let Err(error) = record(shared, phase) {
        // A diagnostic failure must not interrupt a partially applied load.
        eprintln!("{error}");
    }
}

fn record(shared: &crate::vm::VmShared, phase: &str) -> Result<(), SnapshotError> {
    let Some(directory) = std::env::var_os("VMLAB_HV_SNAPSHOT_AUDIT") else {
        return Ok(());
    };
    let error = |e: &dyn std::fmt::Display| SnapshotError::Internal(format!("snapshot audit: {e}"));
    let mut state = BTreeMap::<String, Value>::new();
    let mut sections = Vec::new();
    super::save_kernel_state(shared, &mut sections)?;
    for (key, bytes) in sections {
        state.insert(
            key,
            serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(bytes)),
        );
    }
    let indices = shared.kvm.kvm.get_msr_index_list().map_err(|e| error(&e))?;
    state.insert(
        "xsave-size".into(),
        json!(shared.kvm.vm.check_extension_int(kvm_ioctls::Cap::Xsave2)),
    );
    for (id, fd) in shared.vcpus.lock().iter() {
        let fd = fd.lock();
        let p = |s: &str| format!("vcpu{id}/{s}");
        state.insert(p("xcrs"), json!(fd.get_xcrs().map_err(|e| error(&e))?));
        state.insert(
            p("debugregs"),
            json!(fd.get_debug_regs().map_err(|e| error(&e))?),
        );
        state.insert(
            p("cpuid"),
            json!(
                fd.get_cpuid2(KVM_MAX_CPUID_ENTRIES)
                    .map_err(|e| error(&e))?
                    .as_slice()
            ),
        );
        state.insert(
            p("tsc-khz"),
            json!(fd.get_tsc_khz().map_err(|e| error(&e))?),
        );
        if shared.kvm.kvm.check_extension(kvm_ioctls::Cap::NestedState) {
            let mut nested = kvm_ioctls::KvmNestedStateBuffer::empty();
            fd.nested_state(&mut nested).map_err(|e| error(&e))?;
            state.insert(
                p("nested"),
                json!(&nested.as_bytes()[..nested.size as usize]),
            );
        }
        let mut msrs = BTreeMap::new();
        let mut all = indices.as_slice().to_vec();
        all.extend(
            crate::hyperv::snapshot_msr_indices(&shared.kvm.kvm, &fd).map_err(|e| error(&e))?,
        );
        all.sort_unstable();
        all.dedup();
        for index in all {
            let mut entry = Msrs::from_entries(&[kvm_msr_entry {
                index,
                ..Default::default()
            }])
            .map_err(|e| error(&e))?;
            let value = if fd.get_msrs(&mut entry).map_err(|e| error(&e))? == 1 {
                json!(entry.as_slice()[0].data)
            } else {
                Value::Null
            };
            msrs.insert(format!("{index:#010x}"), value);
        }
        state.insert(p("all-msrs"), json!(msrs));
    }
    if let Some(ram) = shared.ram() {
        let mut page = [0u8; 4096];
        for region in ram.mem.iter() {
            let base = region.start_addr().raw_value();
            let len = region.len();
            let mut hash = Sha256::new();
            for offset in (0..len).step_by(page.len()) {
                ram.mem
                    .read_slice(&mut page, GuestAddress(base + offset))
                    .map_err(|e| error(&e))?;
                hash.update(page);
            }
            state.insert(
                format!("ram/{base:#x}+{len:#x}"),
                json!(format!("{:x}", hash.finalize())),
            );
        }
    }
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::path::PathBuf::from(directory)
        .join(format!("{}-{sequence:03}-{phase}.json", std::process::id()));
    let file = std::fs::File::create(path).map_err(|e| error(&e))?;
    serde_json::to_writer_pretty(file, &state).map_err(|e| error(&e))
}
