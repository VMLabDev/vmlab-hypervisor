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
    _pause_target: Arc<PlMutex<Option<VmHandle>>>,
}

struct PauseOnIo(std::sync::Weak<PlMutex<Option<VmHandle>>>, u8);

impl vmlab_hv_devices::BusDevice for PauseOnIo {
    fn read(&mut self, _: u64, data: &mut [u8]) {
        data.fill(self.1);
        self.1 = self.1.wrapping_add(1);
        if let Some(target) = self.0.upgrade()
            && let Some(handle) = target.lock().as_ref()
        {
            handle.pause().unwrap();
        }
    }

    fn write(&mut self, _: u64, _: &[u8]) {
        if let Some(target) = self.0.upgrade()
            && let Some(handle) = target.lock().as_ref()
        {
            handle.pause().unwrap();
        }
    }
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
    machine_with_setup(payload, disk_mib, |_, _| {})
}

fn machine_with_setup(
    payload: &[u8],
    disk_mib: u64,
    setup: impl FnOnce(&KvmVm, &mut kvm_ioctls::VcpuFd),
) -> Option<Machine> {
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
    let mut mmio_bus = crate::bus::MmioBus::new();
    let pause_target = Arc::new(PlMutex::new(None));
    io_bus
        .register(
            0x80,
            1,
            Arc::new(PlMutex::new(PauseOnIo(Arc::downgrade(&pause_target), 0x42))),
        )
        .ok()?;
    mmio_bus
        .register(
            0x1000_0000,
            4096,
            Arc::new(PlMutex::new(PauseOnIo(Arc::downgrade(&pause_target), 0x42))),
        )
        .ok()?;
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
    let mut sregs = vcpu.get_sregs().ok()?;
    sregs.ds.limit = u32::MAX;
    vcpu.set_sregs(&sregs).ok()?;
    setup(&shared.kvm, &mut vcpu);
    let mut vm = Vm::from_parts(shared, vec![(0, vcpu)], ram).ok()?;
    *pause_target.lock() = Some(vm.handle());
    vm.start();
    let handle = vm.handle();
    Some(Machine {
        vm,
        handle,
        log,
        disk_path,
        _dir: dir,
        _pause_target: pause_target,
    })
}

#[test]
fn dropping_vm_joins_vcpus_and_releases_ram() {
    for paused in [false, true] {
        for _ in 0..8 {
            let Some(machine) = machine(&counter_payload(), 32) else {
                return;
            };
            std::thread::sleep(SETTLE);
            if paused {
                machine.handle.pause().unwrap();
            }
            let shared = Arc::downgrade(machine.handle.shared());
            let ram = Arc::downgrade(&machine.vm.ram);
            let log = machine.log.clone();
            drop(machine);
            let before = log.0.lock().len();
            std::thread::sleep(SETTLE);
            let after = log.0.lock().len();
            eprintln!(
                "after VM drop: shared={}, RAM={}, serial bytes {before} -> {after}",
                shared.strong_count(),
                ram.strong_count()
            );
            assert!(
                shared.upgrade().is_none(),
                "dropped VM retained shared state"
            );
            assert!(ram.upgrade().is_none(), "dropped VM retained guest RAM");
        }
    }
}

#[test]
fn control_server_releases_exited_vm() {
    use vmlab_hv_proto::{client::Client, control::Command};
    let Some(machine) = machine(&counter_payload(), 32) else {
        return;
    };
    let path = machine._dir.path().join("control.sock");
    let shared = Arc::downgrade(machine.handle.shared());
    let ram = Arc::downgrade(&machine.vm.ram);
    let server = crate::control::serve(&path, &machine.handle).unwrap();
    let mut client = Client::connect(&path).unwrap();
    client.call(Command::Quit).unwrap();
    drop(client);
    machine.vm.wait();
    drop(machine.handle);
    drop(machine._pause_target);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !server.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(server.is_finished(), "control listener outlived its VM");
    server.join().unwrap();
    assert!(
        shared.upgrade().is_none(),
        "control server retained VM state"
    );
    assert!(ram.upgrade().is_none(), "control server retained guest RAM");
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
fn snapshot_preserves_a_broadcast_smi_waiting_for_its_vcpu() {
    let Some(m) = machine(&counter_payload(), 16) else {
        return;
    };
    m.handle.pause().unwrap();
    let shared = m.handle.shared();
    assert!(!shared.dispatch_smi(1));
    snapshot::save(&m.handle, "pending-smi").unwrap();
    assert!(shared.take_pending_smi(0));
    snapshot::load(&m.handle, "pending-smi").unwrap();
    assert!(shared.take_pending_smi(0), "snapshot lost the queued SMI");
    assert!(!shared.take_pending_smi(0));
    snapshot::save(&m.handle, "no-smi").unwrap();
    shared.dispatch_smi(1);
    snapshot::load(&m.handle, "no-smi").unwrap();
    assert!(!shared.take_pending_smi(0), "restore retained a future SMI");
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
fn load_clears_pages_that_were_zero_at_save() {
    let Some(m) = machine(&counter_payload(), 16) else {
        return;
    };
    m.handle.pause().expect("pause");
    let ram = m.handle.shared().ram().unwrap();
    let address = GuestAddress(0x20000);
    let mut bytes = [0u8; 4096];
    ram.mem.read_slice(&mut bytes, address).unwrap();
    assert_eq!(bytes, [0; 4096]);
    snapshot::save(&m.handle, "zeros").expect("save");
    ram.mem.write_slice(&[0xa5; 4096], address).unwrap();
    snapshot::load(&m.handle, "zeros").expect("load");
    ram.mem.read_slice(&mut bytes, address).unwrap();
    assert_eq!(bytes, [0; 4096]);
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

#[test]
fn io_instruction_completed_before_snapshot() {
    snapshot_at_io(&[0xec], 0x42);
}

#[test]
fn mmio_instruction_completed_before_snapshot() {
    snapshot_at_io(&[0x67, 0xa0, 0, 0, 0, 0x10], 0x42);
}

#[test]
fn output_instruction_completed_before_snapshot() {
    snapshot_at_io(&[0xee], 0x11);
}

#[test]
fn mmio_write_instruction_completed_before_snapshot() {
    snapshot_at_io(&[0x67, 0xa2, 0, 0, 0, 0x10], 0x11);
}

fn snapshot_at_io(instruction: &[u8], expected: u16) {
    // Pause in the device callback, before KVM completes the instruction.
    let mut payload = vec![
        0xba, 0x80, 0x00, // mov dx, 0x80
        0xb0, 0x11, // mov al, 0x11
    ];
    payload.extend_from_slice(instruction);
    payload.extend_from_slice(&[
        0xa2,
        0x00,
        0x90, // mov [0x9000], al
        0xeb,
        (-(instruction.len() as i8 + 5)) as u8, // repeat
    ]);
    let Some(m) = machine(&payload, 8) else {
        return;
    };
    let wait = || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while m.handle.status() != vmlab_hv_proto::control::RunState::Paused {
            assert!(std::time::Instant::now() < deadline, "I/O did not pause");
            std::thread::yield_now();
        }
        m.handle
            .shared()
            .wait_quiesced(Duration::from_secs(5))
            .unwrap();
    };
    wait();
    let socket = m._dir.path().join("control.sock");
    crate::control::serve(&socket, &m.handle).unwrap();
    let mut client = vmlab_hv_proto::client::Client::connect(&socket).unwrap();
    let call = |client: &mut vmlab_hv_proto::client::Client, command| {
        assert!(matches!(
            client.call(command).unwrap(),
            vmlab_hv_proto::control::Outcome::Ok { .. }
        ));
    };
    call(
        &mut client,
        vmlab_hv_proto::control::Command::SnapshotSave { name: "io".into() },
    );
    let regs = m.handle.shared().vcpus.lock()[0]
        .1
        .lock()
        .get_regs()
        .unwrap();
    assert_eq!(
        regs.rax & 0xff,
        expected as u64,
        "snapshot captured incomplete I/O"
    );
    assert_eq!(
        regs.rip,
        0x8005 + instruction.len() as u64,
        "snapshot captured I/O before completion"
    );
    call(&mut client, vmlab_hv_proto::control::Command::Resume);
    wait();
    call(
        &mut client,
        vmlab_hv_proto::control::Command::SnapshotLoad { name: "io".into() },
    );
    call(&mut client, vmlab_hv_proto::control::Command::Resume);
    wait();
    assert_eq!(
        m.counter() & 0xff,
        expected,
        "post-save I/O leaked into restored CPU"
    );
}

#[test]
fn snapshot_load_completes_deferred_hyperv_hypercalls() {
    // Save just before the marker store, then load while PostMessage exits
    // repeatedly. A stale completion overwrites restored registers and uses
    // KVM's cached next RIP inside the old hypercall page.
    let mut payload = vec![0xe4, 0x80, 0xb8, 0xcd, 0xab, 0xa3, 0x00, 0x90];
    let loop_at = payload.len();
    // Hyper-V's x86 convention: EDX:EAX control, EBX:ECX input GPA.
    for (register, value) in [(0xb8, 0x5cu32), (0xba, 0), (0xbb, 0), (0xb9, 0xc000)] {
        payload.extend([0x66, register]);
        payload.extend(value.to_le_bytes());
    }
    payload.push(0xe8);
    let displacement = 0xb000u16.wrapping_sub(0x8000 + payload.len() as u16 + 2);
    payload.extend(displacement.to_le_bytes());
    payload.push(0xe9);
    let displacement = (loop_at as u16).wrapping_sub(payload.len() as u16 + 2);
    payload.extend(displacement.to_le_bytes());
    let Some(m) = machine_with_setup(&payload, 8, |kvm, cpu| {
        crate::hyperv::configure_vcpu(&kvm.kvm, cpu, crate::hyperv::WINDOWS_FEATURES).unwrap();
        crate::loader::init_protected_16(cpu, 0x8000).unwrap();
        let msrs = kvm_bindings::Msrs::from_entries(&[
            kvm_bindings::kvm_msr_entry {
                index: 0x40000000,
                data: 42,
                ..Default::default()
            },
            kvm_bindings::kvm_msr_entry {
                index: 0x40000001,
                data: 0xb001,
                ..Default::default()
            },
        ])
        .unwrap();
        assert_eq!(cpu.set_msrs(&msrs).unwrap(), 2);
    }) else {
        return;
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while m.handle.status() != vmlab_hv_proto::control::RunState::Paused
        && std::time::Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    assert_eq!(m.handle.status(), vmlab_hv_proto::control::RunState::Paused);
    m.handle
        .shared()
        .wait_quiesced(Duration::from_secs(5))
        .unwrap();
    let socket = m._dir.path().join("control.sock");
    crate::control::serve(&socket, &m.handle).unwrap();
    let mut client = vmlab_hv_proto::client::Client::connect(socket).unwrap();
    let call = |client: &mut vmlab_hv_proto::client::Client, command| {
        assert!(matches!(
            client.call(command).unwrap(),
            vmlab_hv_proto::control::Outcome::Ok { .. }
        ));
    };
    call(
        &mut client,
        vmlab_hv_proto::control::Command::SnapshotSave {
            name: "hcall".into(),
        },
    );
    for cycle in 0..20 {
        call(&mut client, vmlab_hv_proto::control::Command::Resume);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(
            m.counter(),
            0xabcd,
            "guest did not reach the hypercall loop"
        );
        call(
            &mut client,
            vmlab_hv_proto::control::Command::SnapshotLoad {
                name: "hcall".into(),
            },
        );
        call(&mut client, vmlab_hv_proto::control::Command::Resume);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while m.counter() == 0 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(
            m.counter(),
            0xabcd,
            "load {cycle} completed a stale hypercall over restored registers"
        );
        call(&mut client, vmlab_hv_proto::control::Command::Pause);
        m.handle
            .shared()
            .wait_quiesced(Duration::from_secs(5))
            .unwrap();
    }
}

#[test]
fn snapshot_stops_console_dma_before_saving_ram() {
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use virtio_queue::QueueT;
    use vmlab_hv_devices::virtio::console::{Console, PortConfig};
    use vmlab_hv_devices::{DeviceError, Snapshot, VirtioDevice, VirtioInterrupt};

    struct Interrupt;
    impl VirtioInterrupt for Interrupt {
        fn notify_queue(&self, _: u16) {}
        fn notify_config(&self) {}
    }
    struct ConnectDuringSave {
        path: std::path::PathBuf,
        mem: vmlab_hv_devices::GuestMemory,
        client: PlMutex<Option<UnixStream>>,
        wrote: Arc<AtomicBool>,
    }
    impl ConnectDuringSave {
        fn connect_and_observe(&self) -> Result<(), DeviceError> {
            self.client.lock().take();
            *self.client.lock() = Some(UnixStream::connect(&self.path)?);
            // A host connection queues PORT_OPEN after the console state has
            // been captured, but before the VMM captures RAM. No guest runs.
            let deadline = std::time::Instant::now() + Duration::from_millis(200);
            while std::time::Instant::now() < deadline {
                if self.mem.read_obj::<u32>(GuestAddress(0x9000)).unwrap() != 0 {
                    self.wrote.store(true, Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        }
    }
    impl Snapshot for ConnectDuringSave {
        fn state_id(&self) -> String {
            "connect-during-save".into()
        }
        fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
            self.connect_and_observe()?;
            Ok(Vec::new())
        }
        fn restore_state(&mut self, _: &[u8]) -> Result<(), DeviceError> {
            self.connect_and_observe()
        }
    }
    let Some(m) = machine(&[0xe4, 0x80, 0xeb, 0xfc], 8) else {
        return;
    };
    m.handle
        .shared()
        .wait_quiesced(Duration::from_secs(5))
        .unwrap();
    let path = m._dir.path().join("console.sock");
    let mut console = Console::new(
        "console",
        vec![PortConfig {
            name: "test".into(),
            path: path.clone(),
        }],
    )
    .unwrap();
    let queues = console
        .queue_max_sizes()
        .iter()
        .enumerate()
        .map(|(i, &size)| {
            let mut queue = virtio_queue::Queue::new(size).unwrap();
            queue.set_size(size);
            queue.set_desc_table_address(Some(0x10000 + i as u32 * 0x10000), Some(0));
            queue.set_avail_ring_address(Some(0x11000 + i as u32 * 0x10000), Some(0));
            queue.set_used_ring_address(Some(0x12000 + i as u32 * 0x10000), Some(0));
            queue.set_ready(true);
            queue
        })
        .collect();
    let mem = m.vm.ram.mem.clone();
    mem.write_obj(0x9000u64, GuestAddress(0x30000)).unwrap();
    mem.write_obj(64u32, GuestAddress(0x30008)).unwrap();
    mem.write_obj(2u16, GuestAddress(0x3000c)).unwrap();
    mem.write_obj(1u16, GuestAddress(0x31002)).unwrap();
    console
        .activate(mem.clone(), Arc::new(Interrupt), queues)
        .unwrap();
    let wrote = Arc::new(AtomicBool::new(false));
    let original = m.handle.shared().snapshot_registry().unwrap();
    m.handle
        .shared()
        .set_snapshot_registry(Arc::new(SnapshotRegistry {
            digest: original.digest,
            disks: original
                .disks
                .iter()
                .map(|disk| DiskNode {
                    id: disk.id.clone(),
                    writable: disk.writable,
                    vmstate: disk.vmstate,
                    image: disk.image.clone(),
                })
                .collect(),
            nodes: vec![
                DeviceNode::Plain(Arc::new(PlMutex::new(console))),
                DeviceNode::Plain(Arc::new(PlMutex::new(ConnectDuringSave {
                    path,
                    mem,
                    client: PlMutex::new(None),
                    wrote: wrote.clone(),
                }))),
            ],
        }));
    let socket = m._dir.path().join("control.sock");
    crate::control::serve(&socket, &m.handle).unwrap();
    let mut client = vmlab_hv_proto::client::Client::connect(socket).unwrap();
    assert!(matches!(
        client
            .call(vmlab_hv_proto::control::Command::SnapshotSave { name: "dma".into() })
            .unwrap(),
        vmlab_hv_proto::control::Outcome::Ok { .. }
    ));
    assert!(
        !wrote.load(Ordering::SeqCst),
        "console DMA changed RAM after its queue indices were captured"
    );
    let wait_for_dma = || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while m.vm.ram.mem.read_obj::<u32>(GuestAddress(0x9000)).unwrap() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "console did not resume DMA"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    wait_for_dma();
    wrote.store(false, Ordering::SeqCst);
    assert!(matches!(
        client
            .call(vmlab_hv_proto::control::Command::SnapshotLoad { name: "dma".into() })
            .unwrap(),
        vmlab_hv_proto::control::Outcome::Ok { .. }
    ));
    assert!(
        !wrote.load(Ordering::SeqCst),
        "console DMA resumed before every device was restored"
    );
    wait_for_dma();
}

#[test]
fn rapid_snapshot_loads_roll_back_guest_tsc() {
    // IN pauses before RDTSC; each resume stores EDX:EAX and pauses again.
    let payload = [
        0xe4, 0x80, 0x0f, 0x31, 0x66, 0xa3, 0x00, 0x90, 0x66, 0x89, 0x16, 0x04, 0x90, 0xeb, 0xf1,
    ];
    let Some(m) = machine(&payload, 8) else {
        return;
    };
    let wait = || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while m.handle.status() != vmlab_hv_proto::control::RunState::Paused {
            assert!(std::time::Instant::now() < deadline, "guest did not pause");
            std::thread::yield_now();
        }
        m.handle
            .shared()
            .wait_quiesced(Duration::from_secs(5))
            .unwrap();
    };
    wait();
    let cpus = m.handle.shared().vcpus.lock().clone();
    let khz = cpus[0].1.lock().get_tsc_khz().unwrap() as u64;
    let socket = m._dir.path().join("control.sock");
    crate::control::serve(&socket, &m.handle).unwrap();
    let mut client = vmlab_hv_proto::client::Client::connect(socket).unwrap();
    let call = |client: &mut vmlab_hv_proto::client::Client, command| {
        assert!(matches!(
            client.call(command).unwrap(),
            vmlab_hv_proto::control::Outcome::Ok { .. }
        ));
    };
    call(
        &mut client,
        vmlab_hv_proto::control::Command::SnapshotSave { name: "tsc".into() },
    );
    let mut tsc = kvm_bindings::Msrs::from_entries(&[kvm_bindings::kvm_msr_entry {
        index: 0x10,
        ..Default::default()
    }])
    .unwrap();
    assert_eq!(cpus[0].1.lock().get_msrs(&mut tsc).unwrap(), 1);
    let saved = tsc.as_slice()[0].data;
    for cycle in 0..3 {
        std::thread::sleep(Duration::from_millis(100));
        call(
            &mut client,
            vmlab_hv_proto::control::Command::SnapshotLoad { name: "tsc".into() },
        );
        call(&mut client, vmlab_hv_proto::control::Command::Resume);
        wait();
        let mut bytes = [0; 8];
        m.vm.ram
            .mem
            .read_slice(&mut bytes, GuestAddress(0x9000))
            .unwrap();
        let observed = u64::from_le_bytes(bytes);
        assert!(
            observed.abs_diff(saved) < khz * 50,
            "load {cycle}: guest TSC drifted {} ms",
            observed.abs_diff(saved) / khz
        );
    }
}

#[test]
fn snapshot_restores_tsc_deadline_after_lapic() {
    use kvm_bindings::{Msrs, kvm_msr_entry};
    let Some(m) = machine(&counter_payload(), 8) else {
        return;
    };
    m.handle.pause().unwrap();
    m.handle
        .shared()
        .wait_quiesced(Duration::from_secs(5))
        .unwrap();
    let fds = m.handle.shared().vcpus.lock().clone();
    let fd = fds[0].1.lock();
    let cpuid = fd.get_cpuid2(256).unwrap();
    if !cpuid
        .as_slice()
        .iter()
        .any(|entry| entry.function == 1 && entry.ecx & (1 << 24) != 0)
    {
        eprintln!("SKIP: KVM does not expose TSC-deadline mode");
        return;
    }
    let mut lapic = fd.get_lapic().unwrap();
    for (offset, value) in [(0xf0, 0x1ffu32), (0x320, (2 << 17) | 0x40)] {
        for (byte, value) in lapic.regs[offset..offset + 4]
            .iter_mut()
            .zip(value.to_le_bytes())
        {
            *byte = value as i8;
        }
    }
    fd.set_lapic(&lapic).unwrap();
    let deadline = u64::MAX / 2;
    let msrs = Msrs::from_entries(&[kvm_msr_entry {
        index: 0x6e0,
        data: deadline,
        ..Default::default()
    }])
    .unwrap();
    assert_eq!(fd.set_msrs(&msrs).unwrap(), 1);
    drop(fd);
    let socket = m._dir.path().join("control.sock");
    crate::control::serve(&socket, &m.handle).unwrap();
    let mut client = vmlab_hv_proto::client::Client::connect(socket).unwrap();
    for (index, command) in [
        vmlab_hv_proto::control::Command::SnapshotSave {
            name: "deadline".into(),
        },
        vmlab_hv_proto::control::Command::SnapshotLoad {
            name: "deadline".into(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        assert!(matches!(
            client.call(command).unwrap(),
            vmlab_hv_proto::control::Outcome::Ok { .. }
        ));
        if index == 0 {
            let changed = Msrs::from_entries(&[kvm_msr_entry {
                index: 0x6e0,
                data: 0,
                ..Default::default()
            }])
            .unwrap();
            assert_eq!(fds[0].1.lock().set_msrs(&changed).unwrap(), 1);
        }
    }
    let mut msrs = Msrs::from_entries(&[kvm_msr_entry {
        index: 0x6e0,
        ..Default::default()
    }])
    .unwrap();
    assert_eq!(fds[0].1.lock().get_msrs(&mut msrs).unwrap(), 1);
    assert_eq!(
        msrs.as_slice()[0].data,
        deadline,
        "restore disarmed the guest timer"
    );
}
