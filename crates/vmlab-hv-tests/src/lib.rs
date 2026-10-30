//! The S1 integration harness (spec, "Testing decisions"): spawn the built
//! `vmlab-hv` binary with a generated machine description, connect the
//! control client, and speak the guest protocols — the cinit ctl channel
//! (`vmlab.ctl.0`) and the vmlab-agent port (`vmlab.agent.0`) — against
//! vmlab's own wire crates.
//!
//! Guest assets: vmlab's pinned container kernel and initramfs from its
//! checkout, and a squashfs rootfs resolved by [`probe_assets`]. Tests skip
//! with a printed reason when `/dev/kvm` or a rootfs is unavailable.

pub mod agent;
pub mod ctl;
pub mod switch;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::Command as CtrlCommand;
use vmlab_hv_proto::machine::{
    Accel, Arch, Boot, Disk, DiskFormat, MachineDescription, Nic, NicAttachment, SerialPort,
};

/// The container kernel and initramfs vmlab's guest ships.
const VMLAB_GUEST_DIST: &str = "/home/wil/dev/vmlab/guest/dist/x86_64";

/// Kernel cmdline exactly as the spec's phase 1 section gives it (what vmlab
/// passes QEMU today): serial console, quiet, panic=reboot, and the two
/// virtio-blk devices cinit expects.
pub const CMDLINE: &str = "console=ttyS0 quiet loglevel=3 panic=-1 reboot=t \
                           vmlab.root=/dev/vda vmlab.scratch=/dev/vdb";

/// Scratch (overlay upper layer) virtual size: vmlab's own container size.
const SCRATCH_SIZE: u64 = 2 << 30;

/// Guest memory. Generous on purpose: these tests measure boot, not footprint.
const MEMORY_MIB: u64 = 512;

/// The MAC a one-NIC machine uses (QEMU's locally-administered range).
pub const TEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

pub fn mac_string(mac: [u8; 6]) -> String {
    mac.map(|b| format!("{b:02x}")).join(":")
}

// ---- assets -----------------------------------------------------------------

/// The guest assets a test needs, plus why they may be missing.
#[derive(Debug)]
pub struct Assets {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub rootfs: Option<PathBuf>,
    pub kvm: bool,
}

impl Assets {
    /// `None` when the tests can run; otherwise the reason to skip with.
    pub fn skip_reason(&self) -> Option<String> {
        if !self.kvm {
            return Some("/dev/kvm is not present".to_string());
        }
        if !self.kernel.is_file() {
            return Some(format!(
                "container kernel {} is not present (vmlab checkout?)",
                self.kernel.display()
            ));
        }
        if self.rootfs.is_none() {
            return Some(
                "no rootfs: set VMLAB_HV_TEST_ROOTFS, run `just s1-rootfs-build`, or pull an \
                 image with vmlab"
                    .to_string(),
            );
        }
        None
    }
}

/// Resolve every asset the S1 tests need.
///
/// Rootfs order (the work package's contract): the `VMLAB_HV_TEST_ROOTFS`
/// env var; then the reproducible Alpine image the just recipe caches under
/// the workspace target dir; then any `rootfs.sqfs` vmlab has pulled under
/// `~/.local/share/vmlab/oci/images/`.
pub fn probe_assets() -> Assets {
    let dist = Path::new(VMLAB_GUEST_DIST);
    let rootfs = rootfs_from_env()
        .or_else(rootfs_from_target)
        .or_else(rootfs_from_vmlab_store);
    Assets {
        kernel: dist.join("vmlinuz"),
        initrd: dist.join("initramfs.img"),
        rootfs,
        kvm: Path::new("/dev/kvm").exists(),
    }
}

fn rootfs_from_env() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var("VMLAB_HV_TEST_ROOTFS").ok()?);
    if path.is_file() {
        Some(path)
    } else {
        panic!("VMLAB_HV_TEST_ROOTFS={path:?} does not exist");
    }
}

fn rootfs_from_target() -> Option<PathBuf> {
    let path = target_dir().join("s1").join("rootfs.sqfs");
    path.is_file().then_some(path)
}

fn rootfs_from_vmlab_store() -> Option<PathBuf> {
    let images = home_dir()?.join(".local/share/vmlab/oci/images/sha256");
    let mut found: Vec<PathBuf> = std::fs::read_dir(images)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("rootfs.sqfs").is_file())
        .map(|e| e.path().join("rootfs.sqfs"))
        .collect();
    found.sort();
    found.into_iter().next()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The workspace target dir (honours `CARGO_TARGET_DIR`).
pub fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Use an explicit binary override, or ask Cargo for a current debug build
/// once per test process. An existing executable may predate a merge.
pub fn hv_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("VMLAB_HV_BIN") {
        let path = PathBuf::from(path);
        assert!(path.is_file(), "VMLAB_HV_BIN={path:?} does not exist");
        return path;
    }
    static BINARY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BINARY
        .get_or_init(|| {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let status = Command::new(cargo)
                .args(["build", "-p", "vmlab-hv"])
                .current_dir(workspace_root())
                .status()
                .expect("spawn cargo to build vmlab-hv");
            assert!(status.success(), "cargo build -p vmlab-hv failed");
            let debug = target_dir().join("debug/vmlab-hv");
            assert!(debug.is_file(), "cargo did not produce {}", debug.display());
            debug
        })
        .clone()
}

// ---- the machine ------------------------------------------------------------

/// What kind of container a test boots.
#[derive(Debug, Clone)]
pub enum Mode {
    /// Exec-driven idle mode: cinit prepares rootfs and networking, emits
    /// `idle`, and never starts a workload process.
    Idle,
    /// Workload mode: `cmd` runs as the container process; cinit powers the
    /// machine off once it exits.
    Workload { cmd: Vec<String>, expect_code: i32 },
}

/// A machine to launch.
#[derive(Debug, Clone)]
pub struct MachineSpec {
    pub name: String,
    pub mode: Mode,
    /// NIC count (each attached to a switch socket the test hosts).
    pub nics: u32,
    pub memory_mib: u64,
    pub vcpus: u32,
    pub paused: bool,
}

impl MachineSpec {
    pub fn idle(name: &str) -> Self {
        MachineSpec {
            name: name.to_string(),
            mode: Mode::Idle,
            nics: 0,
            memory_mib: MEMORY_MIB,
            vcpus: 1,
            paused: false,
        }
    }

    pub fn workload(name: &str, cmd: Vec<String>, expect_code: i32) -> Self {
        MachineSpec {
            mode: Mode::Workload { cmd, expect_code },
            ..MachineSpec::idle(name)
        }
    }

    pub fn with_nics(mut self, nics: u32) -> Self {
        self.nics = nics;
        self
    }

    /// The spec cinit receives, matching the mode.
    pub fn container_spec(&self) -> vmlab_cinit_proto::ContainerSpec {
        let (mode, cmd) = match &self.mode {
            Mode::Idle => (vmlab_cinit_proto::RuntimeMode::Idle, Vec::new()),
            Mode::Workload { cmd, .. } => (vmlab_cinit_proto::RuntimeMode::Workload, cmd.clone()),
        };
        vmlab_cinit_proto::ContainerSpec {
            hostname: self.name.clone(),
            mode,
            entrypoint: Vec::new(),
            cmd,
            env: Vec::new(),
            workdir: None,
            user: None,
            stop_signal: None,
            stop_grace_secs: 1,
            volumes: Vec::new(),
            smb: None,
            nics: self.nics,
            healthcheck: None,
        }
    }
}

/// A launched machine: the process, the control client and the paths its
/// sockets live at. Dropping it stops the process and removes the directory.
pub struct Guest {
    dir: Option<PathBuf>,
    pub child: Child,
    pub control: Client,
    pub ctl_sock: PathBuf,
    pub agent_sock: PathBuf,
    pub switch_sock: Option<PathBuf>,
    pub switch: Option<switch::MiniSwitch>,
    pub hv_log: PathBuf,
    pub serial_log: PathBuf,
    /// When the process was spawned — the boot-clock zero.
    pub launched: Instant,
}

impl std::fmt::Debug for Guest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guest")
            .field("dir", &self.dir)
            .field("hv_log", &self.hv_log)
            .field("serial_log", &self.serial_log)
            .finish()
    }
}

impl Guest {
    pub fn dir(&self) -> &Path {
        self.dir.as_deref().expect("dir is only taken on drop")
    }

    /// The serial log so far, for failure messages.
    pub fn serial_text(&self) -> String {
        std::fs::read_to_string(&self.serial_log).unwrap_or_default()
    }

    /// The hypervisor's own stderr, for failure messages.
    pub fn hv_log_text(&self) -> String {
        std::fs::read_to_string(&self.hv_log).unwrap_or_default()
    }

    /// Everything the tests need when a guest did not behave: both logs.
    pub fn diagnostics(&self, what: &str) -> String {
        format!(
            "--- {what} ---\n== serial ==\n{}\n== vmlab-hv ==\n{}",
            self.serial_text(),
            self.hv_log_text()
        )
    }

    /// Wait for the process to exit; kill it at the deadline.
    pub fn wait_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait().expect("try_wait vmlab-hv") {
                Some(status) => return status,
                None if Instant::now() > deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!(
                        "vmlab-hv did not exit within {timeout:?}: {}",
                        self.diagnostics("timeout")
                    );
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    /// Stop the machine with `quit` and require exit 0.
    pub fn quit_and_exit_zero(&mut self) {
        let outcome = self.control.call(CtrlCommand::Quit).expect("quit command");
        assert!(
            matches!(outcome, vmlab_hv_proto::control::Outcome::Ok { .. }),
            "quit must be accepted: {outcome:?}"
        );
        let status = self.wait_exit(Duration::from_secs(30));
        assert!(status.success(), "quit must exit 0: {status}");
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(dir) = self.dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Launch one machine against a rootfs. Binds the switch socket first (the
/// NIC connects while the machine is built), writes the scratch qcow2 with
/// `vmlab-disk`, spawns the hypervisor, and connects the control client.
pub fn launch(spec: &MachineSpec, assets: &Assets) -> Guest {
    let launched = Instant::now();
    let dir = temp_dir(&spec.name);
    std::fs::create_dir_all(&dir).expect("create test dir");

    let scratch = dir.join("scratch.qcow2");
    vmlab_disk::ops::create_qcow2(&scratch, SCRATCH_SIZE, None, None)
        .expect("create scratch qcow2");
    let rootfs = assets.rootfs.clone().expect("caller checked rootfs");

    let (switch_sock, mini) = if spec.nics > 0 {
        let sock = dir.join("switch.sock");
        let mini = switch::MiniSwitch::bind(&sock).expect("bind test switch");
        (Some(sock), Some(mini))
    } else {
        (None, None)
    };

    let desc = MachineDescription {
        name: spec.name.clone(),
        arch: Arch::X86_64,
        accel: Accel::Kvm,
        vcpus: spec.vcpus,
        memory_mib: spec.memory_mib,
        boot: Boot::Kernel {
            kernel: assets.kernel.clone(),
            initrd: Some(assets.initrd.clone()),
            cmdline: CMDLINE.to_string(),
        },
        disks: vec![
            Disk {
                id: "rootfs".to_string(),
                path: rootfs,
                format: DiskFormat::Raw,
                read_only: true,
                vmstate: false,
            },
            Disk {
                id: "scratch".to_string(),
                path: scratch,
                format: DiskFormat::Qcow2,
                read_only: false,
                vmstate: true,
            },
        ],
        nics: if spec.nics > 0 {
            vec![Nic {
                mac: mac_string(TEST_MAC),
                mtu: None,
                attachment: NicAttachment::Stream {
                    socket: switch_sock.clone().expect("bound above"),
                },
            }]
        } else {
            Vec::new()
        },
        serial_log: dir.join("serial.log"),
        ports: vec![
            SerialPort {
                name: "vmlab.ctl.0".to_string(),
                socket: dir.join("ctl.sock"),
            },
            SerialPort {
                name: "vmlab.agent.0".to_string(),
                socket: dir.join("agent.sock"),
            },
        ],
        shares: Vec::new(),
        audio: None,
        rtc_base: None,
    };
    let machine_path = dir.join("machine.json");
    std::fs::write(
        &machine_path,
        serde_json::to_vec_pretty(&desc).expect("serialize machine"),
    )
    .expect("write machine.json");

    let hv_log_path = dir.join("hv.log");
    let hv_log = std::fs::File::create(&hv_log_path).expect("create hv log");
    let control_path = dir.join("control.sock");
    let mut child = Command::new(hv_binary())
        .arg("--machine")
        .arg(&machine_path)
        .arg("--control")
        .arg(&control_path)
        .args(spec.paused.then_some("--paused"))
        .stdout(Stdio::from(hv_log.try_clone().expect("clone hv log")))
        .stderr(Stdio::from(hv_log))
        .spawn()
        .expect("spawn vmlab-hv");

    let deadline = Instant::now() + Duration::from_secs(30);
    let control = loop {
        match Client::connect(&control_path) {
            Ok(client) => break client,
            Err(e) if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "vmlab-hv never served {}: {e}\n== vmlab-hv ==\n{}",
                    control_path.display(),
                    std::fs::read_to_string(&hv_log_path).unwrap_or_default()
                );
            }
            Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    };

    Guest {
        dir: Some(dir),
        child,
        control,
        ctl_sock: machine_path.parent().unwrap().join("ctl.sock"),
        agent_sock: machine_path.parent().unwrap().join("agent.sock"),
        switch_sock,
        switch: mini,
        hv_log: hv_log_path,
        serial_log: desc.serial_log,
        launched,
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!("vmlab-hv-s1-{name}-{}-{nanos}", std::process::id()))
}

pub(crate) fn connect_with_retry(
    path: &Path,
    timeout: Duration,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(e) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
                let _ = e;
            }
            Err(e) => return Err(e),
        }
    }
}
