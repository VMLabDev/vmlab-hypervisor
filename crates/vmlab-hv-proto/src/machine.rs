//! The machine description: everything `vmlab-hv` needs to build one machine.
//!
//! vmlab writes it as JSON and passes its path with `--machine`. It replaces the
//! QEMU argv for this hypervisor; vmlab's resolver produces it, so it carries
//! resolved values only (no defaults are applied here).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One machine, fully resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineDescription {
    /// Exit cleanly with shutdown{guest_reset} instead of resetting in place.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exit_on_reset: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vfio: Vec<VfioDevice>,
    #[serde(default, skip_serializing_if = "Machine::is_microvm")]
    pub machine: Machine,
    #[serde(default, skip_serializing_if = "CpuPolicy::is_host")]
    pub cpu: CpuPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hyperv: Vec<HyperV>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<Display>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tablet: Option<PciAddress>,
    /// The xHCI controller for host USB passthrough (the VM's `usb = true`):
    /// the controller vmlab names `usbx`, whose root ports `device_add`
    /// addresses as `usbx.0`. One controller carries the tablet and the
    /// passthrough ports; when both this and `tablet` are set they name the
    /// same PCI function.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb: Option<PciAddress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm: Option<Tpm>,
    /// PCI address of the shared multiport virtio-serial controller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_pci: Option<PciAddress>,
    /// PCI address of the AHCI controller, whose ports carry AHCI disks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahci_pci: Option<PciAddress>,
    /// Human-readable label used in logs and the process title.
    pub name: String,
    pub arch: Arch,
    pub accel: Accel,
    pub vcpus: u32,
    /// Guest RAM in MiB.
    pub memory_mib: u64,
    pub boot: Boot,
    /// Disks in guest enumeration order (`/dev/vda`, `/dev/vdb`, ...).
    pub disks: Vec<Disk>,
    pub nics: Vec<Nic>,
    /// File the first UART's output is appended to.
    pub serial_log: PathBuf,
    /// Named virtio-serial ports, each served on its own unix socket.
    pub ports: Vec<SerialPort>,
    /// virtio-fs shares: `socket` is served by an external virtiofsd over
    /// vhost-user, `path` by the hypervisor's in-process device.
    #[serde(default)]
    pub shares: Vec<FsShare>,
    /// Guest audio device; absent means the machine has no audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<Audio>,
    /// RTC start, RFC 3339. `None` means host time.
    #[serde(default)]
    pub rtc_base: Option<String>,
}

/// A pre-bound host PCI function assigned at an explicit guest address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VfioDevice {
    /// Canonical domain:bus:device.function, e.g. `0000:65:00.0`.
    pub host: String,
    pub pci: PciAddress,
}

/// The guest audio device and the host streams it plays into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Audio {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pci: Option<PciAddress>,
    /// Guest device model presenting the audio hardware.
    pub device: AudioDevice,
    /// Whether the host microphone is attached to the guest. Default off.
    #[serde(default)]
    pub microphone: bool,
    /// Host-side node name (vmlab sets `vmlab.<lab>.<machine>`), used by
    /// PipeWire and PulseAudio to identify the machine's streams.
    pub node_name: String,
    /// Human-readable node description shown by volume mixers.
    pub node_description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioDevice {
    Hda,
    VirtioSnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    X86_64,
    Aarch64,
    Riscv64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Accel {
    Kvm,
    Tcg,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Boot {
    /// Direct kernel boot (container micro-VMs).
    Kernel {
        kernel: PathBuf,
        initrd: Option<PathBuf>,
        cmdline: String,
    },
    Firmware {
        firmware: FirmwareKind,
        code: PathBuf,
        #[serde(default)]
        vars: Option<PathBuf>,
        #[serde(default)]
        secure_boot: bool,
        #[serde(default)]
        bootorder: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disk {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pci: Option<PciAddress>,
    #[serde(default, skip_serializing_if = "DiskBus::is_virtio")]
    pub bus: DiskBus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u8>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cdrom: bool,
    /// Stable id; also the snapshot node name (e.g. `rootfs`, `scratch`, `disk0`).
    pub id: String,
    /// Absent only for an AHCI CD-ROM drive with no inserted medium.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub format: DiskFormat,
    pub read_only: bool,
    /// The disk whose snapshot carries RAM and device state. Exactly one
    /// writable disk sets this when the machine supports online snapshots.
    #[serde(default)]
    pub vmstate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskFormat {
    Raw,
    Qcow2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Nic {
    /// PCI option ROM image; absent disables firmware network boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rom: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pci: Option<PciAddress>,
    /// `aa:bb:cc:dd:ee:ff`.
    pub mac: String,
    /// Advertised MTU; `None` means 1500.
    #[serde(default)]
    pub mtu: Option<u16>,
    pub attachment: NicAttachment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NicAttachment {
    /// Client connection to vmlab's switch: each frame is a 4-byte big-endian
    /// length followed by one Ethernet frame (vmlab `src/net/framing.rs`).
    Stream { socket: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SerialPort {
    /// Port name the guest sees (e.g. `vmlab.agent.0`, `vmlab.ctl.0`).
    pub name: String,
    /// Unix socket the hypervisor listens on for the host side.
    pub socket: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsShare {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pci: Option<PciAddress>,
    /// Mount tag the guest uses.
    pub tag: String,
    /// virtiofsd's vhost-user socket: the share is served by an external
    /// daemon over vhost-user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<PathBuf>,
    /// Host directory the hypervisor itself serves with the in-process
    /// virtio-fs device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Serve the share read-only: every mutating operation is refused with
    /// EROFS. The in-process device enforces it; an external virtiofsd is
    /// started read-only by its owner.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
}

/// Where a share's contents come from, resolved from an [`FsShare`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsBacking {
    /// An external virtiofsd on this vhost-user socket.
    Socket(PathBuf),
    /// The in-process device rooted at this host directory.
    Directory(PathBuf),
}

impl FsShare {
    /// The share's backing. `None` when the description sets both `socket`
    /// and `path` or neither: exactly one must name the server.
    pub fn backing(&self) -> Option<FsBacking> {
        match (self.socket.as_ref(), self.path.as_ref()) {
            (Some(socket), None) => Some(FsBacking::Socket(socket.clone())),
            (None, Some(path)) => Some(FsBacking::Directory(path.clone())),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Machine {
    #[default]
    Microvm,
    Q35,
}

impl Machine {
    pub fn is_microvm(&self) -> bool {
        *self == Self::Microvm
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuPolicy {
    #[default]
    Host,
    Nested,
}

impl CpuPolicy {
    pub fn is_host(&self) -> bool {
        *self == Self::Host
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirmwareKind {
    Seabios,
    Ovmf,
}

/// Bus-zero BDF. Range and collision checks belong to machine validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PciAddress {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskBus {
    #[default]
    Virtio,
    Ahci,
}

impl DiskBus {
    pub fn is_virtio(&self) -> bool {
        *self == Self::Virtio
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Display {
    pub device: DisplayDevice,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_node: Option<PathBuf>,
    pub pci: PciAddress,
    pub rom: Option<PathBuf>,
    /// Native display socket; absent uses display.sock beside the control socket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<PathBuf>,
    /// RFB socket; absent uses vnc.sock beside the control socket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vnc_socket: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DisplayDevice {
    StdVga,
    VirtioVga,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuMode {
    Virgl,
    Venus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tpm {
    pub socket: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HyperV {
    Relaxed,
    Vapic,
    Spinlocks,
    Vpindex,
    Runtime,
    Time,
    Synic,
    Stimer,
    Reset,
    Frequencies,
    Reenlightenment,
    Tlbflush,
    Ipi,
    Avic,
    Evmbcs,
}

#[cfg(test)]
mod hyperv_wire_tests {
    use super::{CpuPolicy, HyperV};

    #[test]
    fn pinned_hyperv_and_cpu_policy() {
        let features = [
            HyperV::Relaxed,
            HyperV::Vapic,
            HyperV::Spinlocks,
            HyperV::Vpindex,
            HyperV::Runtime,
            HyperV::Time,
            HyperV::Synic,
            HyperV::Stimer,
            HyperV::Frequencies,
            HyperV::Reset,
            HyperV::Tlbflush,
            HyperV::Ipi,
            HyperV::Avic,
        ];
        let json = r#"["relaxed","vapic","spinlocks","vpindex","runtime","time","synic","stimer","frequencies","reset","tlbflush","ipi","avic"]"#;
        assert_eq!(serde_json::to_string(&features).unwrap(), json);
        assert_eq!(serde_json::from_str::<Vec<HyperV>>(json).unwrap(), features);
        assert_eq!(serde_json::to_string(&CpuPolicy::Host).unwrap(), "\"host\"");
        assert_eq!(
            serde_json::to_string(&CpuPolicy::Nested).unwrap(),
            "\"nested\""
        );
    }
}

#[cfg(test)]
mod audio_wire_tests {
    use super::{Audio, AudioDevice, MachineDescription, PciAddress};

    /// The audio section's wire shape, pinned (hv-audio D7): `device`,
    /// `microphone` defaulting off, the node identity, and the optional
    /// q35 PCI address.
    #[test]
    fn pinned_audio_section() {
        let json = r#"{"device":"hda","microphone":true,"node_name":"vmlab.lab.machine",
                "node_description":"vmlab machine"}"#
            .replace('\n', "");
        let audio: Audio = serde_json::from_str(&json).unwrap();
        assert_eq!(audio.device, AudioDevice::Hda);
        assert!(audio.microphone);
        assert_eq!(audio.node_name, "vmlab.lab.machine");
        assert_eq!(audio.node_description, "vmlab machine");
        assert_eq!(audio.pci, None);

        let json = r#"{"device":"virtio_snd","microphone":false,
                "node_name":"n","node_description":"d",
                "pci":{"bus":0,"device":27,"function":0}}"#
            .replace('\n', "");
        let audio: Audio = serde_json::from_str(&json).unwrap();
        assert_eq!(audio.device, AudioDevice::VirtioSnd);
        assert!(!audio.microphone);
        assert_eq!(
            audio.pci,
            Some(PciAddress {
                bus: 0,
                device: 27,
                function: 0
            })
        );

        // `microphone` is optional and defaults off; nothing else is.
        let minimal: Audio =
            serde_json::from_str(r#"{"device":"hda","node_name":"n","node_description":"d"}"#)
                .unwrap();
        assert!(!minimal.microphone);
        assert!(serde_json::from_str::<Audio>(r#"{"device":"hda"}"#).is_err());
        // The wire name of the device variants is snake_case.
        assert_eq!(
            serde_json::to_string(&AudioDevice::VirtioSnd).unwrap(),
            "\"virtio_snd\""
        );
    }

    /// The audio section rides at the top level of the machine description
    /// and is absent by default.
    #[test]
    fn audio_section_of_the_machine_description() {
        let base = serde_json::json!({
            "name": "m", "arch": "x86_64", "accel": "kvm", "vcpus": 1, "memory_mib": 64,
            "boot": {"kind": "kernel", "kernel": "/k", "initrd": null, "cmdline": ""},
            "disks": [], "nics": [], "serial_log": "/log", "ports": [],
        });
        let plain: MachineDescription = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(plain.audio, None);

        let mut with_audio = base;
        with_audio["audio"] = serde_json::json!({
            "device": "hda", "node_name": "n", "node_description": "d",
            "pci": {"bus": 0, "device": 27, "function": 0},
        });
        let desc: MachineDescription = serde_json::from_value(with_audio).unwrap();
        let audio = desc.audio.unwrap();
        assert_eq!(audio.device, AudioDevice::Hda);
        assert_eq!(audio.node_name, "n");
    }
}
