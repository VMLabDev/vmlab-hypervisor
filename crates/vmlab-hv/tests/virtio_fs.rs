//! KVM tests for the in-process virtio-fs device.
//!
//! These boot vmlab's container kernel and initramfs on a machine with one
//! share whose `path` the hypervisor serves itself (no virtiofsd): the
//! guest mounts it, reads and writes files, and a snapshot taken while a
//! file descriptor is open restores with that descriptor still readable.
//! The unit tests in `vmlab-hv-devices::virtio::fs` prove the FUSE ops and
//! the snapshot encoding; these prove the whole device behind the control
//! socket (S1).
//!
//! Every test skips with a printed reason when `/dev/kvm` or the kernel and
//! initramfs assets are missing.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use vmlab_hv_devices::virtio::fs::{InProcessFs, InProcessFsConfig};
use vmlab_hv_devices::virtio::pci::VirtioPciDevice;
use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::Command as Ctrl;
use vmlab_hv_proto::machine::{Accel, Arch, Boot, FsShare, MachineDescription};
use vmlab_hv_vmm::error::DeviceUnavailable;
use vmlab_hv_vmm::factory::{BuiltPci, PciWiring};
use vmlab_hv_vmm::{DeviceFactory, Vm, builder};

const KERNEL: &str = "/home/wil/dev/vmlab/guest/dist/x86_64/vmlinuz";
const INITRD: &str = "/home/wil/dev/vmlab/guest/dist/x86_64/initramfs.img";
const TAG: &str = "scratch";
/// How long a boot has to reach the virtio-fs probe.
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);

fn or_skip() -> Option<()> {
    let kvm = Path::new("/dev/kvm").exists();
    let assets = Path::new(KERNEL).is_file() && Path::new(INITRD).is_file();
    if kvm && assets {
        return Some(());
    }
    let why = if kvm {
        "the container kernel or initramfs is missing"
    } else {
        "/dev/kvm is missing"
    };
    eprintln!("skipping: {why}");
    None
}

/// A scratch directory removed on drop.
struct Dir {
    path: PathBuf,
}

impl Dir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "hv-virtio-fs-{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        Dir { path }
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A factory whose serial is the real 16550A into the machine's log file
/// and whose PCI devices are the real virtio block and in-process fs.
struct TestFactory;

impl DeviceFactory for TestFactory {
    fn serial(
        &self,
        log_path: &Path,
        irq: Arc<dyn vmlab_hv_devices::IrqLine>,
    ) -> Result<vmlab_hv_vmm::factory::BuiltSerial, DeviceUnavailable> {
        let file = File::options()
            .create(true)
            .append(true)
            .open(log_path)
            .map_err(|e| DeviceUnavailable::new("serial-16550a", format!("{e}")))?;
        let dev = Arc::new(Mutex::new(vmlab_hv_devices::serial::Serial::new(
            "serial0",
            vmlab_hv_devices::serial::SerialOut::new(Box::new(file)),
            irq,
        )));
        Ok(vmlab_hv_vmm::factory::BuiltSerial {
            device: dev.clone(),
            snapshot: Some(dev),
        })
    }

    fn virtio_blk(
        &self,
        disk: &vmlab_hv_proto::machine::Disk,
        mem: &vmlab_hv_devices::GuestMemory,
        msi: PciWiring,
    ) -> Result<vmlab_hv_vmm::factory::BuiltBlk, DeviceUnavailable> {
        // A real block device: online snapshots need a writable vmstate
        // disk to land on.
        let image = vmlab_hv_vmm::factory::open_image(disk)?;
        let block = vmlab_hv_devices::virtio::block::Block::new(
            disk.id.clone(),
            Box::new(vmlab_hv_vmm::factory::SharedDisk(image.clone())),
        );
        let pci = Arc::new(Mutex::new(VirtioPciDevice::new(
            disk.id.clone(),
            Box::new(block),
            mem.clone(),
            Some(msi.msi),
            None,
        )));
        Ok(vmlab_hv_vmm::factory::BuiltBlk {
            pci: vmlab_hv_vmm::factory::BuiltPci {
                function: pci,
                snapshot: None,
            },
            disk: image,
        })
    }

    fn virtio_console(
        &self,
        _ports: &[vmlab_hv_proto::machine::SerialPort],
        _mem: &vmlab_hv_devices::GuestMemory,
        _msi: PciWiring,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-console",
            "no console in this test",
        ))
    }

    fn virtio_net(
        &self,
        nic: &vmlab_hv_proto::machine::Nic,
        _mem: &vmlab_hv_devices::GuestMemory,
        _msi: PciWiring,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-net",
            format!("no net device in this test (nic {})", nic.mac),
        ))
    }

    fn fs_share(
        &self,
        share: &FsShare,
        mem: &vmlab_hv_devices::GuestMemory,
        msi: PciWiring,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        let id = format!("fs-{}", share.tag);
        let Some(path) = share.path.clone() else {
            return Err(DeviceUnavailable::new(
                "virtio-fs",
                format!("share `{}` is not an in-process path", share.tag),
            ));
        };
        let fs = InProcessFs::new(
            id.clone(),
            InProcessFsConfig {
                tag: share.tag.clone(),
                path,
                read_only: share.read_only,
            },
        )
        .map_err(|e| DeviceUnavailable::new("virtio-fs", format!("share `{}`: {e}", share.tag)))?;
        let pci = Arc::new(Mutex::new(VirtioPciDevice::new(
            id,
            Box::new(fs),
            mem.clone(),
            Some(msi.msi),
            None,
        )));
        Ok(BuiltPci {
            function: pci,
            snapshot: None,
        })
    }
}

/// An initramfs overlay whose `/init` mounts the share, reads and writes
/// through it, and then holds a file descriptor open across snapshots while
/// a loop keeps reading it to the serial log.
fn initrd_with_init_script(dir: &Dir) -> PathBuf {
    let overlay = dir.path.join("overlay");
    fs::create_dir_all(&overlay).unwrap();
    // The initramfs ships busybox without applet symlinks, so every tool is
    // called through the busybox binary itself.
    let script = format!(
        "#!/bin/sh\necho VMLAB-FS-BOOT\n/bin/busybox modprobe virtiofs\n\
         /bin/busybox mkdir -p /mnt\n/bin/busybox mount -t virtiofs {TAG} /mnt\n\
         echo MOUNT-OK\n/bin/busybox cat /mnt/hello.txt\n\
         echo written-by-guest > /mnt/guest.txt\necho WRITE-OK\n\
         exec 3</mnt/hello.txt\n\
         while :; do\n\
           if IFS= read -r line <&3; then echo \"FD:$line\"; \
           else exec 3</mnt/hello.txt; echo REOPEN; /bin/busybox sleep 1; fi\n\
         done\n"
    );
    let init = overlay.join("init");
    fs::write(&init, script).unwrap();
    let mut perms = fs::metadata(&init).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&init, perms).unwrap();
    let archive = dir.path.join("overlay.cpio");
    let mut child = Command::new("cpio")
        .current_dir(&overlay)
        .args(["-o", "-H", "newc", "--owner", "0:0", "-F"])
        .arg(&archive)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn cpio");
    {
        let mut stdin = child.stdin.take().expect("cpio stdin");
        stdin
            .write_all(b".\n./init\n")
            .expect("feed cpio the file list");
    }
    let status = child.wait().expect("wait for cpio to finish the archive");
    assert!(
        status.success(),
        "cpio failed to build {archive:?}: {status}"
    );
    let combined = dir.path.join("initrd.img");
    let mut out = File::create(&combined).unwrap();
    let mut base = File::open(INITRD).unwrap();
    std::io::copy(&mut base, &mut out).unwrap();
    let mut extra = File::open(&archive).unwrap();
    std::io::copy(&mut extra, &mut out).unwrap();
    out.flush().unwrap();
    combined
}

/// One machine description with a single in-process share on `path`.
fn machine(dir: &Dir, share: &Path, initrd: &Path, read_only: bool) -> MachineDescription {
    MachineDescription {
        machine: Default::default(),
        cpu: Default::default(),
        hyperv: vec![],
        display: None,
        tablet: None,
        tpm: None,
        console_pci: None,
        ahci_pci: None,
        name: "virtio-fs-test".into(),
        arch: Arch::X86_64,
        accel: Accel::Kvm,
        vcpus: 1,
        memory_mib: 256,
        boot: Boot::Kernel {
            kernel: KERNEL.into(),
            initrd: Some(initrd.to_path_buf()),
            // Loud enough for the virtio-fs probe to reach the log.
            cmdline: "console=ttyS0 loglevel=7 panic=-1 reboot=t".into(),
        },
        disks: vec![vmlab_hv_proto::machine::Disk {
            id: "vmstate".into(),
            path: Some(dir.path.join("vmstate.qcow2")),
            format: vmlab_hv_proto::machine::DiskFormat::Qcow2,
            read_only: false,
            vmstate: true,
            bus: vmlab_hv_proto::machine::DiskBus::Virtio,
            cdrom: false,
            port: None,
            pci: None,
        }],
        nics: vec![],
        serial_log: dir.path.join("serial.log"),
        ports: vec![],
        shares: vec![FsShare {
            pci: None,
            tag: TAG.into(),
            socket: None,
            path: Some(share.to_path_buf()),
            read_only,
        }],
        audio: None,
        rtc_base: None,
    }
}

/// Poll the serial log until it contains `needle` beyond `min_len` bytes,
/// then return it.
fn wait_for_serial_after(log: &Path, needle: &str, min_len: usize) -> String {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        let text = fs::read_to_string(log).unwrap_or_default();
        if text.len() > min_len && text[min_len..].contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "serial log never contained {needle:?} past byte {min_len}; log so far:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_serial(log: &Path, needle: &str) -> String {
    wait_for_serial_after(log, needle, 0)
}

/// Stop a machine at the end of a test and hand back nothing.
fn teardown(vm: Vm) {
    vm.handle().quit();
    vm.wait();
}

/// Boot the container guest with one in-process share prepared at `share`.
fn boot(dir: &Dir, share: &Path, read_only: bool) -> (Vm, PathBuf) {
    vmlab_disk::ops::create_qcow2(&dir.path.join("vmstate.qcow2"), 64 << 20, None, None).unwrap();
    let initrd = initrd_with_init_script(dir);
    let factory = TestFactory;
    let description = machine(dir, share, &initrd, read_only);
    let vm = match builder::build(&description, &factory, false) {
        Ok(vm) => vm,
        Err(e) => panic!("build the machine: {e}"),
    };
    (vm, description.serial_log)
}

#[test]
fn guest_mounts_reads_and_writes_the_share() {
    let Some(()) = or_skip() else {
        return;
    };
    let _permit = vmlab_hv_tests::process::VmPermit::acquire().unwrap();
    let dir = Dir::new("rw");
    let share = dir.path.join("share");
    fs::create_dir(&share).unwrap();
    fs::write(share.join("hello.txt"), b"from the host share\n").unwrap();

    let (vm, log) = boot(&dir, &share, false);

    // Mount, read and write: the guest's virtio-fs driver drives the
    // in-process device end to end.
    let text = wait_for_serial(&log, "WRITE-OK");
    assert!(
        text.contains("MOUNT-OK"),
        "the share must mount; log:\n{text}"
    );
    assert!(
        text.contains("from the host share"),
        "the guest must read the shared file; log:\n{text}"
    );
    // The guest's write reached the host directory through the device.
    let written = fs::read_to_string(share.join("guest.txt")).unwrap_or_default();
    assert_eq!(written.trim(), "written-by-guest");
    teardown(vm);
}

#[test]
fn snapshot_with_an_open_file_restores_readable() {
    let Some(()) = or_skip() else {
        return;
    };
    let _permit = vmlab_hv_tests::process::VmPermit::acquire().unwrap();
    let dir = Dir::new("snap");
    let share = dir.path.join("share");
    fs::create_dir(&share).unwrap();
    fs::write(share.join("hello.txt"), b"open across snapshots\n").unwrap();

    let (vm, log) = boot(&dir, &share, false);
    // The guest holds the file open on fd 3 and keeps reading it; the first
    // FD line proves the loop is live before anything is saved.
    let before = wait_for_serial(&log, "FD:open across snapshots");
    assert!(
        before.contains("REOPEN"),
        "the read loop must cycle before the snapshot; log:\n{before}"
    );

    // The whole flow rides the control socket: save with the descriptor
    // open, then restore the same machine and resume.
    let control = dir.path.join("control.sock");
    vmlab_hv_vmm::control::serve(&control, &vm.handle()).expect("serve control");
    let mut client = Client::connect(&control).expect("connect control");
    for command in [
        Ctrl::SnapshotSave {
            name: "open-file".into(),
        },
        Ctrl::Pause,
        Ctrl::SnapshotLoad {
            name: "open-file".into(),
        },
        Ctrl::Resume,
    ] {
        let outcome = client.call(command).expect("control call");
        assert!(
            matches!(outcome, vmlab_hv_proto::control::Outcome::Ok { .. }),
            "control command failed: {outcome:?}"
        );
    }

    // After the restore the device's session was rebuilt by re-walking the
    // share, and the guest's open descriptor still reads.
    let after = wait_for_serial_after(&log, "FD:open across snapshots", before.len());
    assert!(
        after[before.len()..].contains("FD:open across snapshots"),
        "the open descriptor must keep reading after restore; new log:\n{}",
        &after[before.len()..]
    );
    // The host file is still itself, through the restored session.
    assert_eq!(
        fs::read_to_string(share.join("hello.txt")).unwrap(),
        "open across snapshots\n"
    );
    drop(client);
    teardown(vm);
}
