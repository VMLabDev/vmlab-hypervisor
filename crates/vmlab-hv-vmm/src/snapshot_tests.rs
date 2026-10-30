//! KVM integration tests for snapshot save, load and delete.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex as PlMutex;
use vm_memory::{Bytes, GuestAddress};
use vmlab_hv_devices::IrqLine;
use vmlab_hv_devices::serial::{Serial, SerialOut};

use crate::kvm::KvmVm;
use crate::layout::MemLayout;
use crate::memory::{GuestRam, register_memslots};
use crate::pci::PciRoot;
use crate::pm::{PM_BLOCK_LEN, PM1A_EVT_BASE, PmDevice, SleepSignal};
use crate::snapshot::{self, DeviceNode, DiskNode, SnapshotRegistry};
use crate::vm::{Vm, VmHandle, VmShared};

/// How long to let the guest run between observations.
const SETTLE: Duration = Duration::from_millis(150);

/// A serial-log sink the test can read.
#[derive(Clone, Default)]
struct Log(Arc<PlMutex<Vec<u8>>>);

struct LogWriter(Log);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct NullIrq;

impl IrqLine for NullIrq {
    fn set_level(&self, _high: bool) {}
}

/// The counter/scratch payload: sets the UART scratch register to 0x42,
/// then forever increments the word at 0x9000, prints its low byte to
/// COM1, prints the scratch register's readback, and switches the scratch
/// to 0x99 once the counter passes 0x4000. A snapshot restored after the
/// switch must roll both back.
fn counter_payload() -> Vec<u8> {
    let mut code = Vec::new();
    // SCRATCH = 0x42
    code.extend_from_slice(&[0xBA, 0xFF, 0x03]); // mov dx, 0x3FF
    code.extend_from_slice(&[0xB0, 0x42]); // mov al, 0x42
    code.extend_from_slice(&[0xEE]); // out dx, al
    let loop_at = code.len();
    code.extend_from_slice(&[0xFF, 0x06, 0x00, 0x90]); // inc word [0x9000]
    code.extend_from_slice(&[0x81, 0x3E, 0x00, 0x90, 0x00, 0x40]); // cmp word [0x9000], 0x4000
    let jb_at = code.len();
    code.extend_from_slice(&[0x72, 0x00]); // jb .keep (patched below)
    code.extend_from_slice(&[0xBA, 0xFF, 0x03]); // mov dx, 0x3FF
    code.extend_from_slice(&[0xB0, 0x99]); // mov al, 0x99
    code.extend_from_slice(&[0xEE]); // out dx, al
    let keep_at = code.len();
    // Print the scratch register's readback; that is all the log carries.
    code.extend_from_slice(&[0xBA, 0xFF, 0x03]); // mov dx, 0x3FF
    code.extend_from_slice(&[0xEC]); // in al, dx (SCRATCH)
    code.extend_from_slice(&[0xBA, 0xF8, 0x03]); // mov dx, 0x3F8
    code.extend_from_slice(&[0xEE]); // out dx, al
    let jmp_at = code.len();
    code.extend_from_slice(&[0xEB, 0x00]); // jmp .loop (patched below)
    code[jb_at + 1] = (keep_at as i32 - (jb_at as i32 + 2)) as i8 as u8;
    code[jmp_at + 1] = (loop_at as i32 - (jmp_at as i32 + 2)) as i8 as u8;
    code.resize(0x8000, 0);
    code
}

/// One snapshot test machine: real 16550A into a readable log, one qcow2
/// scratch disk marked vmstate, payload at 0x8000 in real mode.
struct Machine {
    vm: Vm,
    handle: VmHandle,
    log: Log,
    disk_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn kvm_or_skip() -> Option<KvmVm> {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: /dev/kvm is not available");
        return None;
    }
    match KvmVm::new() {
        Ok(vm) => Some(vm),
        Err(e) => {
            eprintln!("skipping: cannot create KVM VM: {e}");
            None
        }
    }
}

fn machine(payload: &[u8], disk_mib: u64) -> Option<Machine> {
    let kvm = kvm_or_skip()?;
    let dir = tempfile::tempdir().ok()?;
    let disk_path = dir.path().join("scratch.qcow2");
    vmlab_disk::ops::create_qcow2(&disk_path, disk_mib * 1024 * 1024, None, None).ok()?;
    let image: crate::factory::SharedImage = Arc::new(PlMutex::new(
        vmlab_disk::open(&disk_path, vmlab_disk::Format::Qcow2, false).ok()?,
    ));

    let layout = MemLayout::new(16).ok()?;
    let ram = GuestRam::anonymous(layout.clone()).ok()?;
    register_memslots(&kvm.vm, &ram).ok()?;
    ram.mem.write_slice(payload, GuestAddress(0x8000)).ok()?;

    let log = Log::default();
    let serial = Serial::new(
        "serial0",
        SerialOut::new(Box::new(LogWriter(log.clone()))),
        Arc::new(NullIrq),
    );
    let serial: Arc<PlMutex<Serial>> = Arc::new(PlMutex::new(serial));
    let bus_serial: Arc<PlMutex<dyn vmlab_hv_devices::BusDevice>> = serial.clone();
    let snap_serial: Arc<PlMutex<dyn vmlab_hv_devices::Snapshot>> = serial.clone();

    let mut io_bus = crate::bus::IoBus::new();
    let mmio_bus = crate::bus::MmioBus::new();
    let pci_root = Arc::new(PlMutex::new(PciRoot::new(layout.mmio64_base)));
    io_bus.register(0x3F8, 8, bus_serial).ok()?;

    let sleep_signal = Arc::new(SleepSignal::default());
    let pm = Arc::new(PlMutex::new(PmDevice::new(
        Arc::new(NullIrq),
        sleep_signal.sink(),
    )));
    io_bus
        .register(PM1A_EVT_BASE, PM_BLOCK_LEN, pm.clone())
        .ok()?;

    let shared = VmShared::new(
        kvm,
        io_bus,
        mmio_bus,
        pci_root,
        pm,
        sleep_signal,
        layout.mmio64_base,
    );
    shared.set_snapshot_registry(Arc::new(SnapshotRegistry {
        digest: [9u8; 32],
        disks: vec![DiskNode {
            id: "scratch".into(),
            writable: true,
            vmstate: true,
            image,
        }],
        nodes: vec![DeviceNode::Plain(snap_serial)],
    }));
    let mut vcpu = shared.kvm.create_vcpu(0).ok()?;
    crate::loader::init_real_mode(&mut vcpu, 0x8000).ok()?;
    let mut vm = Vm::from_parts(shared, vec![(0, vcpu)], ram).ok()?;
    vm.start();
    let handle = vm.handle();
    Some(Machine {
        vm,
        handle,
        log,
        disk_path,
        _dir: dir,
    })
}

impl Machine {
    /// Pairs of (counter, scratch) the guest has printed so far.
    fn observed(&self) -> Vec<u8> {
        self.log.0.lock().clone()
    }

    /// The guest's counter word at 0x9000.
    fn counter(&self) -> u16 {
        let mut word = [0u8; 2];
        self.vm
            .ram
            .mem
            .read_slice(&mut word, GuestAddress(0x9000))
            .expect("counter page mapped");
        u16::from_le_bytes(word)
    }
}

/// Run the machine for a while and return the observations after.
fn settle(m: &Machine) -> Vec<u8> {
    std::thread::sleep(SETTLE);
    m.observed()
}

#[test]
fn save_load_rolls_back_counter_and_scratch() {
    let Some(m) = machine(&counter_payload(), 16) else {
        return;
    };
    // Let it run; the scratch starts at 0x42 and the counter climbs.
    settle(&m);
    let counter_at_save = m.counter();
    assert!(counter_at_save > 0, "guest did not run");
    snapshot::save(&m.handle, "snap").expect("save");

    // Past the 0x4000 threshold the guest switches the scratch to 0x99;
    // watch the counter word directly, the log only carries scratch bytes.
    let mut switched = false;
    for _ in 0..100 {
        settle(&m);
        switched = m.counter() >= 0x4000;
        if switched {
            break;
        }
        assert!(
            !matches!(
                m.vm.handle().status(),
                vmlab_hv_proto::control::RunState::ShuttingDown
            ),
            "machine exited; log so far: {:?}",
            &m.observed()[m.observed().len().saturating_sub(20)..]
        );
    }
    assert!(switched, "counter never reached the threshold");
    let counter_pre_load = m.counter();
    assert!(
        counter_pre_load >= 0x4000,
        "counter past the threshold before load: {counter_pre_load:#x}"
    );

    let load_at = m.observed().len();
    snapshot::load(&m.handle, "snap").expect("load");
    // Read before settling: the snapshot recorded a running machine, so the
    // counter starts climbing again the moment the load resumes it.
    let counter_post = m.counter();
    assert!(
        counter_post >= counter_at_save && counter_post < counter_pre_load,
        "counter rolled back: post {counter_post:#x}, at save {counter_at_save:#x}, \
         pre-load {counter_pre_load:#x}"
    );
    settle(&m);
    // The scratch survived: 0x42 readbacks resume after the load even
    // though the guest had switched it to 0x99.
    // The first readbacks after the load are 0x42 again: the restored
    // scratch, even though the guest had switched it to 0x99. It switches
    // back to 0x99 later, once the rolled-back counter re-crosses the
    // threshold.
    // The guest ran until the load paused it, so a few pre-load 0x99
    // readbacks can trail into the slice; skip them.
    let tail = &m.observed()[load_at..];
    let restored_at = tail
        .iter()
        .position(|&b| b != 0x99)
        .expect("no readbacks after load");
    let head = &tail[restored_at..restored_at + 16.min(tail.len() - restored_at)];
    assert!(
        head.iter().all(|&b| b == 0x42),
        "scratch did not survive: first readbacks after load: {head:?}"
    );
}

#[test]
fn snapshot_is_visible_to_qemu_img_with_vm_state() {
    let Some(m) = machine(&counter_payload(), 16) else {
        return;
    };
    settle(&m);
    snapshot::save(&m.handle, "snapq").expect("save");

    let out = std::process::Command::new("qemu-img")
        .args(["snapshot", "-l", &m.disk_path.to_string_lossy()])
        .output()
        .expect("qemu-img runs");
    assert!(out.status.success(), "qemu-img snapshot -l failed");
    let text = String::from_utf8_lossy(&out.stdout);
    let row = text
        .lines()
        .find(|l| l.split_whitespace().nth(1) == Some("snapq"))
        .unwrap_or_else(|| panic!("snapshot row missing:\n{text}"));
    // ID TAG VM SIZE DATE ... — the size must not be 0 B.
    let size = row.split_whitespace().nth(2).unwrap();
    let unit = row.split_whitespace().nth(3).unwrap();
    assert!(!(size == "0" && unit == "B"), "VM SIZE is zero: {row}");
}

#[test]
fn delete_removes_and_missing_snapshots_error() {
    let Some(m) = machine(&counter_payload(), 8) else {
        return;
    };
    settle(&m);
    snapshot::save(&m.handle, "gone").expect("save");
    snapshot::delete(&m.handle, "gone").expect("delete");
    let err = snapshot::delete(&m.handle, "gone").expect_err("already deleted");
    assert!(err.is_not_found(), "{err}");
    let err = snapshot::load(&m.handle, "never").expect_err("absent");
    assert!(err.is_not_found(), "{err}");
}

#[test]
fn paused_snapshots_stay_paused() {
    let Some(m) = machine(&counter_payload(), 8) else {
        return;
    };
    settle(&m);
    m.handle.pause().expect("pause");
    assert_eq!(m.handle.status(), vmlab_hv_proto::control::RunState::Paused);
    snapshot::save(&m.handle, "paused-snap").expect("save while paused");
    // Saving a paused machine must not resume it.
    assert_eq!(m.handle.status(), vmlab_hv_proto::control::RunState::Paused);
    snapshot::load(&m.handle, "paused-snap").expect("load while paused");
    assert_eq!(
        m.handle.status(),
        vmlab_hv_proto::control::RunState::Paused,
        "a paused snapshot leaves the machine paused"
    );
    // And it still runs afterwards.
    m.handle.resume().expect("resume");
    let before = m.observed().len();
    settle(&m);
    assert!(m.observed().len() > before, "machine runs after resume");
}
