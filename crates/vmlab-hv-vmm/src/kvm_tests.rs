//! KVM integration tests.
//!
//! These boot tiny real-mode payloads (hand-assembled machine code, no
//! kernel) that write to COM1 and then either acknowledge an ACPI power
//! button press with S5 or triple-fault, plus one black-box test that boots
//! vmlab's real container kernel through the full builder path. Every test
//! skips with a printed reason when `/dev/kvm` is missing.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex as PlMutex;
use parking_lot::Mutex;
use vmlab_hv_devices::{BusDevice, IrqLine};
use vmlab_hv_proto::control::{Command, Event, Outcome, ShutdownReason};
use vmlab_hv_proto::machine::{Arch, Accel, Boot, MachineDescription};
use vm_memory::{Bytes, GuestAddress};

use crate::builder;
use crate::bus::{IoBus, MmioBus};
use crate::error::DeviceUnavailable;
use crate::factory::DeviceFactory;
use crate::kvm::KvmVm;
use crate::layout::MemLayout;
use crate::memory::{register_memslots, GuestRam};
use crate::pci::PciRoot;
use crate::pm::{PmDevice, SleepSignal, PM1A_EVT_BASE, PM_BLOCK_LEN};
use crate::vm::{ExitReason, Vm, VmHandle, VmShared};

/// How long a payload test waits for the guest before forcing a host exit.
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the payload lives; real mode after reset runs from 0xFFFF0.
const PAYLOAD_ADDR: u64 = 0x8000;
const RESET_VECTOR: u64 = 0xFFFF0;

/// A fake 16550A: records every byte written to the port range.
struct FakeSerial {
    out: Arc<PlMutex<Vec<u8>>>,
}

impl BusDevice for FakeSerial {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        // Line status: transmitter empty and ready, no received data.
        if offset == 5 {
            data.fill(0x60);
        } else {
            data.fill(0);
        }
    }

    fn write(&mut self, _offset: u64, data: &[u8]) {
        self.out.lock().extend_from_slice(data);
    }
}

/// A factory whose serial is a [`FakeSerial`]; everything else reports the
/// model as not landed, exactly like the binary's factory until the device
/// crates' work packages arrive.
struct TestFactory {
    out: Arc<PlMutex<Vec<u8>>>,
}

impl TestFactory {
    fn new() -> (Self, Arc<PlMutex<Vec<u8>>>) {
        let out = Arc::new(PlMutex::new(Vec::new()));
        (TestFactory { out: out.clone() }, out)
    }
}

impl DeviceFactory for TestFactory {
    fn serial(
        &self,
        _log_path: &Path,
        _irq: Arc<dyn IrqLine>,
    ) -> Result<Arc<Mutex<dyn BusDevice>>, DeviceUnavailable> {
        Ok(Arc::new(parking_lot::Mutex::new(FakeSerial { out: self.out.clone() })))
    }

    fn virtio_blk(
        &self,
        disk: &vmlab_hv_proto::machine::Disk,
        _msi: Arc<dyn vmlab_hv_devices::MsiSink>,
    ) -> Result<Arc<Mutex<dyn vmlab_hv_devices::PciDevice>>, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-blk",
            format!("no block device in this test (disk `{}`)", disk.id),
        ))
    }

    fn virtio_console(
        &self,
        _ports: &[vmlab_hv_proto::machine::SerialPort],
        _msi: Arc<dyn vmlab_hv_devices::MsiSink>,
    ) -> Result<Arc<Mutex<dyn vmlab_hv_devices::PciDevice>>, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-console",
            "no console device in this test",
        ))
    }

    fn virtio_net(
        &self,
        nic: &vmlab_hv_proto::machine::Nic,
        _msi: Arc<dyn vmlab_hv_devices::MsiSink>,
    ) -> Result<Arc<Mutex<dyn vmlab_hv_devices::PciDevice>>, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-net",
            format!("no net device in this test (nic {})", nic.mac),
        ))
    }

    fn vhost_user_fs(
        &self,
        share: &vmlab_hv_proto::machine::FsShare,
        _msi: Arc<dyn vmlab_hv_devices::MsiSink>,
    ) -> Result<Arc<Mutex<dyn vmlab_hv_devices::PciDevice>>, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "vhost-user-fs",
            format!("no fs device in this test (share `{}`)", share.tag),
        ))
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

/// A real-mode payload: writes the message to COM1, then polls PM1a_STS for
/// the power button and answers S5.
fn s5_payload() -> Vec<u8> {
    let mut code = vec![
        0xBE, 0x40, 0x80, // mov si, 0x8040
        0xAC, // .next: lodsb
        0x84, 0xC0, // test al, al
        0x74, 0x06, // jz .poll
        0xBA, 0xF8, 0x03, // mov dx, 0x3F8
        0xEE, // out dx, al
        0xEB, 0xF5, // jmp .next
        0xBA, 0x00, 0x06, // .poll: mov dx, 0x600
        0xED, // in ax, dx
        0xA9, 0x00, 0x01, // test ax, 0x0100 (PWRBTN_STS)
        0x74, 0xF7, // jz .poll
        0xB8, 0x00, 0x01, // mov ax, 0x0100 (W1C)
        0xEF, // out dx, ax
        0xBA, 0x04, 0x06, // mov dx, 0x604
        0xB8, 0x00, 0x34, // mov ax, 0x3400 (SLP_TYP=5 | SLP_EN)
        0xEF, // out dx, ax
        0xF4, // .halt: hlt
        0xEB, 0xFD, // jmp .halt
    ];
    code.extend_from_slice(&[0u8; 0x40 - 0x25]);
    code.extend_from_slice(b"hello-s5\n\0");
    code
}

/// A real-mode payload: writes the message to COM1, then executes `ud2`,
/// which faults through an empty real-mode IVT into a triple fault.
fn triple_fault_payload() -> Vec<u8> {
    let mut code = vec![
        0xBE, 0x40, 0x80, // mov si, 0x8040
        0xAC, // .next: lodsb
        0x84, 0xC0, // test al, al
        0x74, 0x08, // jz .boom
        0xBA, 0xF8, 0x03, // mov dx, 0x3F8
        0xEE, // out dx, al
        0xEB, 0xF5, // jmp .next
        0x0F, 0x0B, // .boom: ud2
    ];
    code.extend_from_slice(&[0u8; 0x40 - 0x12]);
    code.extend_from_slice(b"triple\0");
    code
}

/// Assemble a machine that boots `payload` in real mode: no kernel, no ACPI
/// tables, one vCPU at its reset state. The payload lands at 0x8000 and a
/// far jump at the reset vector enters it.
fn payload_vm(payload: &[u8], paused: bool) -> Option<(Vm, VmHandle, Arc<PlMutex<Vec<u8>>>)> {
    let kvm = kvm_or_skip()?;
    let layout = MemLayout::new(16).ok()?;
    let ram = GuestRam::anonymous(layout.clone()).ok()?;
    register_memslots(&kvm.vm, &ram).ok()?;

    // Reset vector: far jump to 0x0000:0x8000.
    ram.mem
        .write_slice(&[0xEA, 0x00, 0x80, 0x00, 0x00], GuestAddress(RESET_VECTOR))
        .ok()?;
    ram.mem
        .write_slice(payload, GuestAddress(PAYLOAD_ADDR))
        .ok()?;

    let (factory, out) = TestFactory::new();
    let mut io_bus = IoBus::new();
    let mmio_bus = MmioBus::new();
    let pci_root = Arc::new(PlMutex::new(PciRoot::new(layout.mmio64_base)));

    let serial = factory
        .serial(Path::new("/dev/null"), Arc::new(NullIrq))
        .ok()?;
    io_bus.register(0x3F8, 8, serial).ok()?;

    let sleep_signal = Arc::new(SleepSignal::default());
    let pm = Arc::new(PlMutex::new(PmDevice::new(
        Arc::new(NullIrq),
        sleep_signal.sink(),
    )));
    io_bus.register(PM1A_EVT_BASE, PM_BLOCK_LEN, pm.clone()).ok()?;

    let shared = VmShared::new(
        kvm,
        io_bus,
        mmio_bus,
        pci_root,
        pm,
        sleep_signal,
        layout.mmio64_base,
    );
    let vcpu = shared.kvm.create_vcpu(0).ok()?;
    let mut vm = Vm::from_parts(shared, vec![(0, vcpu)], ram).ok()?;
    if !paused {
        vm.start();
    }
    let handle = vm.handle();
    Some((vm, handle, out))
}

/// An IrqLine that records nothing; the payloads never take interrupts.
struct NullIrq;

impl IrqLine for NullIrq {
    fn set_level(&self, _high: bool) {}
}

/// Force the machine to a host-request exit if the guest never finishes, so
/// a broken payload fails the test instead of hanging it.
fn watchdog(handle: &VmHandle) {
    let handle = handle.clone();
    std::thread::spawn(move || {
        std::thread::sleep(TEST_TIMEOUT);
        handle.quit();
    });
}

fn temp_socket(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "vmlab-hv-kvm-tests-{}-{}",
        std::process::id(),
        name
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("control.sock")
}

#[test]
fn guest_s5_after_powerdown_is_guest_shutdown() {
    let Some((vm, handle, serial)) = payload_vm(&s5_payload(), false) else {
        return;
    };
    watchdog(&handle);
    handle.powerdown().expect("powerdown");

    let reason = vm.wait();
    assert_eq!(
        reason,
        ExitReason::Shutdown(ShutdownReason::GuestShutdown),
        "S5 must end the run as guest_shutdown"
    );
    assert_eq!(*serial.lock(), b"hello-s5\n");
}

#[test]
fn triple_fault_is_guest_reset() {
    let Some((vm, _handle, serial)) = payload_vm(&triple_fault_payload(), false) else {
        return;
    };
    watchdog(&_handle);

    let reason = vm.wait();
    assert_eq!(
        reason,
        ExitReason::Shutdown(ShutdownReason::GuestReset),
        "a triple fault (reboot=t, panic=-1) must end as guest_reset"
    );
    assert_eq!(*serial.lock(), b"triple");
}

#[test]
fn paused_machine_resumes_and_reports_events() {
    let Some((mut vm, handle, serial)) = payload_vm(&s5_payload(), true) else {
        return;
    };
    assert_eq!(handle.status(), vmlab_hv_proto::control::RunState::Paused);
    // A paused vCPU executes nothing: the serial log stays empty.
    std::thread::sleep(Duration::from_millis(100));
    assert!(serial.lock().is_empty());

    vm.start();
    assert_eq!(handle.status(), vmlab_hv_proto::control::RunState::Running);
    handle.powerdown().expect("powerdown");
    watchdog(&handle);

    assert_eq!(
        vm.wait(),
        ExitReason::Shutdown(ShutdownReason::GuestShutdown)
    );
    assert_eq!(*serial.lock(), b"hello-s5\n");
}

#[test]
fn control_socket_end_to_end() {
    let Some((vm, handle, serial)) = payload_vm(&s5_payload(), false) else {
        return;
    };
    watchdog(&handle);

    let socket = temp_socket("e2e");
    crate::control::serve(&socket, &handle).expect("serve control socket");

    let mut client = vmlab_hv_proto::client::Client::connect(&socket).expect("handshake");
    assert_eq!(client.proto_version(), vmlab_hv_proto::PROTO_VERSION);
    assert!(client.capabilities().is_empty());

    match client.call(Command::Status).expect("status reply") {
        Outcome::Ok {
            status: Some(vmlab_hv_proto::control::RunState::Running),
        } => {}
        other => panic!("expected running status, got {other:?}"),
    }

    match client.call(Command::SnapshotSave {
        name: "nope".into(),
    })
    .expect("snapshot reply")
    {
        Outcome::Error {
            code: vmlab_hv_proto::control::ErrorCode::Unsupported,
            ..
        } => {}
        other => panic!("expected Unsupported, got {other:?}"),
    }

    match client.call(Command::Powerdown).expect("powerdown reply") {
        Outcome::Ok { .. } => {}
        other => panic!("expected ok, got {other:?}"),
    }

    // The shutdown event arrives unsolicited on the same connection.
    match client.wait_event().expect("shutdown event") {
        Some(Event::Shutdown {
            reason: ShutdownReason::GuestShutdown,
        }) => {}
        other => panic!("expected shutdown event, got {other:?}"),
    }

    assert_eq!(
        vm.wait(),
        ExitReason::Shutdown(ShutdownReason::GuestShutdown)
    );
    assert_eq!(*serial.lock(), b"hello-s5\n");
}

/// Black-box check of the whole builder path with vmlab's real container
/// kernel: boots, prints to the serial log, finds no rootfs, and `panic=-1`
/// + `reboot=t` turn the panic into the triple-fault reset this VMM reports
/// as `guest_reset`. Skips when the kernel asset is not on this host.
#[test]
fn real_kernel_boots_and_resets() {
    const KERNEL: &str = "/home/wil/dev/vmlab/guest/dist/x86_64/vmlinuz";
    if !Path::new(KERNEL).exists() {
        eprintln!("skipping: container kernel {KERNEL} is not present");
        return;
    }
    let Some(kvm) = kvm_or_skip() else {
        return;
    };
    drop(kvm); // builder opens its own; this only gates on availability.

    let (factory, serial) = TestFactory::new();
    let dir = std::env::temp_dir().join(format!("vmlab-hv-kvm-tests-kernel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let desc = MachineDescription {
        name: "kernel-test".into(),
        arch: Arch::X86_64,
        accel: Accel::Kvm,
        vcpus: 1,
        memory_mib: 512,
        boot: Boot::Kernel {
            kernel: KERNEL.into(),
            initrd: None,
            cmdline: "console=ttyS0 quiet loglevel=3 panic=-1 reboot=t".into(),
        },
        disks: vec![],
        nics: vec![],
        serial_log: dir.join("serial.log"),
        ports: vec![],
        shares: vec![],
        rtc_base: None,
    };

    let vm = builder::build(&desc, &factory, false).expect("build the machine");
    let handle = vm.handle();
    watchdog(&handle);

    let reason = vm.wait();
    let log = String::from_utf8_lossy(&serial.lock()).to_string();
    assert!(
        log.contains("Linux version"),
        "the kernel must reach early boot; serial log: {log}"
    );
    assert_eq!(
        reason,
        ExitReason::Shutdown(ShutdownReason::GuestReset),
        "panic=-1 + reboot=t must end as guest_reset; serial log: {log}"
    );
}

/// S3 through the PM block suspends the machine without ending the run.
#[test]
fn guest_s3_suspends() {
    let mut code = Vec::new();
    // Wait for SCI-less S3: write SLP_TYP=3 | SLP_EN to PM1a_CNT directly.
    code.extend_from_slice(&[
        0xBA, 0x04, 0x06, // mov dx, 0x604
        0xB8, 0x00, 0x1C, // mov ax, 0x1C00 (SLP_TYP=3 | SLP_EN)
        0xEF, // out dx, ax
        0xF4, // hlt
        0xEB, 0xFD, // jmp
    ]);
    let Some((vm, handle, _serial)) = payload_vm(&code, false) else {
        return;
    };
    watchdog(&handle);

    // Poll for Suspended rather than blocking on wait_exit, which never
    // fires: a suspend is not an exit.
    let mut suspended = false;
    for _ in 0..100 {
        if handle.status() == vmlab_hv_proto::control::RunState::Suspended {
            suspended = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(suspended, "guest S3 must report Suspended");
    // Resume from suspend is a wakeup, and the run continues.
    assert_eq!(
        handle.resume().map(|s| s == vmlab_hv_proto::control::RunState::Running),
        Ok(true)
    );
    // Stop the machine so the test ends.
    handle.quit();
    assert_eq!(
        vm.wait(),
        ExitReason::Shutdown(ShutdownReason::HostRequest)
    );
}
