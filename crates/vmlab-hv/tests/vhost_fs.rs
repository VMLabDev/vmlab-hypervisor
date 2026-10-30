//! KVM tests for the vhost-user-fs front-end against a real virtiofsd.
//!
//! These boot vmlab's container kernel and initramfs on a machine with one
//! share, watch the guest's virtio-fs driver probe it through the serial log,
//! and prove the backend session is real by saving and restoring the device
//! through the vhost-user device-state transfer: machine A's section loads
//! into a fresh virtiofsd behind machine B, and B's guest probes the share
//! again. The unit tests in `vmlab-hv-devices::virtio::fs` observe the exact
//! vhost-user messages (SET_MEM_TABLE, vring setup, state transfer) against a
//! fake backend; these tests prove the whole thing against the real daemon.
//!
//! Every test skips with a printed reason when `/dev/kvm`, the kernel and
//! initramfs assets, or `/usr/lib/virtiofsd` are missing.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::io::Write as _;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use vmlab_hv_devices::serial::{Serial, SerialOut};
use vmlab_hv_devices::virtio::fs::{Fs, FsConfig};
use vmlab_hv_devices::virtio::pci::VirtioPciDevice;
use vmlab_hv_devices::{GuestMemory, IrqLine, MsiSink, PciSnapshot};
use vmlab_hv_proto::machine::{Accel, Arch, Boot, FsShare, MachineDescription};
use vmlab_hv_vmm::error::DeviceUnavailable;
use vmlab_hv_vmm::factory::{BuiltPci, BuiltSerial};
use vmlab_hv_vmm::{builder, DeviceFactory, Vm};

const KERNEL: &str = "/home/wil/dev/vmlab/guest/dist/x86_64/vmlinuz";
const INITRD: &str = "/home/wil/dev/vmlab/guest/dist/x86_64/initramfs.img";
const VIRTIOFSD: &str = "/usr/lib/virtiofsd";
const TAG: &str = "scratch";
/// How long a boot has to reach the virtio-fs probe.
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);

fn available() -> bool {
    [
        Path::new("/dev/kvm").exists(),
        Path::new(KERNEL).exists(),
        Path::new(INITRD).exists(),
        Path::new(VIRTIOFSD).exists(),
    ]
    .into_iter()
    .all(|yes| yes)
}

fn or_skip() -> Option<()> {
    if available() {
        Some(())
    } else {
        eprintln!(
            "skipping: needs /dev/kvm, {KERNEL}, {INITRD} and {VIRTIOFSD} on this host"
        );
        None
    }
}

/// A scratch directory removed on drop.
struct Dir {
    path: PathBuf,
}

impl Dir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "vmlab-hv-vhost-fs-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        if std::env::var_os("VHOST_FS_TEST_KEEP").is_none() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// A running virtiofsd serving one directory over one socket.
struct Virtiofsd {
    child: Child,
    socket: PathBuf,
}

impl Virtiofsd {
    /// Start virtiofsd the way vmlab does, with migration enabled when the
    /// binary has the flags (`find-paths`, handle verification).
    fn start(dir: &Dir, tag: &str, shared_dir: &Path) -> Self {
        let socket = dir.path.join(format!("{tag}.sock"));
        let log = File::create(dir.path.join(format!("{tag}.log"))).unwrap();
        let mut args = vec![
            "--socket-path".to_string(),
            socket.display().to_string(),
            "--shared-dir".to_string(),
            shared_dir.display().to_string(),
            "--cache".to_string(),
            "auto".to_string(),
            "--log-level".to_string(),
            std::env::var("VHOST_FS_TEST_LOG").unwrap_or_else(|_| "warn".into()),
        ];
        let help = Command::new(VIRTIOFSD)
            .arg("--help")
            .stderr(Stdio::piped())
            .output()
            .expect("probe virtiofsd");
        let help = String::from_utf8_lossy(&help.stderr).to_string();
        if help.contains("--migration-mode") && help.contains("--migration-verify-handles") {
            args.extend(
                [
                    "--migration-mode",
                    "find-paths",
                    "--migration-verify-handles",
                    "--migration-on-error",
                    "guest-error",
                ]
                .map(String::from),
            );
        }
        let mut child = Command::new(VIRTIOFSD)
            .args(&args)
            .stdout(log.try_clone().unwrap())
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn virtiofsd");
        // The socket appears once the daemon is ready; a bad flag shows up as
        // an early exit instead.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    panic!("virtiofsd exited at startup with {status}");
                }
                Ok(None) => {}
                Err(e) => panic!("waiting for virtiofsd: {e}"),
            }
            assert!(Instant::now() < deadline, "virtiofsd never bound {socket:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child, socket }
    }

    /// virtiofsd exits when its front-end disconnects; prove it did.
    fn expect_exit(mut self, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() > deadline => {
                    panic!("virtiofsd did not exit after {what}")
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => panic!("waiting for virtiofsd exit: {e}"),
            }
        }
    }
}

impl Drop for Virtiofsd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket);
    }
}

/// A factory whose serial is the real 16550A into a log file and whose only
/// PCI device is the real vhost-user-fs front-end; it records the function's
/// snapshot handle so the tests can drive the device-state seam.
struct TestFactory {
    log_dir: PathBuf,
    fs_snapshot: Mutex<Option<Arc<Mutex<dyn PciSnapshot>>>>,
}

impl TestFactory {
    fn new(log_dir: &Path) -> Self {
        Self {
            log_dir: log_dir.to_path_buf(),
            fs_snapshot: Mutex::new(None),
        }
    }

    fn serial_log(&self) -> PathBuf {
        self.log_dir.join("serial.log")
    }

    fn fs_snapshot(&self) -> Arc<Mutex<dyn PciSnapshot>> {
        self.fs_snapshot.lock().clone().expect("share built")
    }
}

impl DeviceFactory for TestFactory {
    fn serial(
        &self,
        log_path: &Path,
        irq: Arc<dyn IrqLine>,
    ) -> Result<BuiltSerial, DeviceUnavailable> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .map_err(|e| {
                DeviceUnavailable::new(
                    "serial-16550a",
                    format!("cannot open serial log {}: {e}", log_path.display()),
                )
            })?;
        let dev = Arc::new(Mutex::new(Serial::new(
            "serial0",
            SerialOut::new(Box::new(file)),
            irq,
        )));
        Ok(BuiltSerial {
            device: dev.clone(),
            snapshot: Some(dev),
        })
    }

    fn virtio_blk(
        &self,
        _disk: &vmlab_hv_proto::machine::Disk,
        _mem: &GuestMemory,
        _msi: Arc<dyn MsiSink>,
    ) -> Result<vmlab_hv_vmm::factory::BuiltBlk, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-blk",
            "no block device in this test",
        ))
    }

    fn virtio_console(
        &self,
        _ports: &[vmlab_hv_proto::machine::SerialPort],
        _mem: &GuestMemory,
        _msi: Arc<dyn MsiSink>,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-console",
            "no console device in this test",
        ))
    }

    fn virtio_net(
        &self,
        _nic: &vmlab_hv_proto::machine::Nic,
        _mem: &GuestMemory,
        _msi: Arc<dyn MsiSink>,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-net",
            "no net device in this test",
        ))
    }

    fn vhost_user_fs(
        &self,
        share: &FsShare,
        mem: &GuestMemory,
        msi: Arc<dyn MsiSink>,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        let id = format!("fs-{}", share.tag);
        let fs = Fs::new(
            id.clone(),
            FsConfig {
                tag: share.tag.clone(),
                socket: share.socket.clone(),
            },
        )
        .map_err(|e| {
            DeviceUnavailable::new("vhost-user-fs", format!("share `{}`: {e}", share.tag))
        })?;
        let pci = Arc::new(Mutex::new(VirtioPciDevice::new(
            id,
            Box::new(fs),
            mem.clone(),
            Some(msi),
            None,
        )));
        *self.fs_snapshot.lock() = Some(pci.clone() as Arc<Mutex<dyn PciSnapshot>>);
        Ok(BuiltPci {
            function: pci,
            snapshot: None,
        })
    }
}


/// The stock initramfs's cinit dies without `vmlab.root=` before it ever
/// loads virtiofs, so append a cpio overlay whose `/init` does the share
/// mount by hand: modprobe virtiofs, mount the share, read the shared file
/// back to the serial log, then idle so the machine survives to be
/// snapshotted. The kernel unpacks concatenated archives, later files
/// winning, so the overlay's `/init` replaces cinit's.
fn initrd_with_init_script(dir: &Dir) -> PathBuf {
    let overlay = dir.path.join("overlay");
    let init_dir = overlay.join("etc"); // keep /etc/vmlab-modules from the base
    std::fs::create_dir_all(&overlay).unwrap();
    let _ = init_dir;
    // The initramfs ships busybox without applet symlinks, so every tool is
    // called through the busybox binary itself.
    let script = format!(
        "#!/bin/sh\necho VMLAB-FS-BOOT\n/bin/busybox modprobe virtiofs\n\
         /bin/busybox mkdir -p /mnt\n/bin/busybox mount -t virtiofs {TAG} /mnt\n\
         /bin/busybox cat /mnt/hello.txt\necho VMLAB-FS-OK\n/bin/busybox sleep 3600\n"
    );
    let init = overlay.join("init");
    fs::write(&init, script).unwrap();
    make_executable(&init);
    let archive = dir.path.join("overlay.cpio");
    let output = Command::new("cpio")
        .current_dir(&overlay)
        .args(["-o", "-H", "newc", "--owner", "0:0", "-F"])
        .arg(&archive)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn cpio");
    // `find .` feeds the file list.
    {
        let mut child = output;
        let mut stdin = child.stdin.take().expect("cpio stdin");
        stdin
            .write_all(b".\n./init\n")
            .expect("feed cpio the file list");
    }
    let combined = dir.path.join("initrd.img");
    let mut out = File::create(&combined).unwrap();
    let mut base = File::open(INITRD).unwrap();
    std::io::copy(&mut base, &mut out).unwrap();
    let mut extra = File::open(&archive).unwrap();
    std::io::copy(&mut extra, &mut out).unwrap();
    std::io::Write::flush(&mut out).unwrap();
    combined
}

/// chmod 0o755 on a path (cpio -o cannot take the mode from the fs when we
/// list files by hand).
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

/// One machine description with a single share on `socket`.
fn machine(dir: &Dir, socket: &Path, initrd: &Path) -> MachineDescription {
    MachineDescription {
        name: "vhost-fs-test".into(),
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
        disks: vec![],
        nics: vec![],
        serial_log: dir.path.join("serial.log"),
        ports: vec![],
        shares: vec![FsShare {
            tag: TAG.into(),
            socket: socket.to_path_buf(),
        }],
        rtc_base: None,
    }
}

/// Poll the serial log until it contains `needle`, then return it.
fn wait_for_serial(log: &Path, needle: &str) -> String {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        let text = fs::read_to_string(log).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "serial log never contained {needle:?}; log so far:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Stop a machine at the end of a test and hand back nothing.
fn teardown(vm: Vm) {
    vm.handle().quit();
    vm.wait();
}

#[test]
fn guest_mounts_the_share_and_the_session_is_real() {
    let Some(()) = or_skip() else {
        return;
    };
    let dir = Dir::new("probe");
    let shared = dir.path.join("share");
    fs::create_dir(&shared).unwrap();
    fs::write(shared.join("hello.txt"), b"from virtiofsd\n").unwrap();
    let virtiofsd = Virtiofsd::start(&dir, "a", &shared);
    let initrd = initrd_with_init_script(&dir);

    let factory = TestFactory::new(&dir.path);
    let vm = builder::build(
        &machine(&dir, &virtiofsd.socket, &initrd),
        &factory,
        false,
    )
    .expect("build the machine");

    // The guest's own virtio-fs driver binds the device (DRIVER_OK runs the
    // front-end's full vhost-user setup), mounts the share and reads the
    // file virtiofsd serves: the whole path both ways.
    let log = wait_for_serial(&factory.serial_log(), "VMLAB-FS-OK");
    assert!(
        log.contains("from virtiofsd"),
        "the guest must read the shared file; log:\n{log}"
    );

    // Pause and save: virtiofsd answers GET_VRING_BASE, serialises its state
    // through the device-state pipe and passes CHECK_DEVICE_STATE — proof the
    // backend holds the mem table and both live vrings.
    vm.handle().pause().expect("pause");
    let fs = factory.fs_snapshot();
    let section = fs
        .lock()
        .save_wrapped_state()
        .expect("save device state");
    // magic(4) + version(1) + active(1) + features(8) + count(2) + 2*base(8)
    // + len(4) leaves the state bytes themselves, which must be non-empty.
    assert!(
        section.len() > 28,
        "the device-state section must carry virtiofsd's state, got {} bytes",
        section.len()
    );
    vm.handle().resume().expect("resume");
    teardown(vm);
    // The daemon exits with its front-end: the session was live end to end.
    virtiofsd.expect_exit("machine teardown");
}

#[test]
fn device_state_round_trips_through_virtiofsd() {
    let Some(()) = or_skip() else {
        return;
    };
    let dir = Dir::new("roundtrip");
    let shared = dir.path.join("share");
    fs::create_dir(&shared).unwrap();
    fs::write(shared.join("hello.txt"), b"from virtiofsd\n").unwrap();
    let initrd = initrd_with_init_script(&dir);

    // Machine A: boot, mount the share, save both sections.
    let virtiofsd_a = Virtiofsd::start(&dir, "a", &shared);
    let factory_a = TestFactory::new(&dir.path);
    let vm_a = builder::build(
        &machine(&dir, &virtiofsd_a.socket, &initrd),
        &factory_a,
        false,
    )
    .expect("build machine A");
    wait_for_serial(&factory_a.serial_log(), "VMLAB-FS-OK");
    vm_a.handle().pause().expect("pause A");
    let fs_a = factory_a.fs_snapshot();
    let (transport_state, device_state) = {
        let mut fs = fs_a.lock();
        let transport = fs.save_function_state().expect("save transport state");
        let device = fs.save_wrapped_state().expect("save device state");
        (transport, device)
    };
    assert!(
        device_state.len() > 28,
        "the device-state section must carry virtiofsd's state, got {} bytes",
        device_state.len()
    );
    teardown(vm_a);
    virtiofsd_a.expect_exit("machine A teardown");

    // Machine B: a fresh virtiofsd on the same shared dir, a paused machine,
    // and the sections restored in the snapshot order — transport first (it
    // re-activates the device), then the device section, which replays the
    // ring bases and pushes the state blob through SET_DEVICE_STATE_FD into
    // the new daemon.
    let virtiofsd_b = Virtiofsd::start(&dir, "b", &shared);
    let dir_b = Dir::new("roundtrip-b");
    let initrd_b = initrd_with_init_script(&dir_b);
    let factory_b = TestFactory::new(&dir_b.path);
    let mut vm_b = builder::build(
        &machine(&dir_b, &virtiofsd_b.socket, &initrd_b),
        &factory_b,
        true,
    )
    .expect("build machine B");
    let fs_b = factory_b.fs_snapshot();
    fs_b
        .lock()
        .restore_function_state(&transport_state)
        .expect("restore transport");
    fs_b
        .lock()
        .restore_wrapped_state(&device_state)
        .expect("restore device state");

    // Resume: the guest boots fresh, and mounts and reads the share through
    // the restored backend session.
    vm_b.start();
    let log_b = wait_for_serial(&factory_b.serial_log(), "VMLAB-FS-OK");
    assert!(
        log_b.contains("from virtiofsd"),
        "the restored session must serve the share; log:\n{log_b}"
    );
    teardown(vm_b);
    virtiofsd_b.expect_exit("machine B teardown");
}
