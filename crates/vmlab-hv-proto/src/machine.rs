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
    /// vhost-user-fs shares served by an external virtiofsd.
    #[serde(default)]
    pub shares: Vec<FsShare>,
    /// Guest audio device; absent means the machine has no audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<Audio>,
    /// RTC start, RFC 3339. `None` means host time.
    #[serde(default)]
    pub rtc_base: Option<String>,
}

/// The guest audio device and the host streams it plays into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Audio {
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disk {
    /// Stable id; also the snapshot node name (e.g. `rootfs`, `scratch`, `disk0`).
    pub id: String,
    pub path: PathBuf,
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
    /// Mount tag the guest uses.
    pub tag: String,
    /// virtiofsd's vhost-user socket.
    pub socket: PathBuf,
}
