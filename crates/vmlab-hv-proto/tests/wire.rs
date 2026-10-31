//! The wire contract with vmlab, pinned.
//!
//! Every message round trips through JSON, and the exact wire shape of
//! representative messages is pinned as string literals: vmlab depends on
//! this crate, but the literals also document the protocol for humans and
//! catch accidental renames. If one of these strings must change, the change
//! is a protocol break and needs a version bump.

use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;
use vmlab_hv_proto::control::{
    Capability, ClientMsg, Command, ErrorCode, Event, Outcome, RunState, ServerMsg, ShutdownReason,
};
use vmlab_hv_proto::machine::{
    Accel, Arch, Audio, AudioDevice, Boot, Disk, DiskFormat, FsShare, MachineDescription, Nic,
    NicAttachment, PciAddress, SerialPort,
};

/// Assert that `value` serializes to exactly `json` and parses back.
fn wire<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T, json: &str) {
    let encoded = serde_json::to_string(value).unwrap();
    assert_eq!(encoded, json, "wire shape changed");
    assert_eq!(&serde_json::from_str::<T>(json).unwrap(), value);
}

/// Assert that `value` survives a serialize/deserialize round trip.
fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) {
    let encoded = serde_json::to_string(value).unwrap();
    assert_eq!(&serde_json::from_str::<T>(&encoded).unwrap(), value);
}

#[test]
fn nic_option_rom_wire() {
    let mut nic = Nic {
        rom: Some("/firmware/virtio-net.rom".into()),
        pci: None,
        mac: "52:54:00:12:34:56".into(),
        mtu: None,
        attachment: NicAttachment::Stream {
            socket: "/switch.sock".into(),
        },
    };
    wire(
        &nic,
        r#"{"rom":"/firmware/virtio-net.rom","mac":"52:54:00:12:34:56","mtu":null,"attachment":{"kind":"stream","socket":"/switch.sock"}}"#,
    );
    nic.rom = None;
    wire(
        &nic,
        r#"{"mac":"52:54:00:12:34:56","mtu":null,"attachment":{"kind":"stream","socket":"/switch.sock"}}"#,
    );
}

#[test]
fn fs_share_socket_and_path_are_alternatives() {
    use vmlab_hv_proto::machine::FsBacking;
    let socket = FsShare {
        pci: None,
        tag: "share0".into(),
        socket: Some("/run/vmlab/web-1/virtiofs.sock".into()),
        path: None,
        read_only: false,
    };
    wire(
        &socket,
        r#"{"tag":"share0","socket":"/run/vmlab/web-1/virtiofs.sock"}"#,
    );
    assert_eq!(
        socket.backing(),
        Some(FsBacking::Socket("/run/vmlab/web-1/virtiofs.sock".into()))
    );
    let path = FsShare {
        pci: Some(PciAddress {
            bus: 0,
            device: 5,
            function: 0,
        }),
        tag: "share0".into(),
        socket: None,
        path: Some("/srv/web-1".into()),
        read_only: false,
    };
    wire(
        &path,
        r#"{"pci":{"bus":0,"device":5,"function":0},"tag":"share0","path":"/srv/web-1"}"#,
    );
    assert_eq!(
        path.backing(),
        Some(FsBacking::Directory("/srv/web-1".into()))
    );
    // Both or neither leave the backing unresolved; the builder rejects them.
    let both = FsShare {
        socket: Some("/sock".into()),
        path: Some("/srv".into()),
        ..path.clone()
    };
    assert!(!both.read_only);
    assert_eq!(both.backing(), None);
    let neither = FsShare {
        socket: None,
        path: None,
        ..path
    };
    assert_eq!(neither.backing(), None);
}

fn sample_machine() -> MachineDescription {
    MachineDescription {
        exit_on_reset: false,
        vfio: vec![],
        machine: Default::default(),
        cpu: Default::default(),
        hyperv: vec![],
        display: None,
        tablet: None,
        usb: None,
        tpm: None,
        console_pci: None,
        ahci_pci: None,
        name: "web-1".to_owned(),
        arch: Arch::X86_64,
        accel: Accel::Kvm,
        vcpus: 2,
        memory_mib: 512,
        boot: Boot::Kernel {
            kernel: PathBuf::from("/boot/vmlinuz"),
            initrd: Some(PathBuf::from("/boot/initramfs")),
            cmdline: "console=ttyS0 quiet".to_owned(),
        },
        disks: vec![
            Disk {
                pci: None,
                bus: Default::default(),
                port: None,
                cdrom: false,
                id: "rootfs".to_owned(),
                path: Some(PathBuf::from("/var/lib/vmlab/web-1/rootfs.squashfs")),
                format: DiskFormat::Raw,
                read_only: true,
                vmstate: false,
            },
            Disk {
                pci: None,
                bus: Default::default(),
                port: None,
                cdrom: false,
                id: "scratch".to_owned(),
                path: Some(PathBuf::from("/var/lib/vmlab/web-1/scratch.qcow2")),
                format: DiskFormat::Qcow2,
                read_only: false,
                vmstate: true,
            },
        ],
        nics: vec![Nic {
            rom: None,
            pci: None,
            mac: "52:54:00:12:34:56".to_owned(),
            mtu: None,
            attachment: NicAttachment::Stream {
                socket: PathBuf::from("/run/vmlab/web-1/switch.sock"),
            },
        }],
        serial_log: PathBuf::from("/var/log/vmlab/web-1.serial.log"),
        ports: vec![SerialPort {
            name: "vmlab.agent.0".to_owned(),
            socket: PathBuf::from("/run/vmlab/web-1/agent.sock"),
        }],
        shares: vec![FsShare {
            pci: None,
            tag: "share0".to_owned(),
            socket: Some(PathBuf::from("/run/vmlab/web-1/virtiofs.sock")),
            path: None,
            read_only: false,
        }],
        audio: Some(Audio {
            pci: None,
            device: AudioDevice::Hda,
            microphone: false,
            node_name: "vmlab.lab.web-1".to_owned(),
            node_description: "vmlab lab/web-1".to_owned(),
        }),
        rtc_base: Some("2026-10-30T00:00:00Z".to_owned()),
    }
}

#[test]
fn reset_defaults_to_in_place_and_exit_on_reset_is_explicit() {
    let description = sample_machine();
    let mut value = serde_json::to_value(&description).unwrap();
    assert!(value.get("exit_on_reset").is_none());
    assert!(
        !serde_json::from_value::<MachineDescription>(value.clone())
            .unwrap()
            .exit_on_reset
    );
    value["exit_on_reset"] = true.into();
    assert!(
        serde_json::from_value::<MachineDescription>(value)
            .unwrap()
            .exit_on_reset
    );
}

#[test]
fn vfio_assignment_wire_shape_and_omitted_default() {
    use vmlab_hv_proto::machine::{PciAddress, VfioDevice};
    let mut machine = sample_machine();
    assert!(
        serde_json::to_value(&machine)
            .unwrap()
            .get("vfio")
            .is_none()
    );
    machine.vfio.push(VfioDevice {
        host: "0000:65:00.0".into(),
        pci: PciAddress {
            bus: 0,
            device: 6,
            function: 0,
        },
    });
    let encoded = serde_json::to_value(&machine).unwrap();
    assert_eq!(
        encoded["vfio"],
        serde_json::json!([
            {"host": "0000:65:00.0", "pci": {"bus": 0, "device": 6, "function": 0}}
        ])
    );
    assert_eq!(
        serde_json::from_value::<MachineDescription>(encoded).unwrap(),
        machine
    );
}

#[test]
fn display_socket_paths_are_optional_and_pinned() {
    use vmlab_hv_proto::machine::{Display, DisplayDevice, PciAddress};
    wire(
        &Display {
            device: DisplayDevice::VirtioVga,
            gpu: None,
            render_node: None,
            pci: PciAddress {
                bus: 0,
                device: 4,
                function: 0,
            },
            rom: None,
            socket: Some("/run/vm/display.sock".into()),
            vnc_socket: Some("/run/vm/vnc.sock".into()),
        },
        r#"{"device":"virtio-vga","pci":{"bus":0,"device":4,"function":0},"rom":null,"socket":"/run/vm/display.sock","vnc_socket":"/run/vm/vnc.sock"}"#,
    );
    let old: Display = serde_json::from_str(
        r#"{"device":"std-vga","pci":{"bus":0,"device":4,"function":0},"rom":null}"#,
    )
    .unwrap();
    assert!(old.socket.is_none() && old.vnc_socket.is_none());
}

#[test]
fn accelerated_display_wire_is_explicit_and_backward_compatible() {
    use vmlab_hv_proto::machine::{Display, GpuMode};
    for mode in ["virgl", "venus"] {
        let json = format!(
            r#"{{"device":"virtio-vga","gpu":"{mode}","render_node":"/dev/dri/renderD130","pci":{{"bus":0,"device":4,"function":0}},"rom":null}}"#
        );
        let display: Display = serde_json::from_str(&json).unwrap();
        assert_eq!(
            display.gpu,
            Some(if mode == "virgl" {
                GpuMode::Virgl
            } else {
                GpuMode::Venus
            })
        );
        wire(&display, &json);
    }
}

#[test]
fn pinned_firmware_boot() {
    use vmlab_hv_proto::machine::FirmwareKind;
    wire(
        &Boot::Firmware {
            firmware: FirmwareKind::Seabios,
            code: "/firmware/bios.bin".into(),
            vars: None,
            secure_boot: false,
            bootorder: vec!["/pci@i0cf8/scsi@2/disk@0,0".into()],
        },
        r#"{"kind":"firmware","firmware":"seabios","code":"/firmware/bios.bin","vars":null,"secure_boot":false,"bootorder":["/pci@i0cf8/scsi@2/disk@0,0"]}"#,
    );
    wire(
        &Boot::Firmware {
            firmware: FirmwareKind::Ovmf,
            code: "/firmware/OVMF_CODE.fd".into(),
            vars: Some("/vm/OVMF_VARS.fd".into()),
            secure_boot: true,
            bootorder: vec![],
        },
        r#"{"kind":"firmware","firmware":"ovmf","code":"/firmware/OVMF_CODE.fd","vars":"/vm/OVMF_VARS.fd","secure_boot":true,"bootorder":[]}"#,
    );
}

#[test]
fn q35_devices_and_policy_wire_contract() {
    wire(
        &Disk {
            pci: None,
            bus: DiskBus::Ahci,
            port: Some(2),
            cdrom: true,
            id: "cdrom0".into(),
            path: None,
            format: DiskFormat::Raw,
            read_only: true,
            vmstate: false,
        },
        r#"{"bus":"ahci","port":2,"cdrom":true,"id":"cdrom0","format":"raw","read_only":true,"vmstate":false}"#,
    );
    use vmlab_hv_proto::machine::*;
    wire(
        &PciAddress {
            bus: 0,
            device: 31,
            function: 2,
        },
        r#"{"bus":0,"device":31,"function":2}"#,
    );
    wire(
        &Disk {
            pci: None,
            bus: DiskBus::Ahci,
            port: Some(3),
            cdrom: true,
            id: "installer".into(),
            path: Some("/install.iso".into()),
            format: DiskFormat::Raw,
            read_only: true,
            vmstate: false,
        },
        r#"{"bus":"ahci","port":3,"cdrom":true,"id":"installer","path":"/install.iso","format":"raw","read_only":true,"vmstate":false}"#,
    );
    wire(
        &Display {
            device: DisplayDevice::StdVga,
            gpu: None,
            render_node: None,
            pci: PciAddress {
                bus: 0,
                device: 4,
                function: 0,
            },
            rom: None,
            socket: None,
            vnc_socket: None,
        },
        r#"{"device":"std-vga","pci":{"bus":0,"device":4,"function":0},"rom":null}"#,
    );
    wire(&CpuPolicy::Nested, r#""nested""#);
    wire(&Machine::Q35, r#""q35""#);
    wire(
        &vec![
            HyperV::Relaxed,
            HyperV::Vapic,
            HyperV::Spinlocks,
            HyperV::Vpindex,
            HyperV::Runtime,
            HyperV::Synic,
            HyperV::Stimer,
            HyperV::Reset,
            HyperV::Frequencies,
            HyperV::Reenlightenment,
            HyperV::Tlbflush,
            HyperV::Ipi,
            HyperV::Evmbcs,
        ],
        r#"["relaxed","vapic","spinlocks","vpindex","runtime","synic","stimer","reset","frequencies","reenlightenment","tlbflush","ipi","evmbcs"]"#,
    );
    let mut desc = sample_machine();
    desc.machine = Machine::Q35;
    desc.cpu = CpuPolicy::Nested;
    desc.tablet = Some(PciAddress {
        bus: 0,
        device: 5,
        function: 0,
    });
    desc.tpm = Some(Tpm {
        socket: "/vm/swtpm.sock".into(),
    });
    round_trip(&desc);
}

#[test]
fn pinned_client_hello() {
    wire(
        &ClientMsg::Hello { proto_version: 1 },
        r#"{"type":"hello","proto_version":1}"#,
    );
}

#[test]
fn pinned_client_requests() {
    wire(
        &ClientMsg::Request {
            id: 1,
            command: Command::Status,
        },
        r#"{"type":"request","id":1,"command":{"command":"status"}}"#,
    );
    wire(
        &ClientMsg::Request {
            id: 2,
            command: Command::SnapshotSave {
                name: "snap1".to_owned(),
            },
        },
        r#"{"type":"request","id":2,"command":{"command":"snapshot_save","name":"snap1"}}"#,
    );
}

#[test]
fn pinned_server_hello() {
    wire(
        &ServerMsg::Hello {
            proto_version: 1,
            hv_version: "0.1.0".to_owned(),
            capabilities: vec![Capability::Snapshots],
        },
        r#"{"type":"hello","proto_version":1,"hv_version":"0.1.0","capabilities":["snapshots"]}"#,
    );
}

#[test]
fn pinned_server_replies() {
    wire(
        &ServerMsg::Reply {
            id: 1,
            outcome: Outcome::Ok {
                status: Some(RunState::Running),
            },
        },
        r#"{"type":"reply","id":1,"outcome":"ok","status":"running"}"#,
    );
    // A result with nothing to carry omits the status key entirely.
    wire(
        &ServerMsg::Reply {
            id: 2,
            outcome: Outcome::Ok { status: None },
        },
        r#"{"type":"reply","id":2,"outcome":"ok"}"#,
    );
    wire(
        &ServerMsg::Reply {
            id: 3,
            outcome: Outcome::Error {
                code: ErrorCode::NoHandshake,
                message: "handshake required before requests".to_owned(),
            },
        },
        r#"{"type":"reply","id":3,"outcome":"error","code":"no_handshake","message":"handshake required before requests"}"#,
    );
}

#[test]
fn pinned_server_events() {
    wire(
        &ServerMsg::Event {
            event: Event::Shutdown {
                reason: ShutdownReason::GuestShutdown,
            },
        },
        r#"{"type":"event","event":"shutdown","reason":"guest_shutdown"}"#,
    );
    wire(
        &ServerMsg::Event {
            event: Event::Reset,
        },
        r#"{"type":"event","event":"reset"}"#,
    );
}

#[test]
fn pinned_handshake_rejection() {
    // The only id-less server error: a rejected handshake.
    wire(
        &ServerMsg::Error {
            code: ErrorCode::VersionMismatch,
            message: "client protocol version 2, server speaks 1".to_owned(),
        },
        r#"{"type":"error","code":"version_mismatch","message":"client protocol version 2, server speaks 1"}"#,
    );
}

#[test]
fn every_command_round_trips() {
    wire(
        &ClientMsg::Request {
            id: 7,
            command: Command::DeviceAdd {
                id: "usbhost-1-2".to_owned(),
                driver: "usb-host".to_owned(),
                bus: Some("usbx.0".to_owned()),
                hostbus: 1,
                hostaddr: 5,
                hostport: "2.4".to_owned(),
            },
        },
        r#"{"type":"request","id":7,"command":{"command":"device_add","id":"usbhost-1-2","driver":"usb-host","bus":"usbx.0","hostbus":1,"hostaddr":5,"hostport":"2.4"}}"#,
    );
    wire(
        &ClientMsg::Request {
            id: 8,
            command: Command::DeviceDel {
                id: "usbhost-1-2".to_owned(),
            },
        },
        r#"{"type":"request","id":8,"command":{"command":"device_del","id":"usbhost-1-2"}}"#,
    );
    wire(
        &ServerMsg::Event {
            event: Event::DeviceDeleted {
                id: "usbhost-1-2".to_owned(),
            },
        },
        r#"{"type":"event","event":"device_deleted","id":"usbhost-1-2"}"#,
    );
    round_trip(&Command::Status);
    round_trip(&Command::Resume);
    round_trip(&Command::Pause);
    round_trip(&Command::Powerdown);
    round_trip(&Command::Quit);
    round_trip(&Command::SnapshotSave {
        name: "before-update".to_owned(),
    });
    round_trip(&Command::SnapshotLoad {
        name: "before-update".to_owned(),
    });
    round_trip(&Command::SnapshotDelete {
        name: "before-update".to_owned(),
    });
}

#[test]
fn every_outcome_round_trips() {
    round_trip(&Outcome::Ok { status: None });
    for status in [
        RunState::Paused,
        RunState::Running,
        RunState::Suspended,
        RunState::ShuttingDown,
    ] {
        round_trip(&Outcome::Ok {
            status: Some(status),
        });
    }
    for code in [
        ErrorCode::NoHandshake,
        ErrorCode::VersionMismatch,
        ErrorCode::InvalidState,
        ErrorCode::Unsupported,
        ErrorCode::SnapshotNotFound,
        ErrorCode::SnapshotIncompatible,
        ErrorCode::Internal,
    ] {
        round_trip(&Outcome::Error {
            code,
            message: "message".to_owned(),
        });
        round_trip(&code);
    }
}

#[test]
fn every_event_round_trips() {
    round_trip(&Event::Reset);
    round_trip(&Event::Stopped);
    round_trip(&Event::Resumed);
    round_trip(&Event::Suspend);
    round_trip(&Event::Wakeup);
    for reason in [
        ShutdownReason::GuestShutdown,
        ShutdownReason::GuestReset,
        ShutdownReason::HostRequest,
        ShutdownReason::GuestPanic,
    ] {
        round_trip(&Event::Shutdown { reason });
        round_trip(&reason);
    }
}

#[test]
fn every_capability_round_trips() {
    for capability in [
        Capability::Snapshots,
        Capability::Display,
        Capability::Input,
        Capability::Hotplug,
    ] {
        round_trip(&capability);
    }
}

#[test]
fn every_run_state_round_trips() {
    for state in [
        RunState::Paused,
        RunState::Running,
        RunState::Suspended,
        RunState::ShuttingDown,
    ] {
        round_trip(&state);
    }
}

#[test]
fn every_client_message_round_trips() {
    round_trip(&ClientMsg::Hello { proto_version: 1 });
    round_trip(&ClientMsg::Hello {
        proto_version: u32::MAX,
    });
    round_trip(&ClientMsg::Request {
        id: 0,
        command: Command::Quit,
    });
    round_trip(&ClientMsg::Request {
        id: u64::MAX,
        command: Command::SnapshotLoad {
            name: "空".to_owned(),
        },
    });
}

#[test]
fn every_server_message_round_trips() {
    round_trip(&ServerMsg::Hello {
        proto_version: 1,
        hv_version: "0.1.0-nightly+abc".to_owned(),
        capabilities: vec![
            Capability::Snapshots,
            Capability::Display,
            Capability::Input,
            Capability::Hotplug,
        ],
    });
    round_trip(&ServerMsg::Reply {
        id: u64::MAX,
        outcome: Outcome::Error {
            code: ErrorCode::SnapshotIncompatible,
            message: "major version 2".to_owned(),
        },
    });
    round_trip(&ServerMsg::Event {
        event: Event::Wakeup,
    });
    round_trip(&ServerMsg::Error {
        code: ErrorCode::VersionMismatch,
        message: "no".to_owned(),
    });
}

#[test]
fn machine_description_round_trips() {
    round_trip(&sample_machine());
}

#[test]
fn pinned_machine_description() {
    wire(
        &sample_machine(),
        concat!(
            r#"{"name":"web-1","arch":"x86_64","accel":"kvm","vcpus":2,"memory_mib":512,"#,
            r#""boot":{"kind":"kernel","kernel":"/boot/vmlinuz","initrd":"/boot/initramfs","cmdline":"console=ttyS0 quiet"},"#,
            r#""disks":[{"id":"rootfs","path":"/var/lib/vmlab/web-1/rootfs.squashfs","format":"raw","read_only":true,"vmstate":false},"#,
            r#"{"id":"scratch","path":"/var/lib/vmlab/web-1/scratch.qcow2","format":"qcow2","read_only":false,"vmstate":true}],"#,
            r#""nics":[{"mac":"52:54:00:12:34:56","mtu":null,"attachment":{"kind":"stream","socket":"/run/vmlab/web-1/switch.sock"}}],"#,
            r#""serial_log":"/var/log/vmlab/web-1.serial.log","#,
            r#""ports":[{"name":"vmlab.agent.0","socket":"/run/vmlab/web-1/agent.sock"}],"#,
            r#""shares":[{"tag":"share0","socket":"/run/vmlab/web-1/virtiofs.sock"}],"#,
            r#""audio":{"device":"hda","microphone":false,"node_name":"vmlab.lab.web-1","node_description":"vmlab lab/web-1"},"#,
            r#""rtc_base":"2026-10-30T00:00:00Z"}"#
        ),
    );
}

#[test]
fn machine_description_defaults_apply() {
    // What vmlab may omit: empty optional collections and a null RTC base.
    let json = concat!(
        r#"{"name":"min","arch":"x86_64","accel":"kvm","vcpus":1,"memory_mib":128,"#,
        r#""boot":{"kind":"kernel","kernel":"/k","initrd":null,"cmdline":""},"#,
        r#""disks":[],"nics":[],"serial_log":"/log","#,
        r#""ports":[]}"#
    );
    let machine: MachineDescription = serde_json::from_str(json).unwrap();
    assert!(machine.shares.is_empty());
    assert!(machine.rtc_base.is_none());
    round_trip(&machine);
}

#[test]
fn machine_description_without_audio_omits_the_section() {
    // The audio section is optional: absent means the machine has no audio.
    let json = concat!(
        r#"{"name":"min","arch":"x86_64","accel":"kvm","vcpus":1,"memory_mib":128,"#,
        r#""boot":{"kind":"kernel","kernel":"/k","initrd":null,"cmdline":""},"#,
        r#""disks":[],"nics":[],"serial_log":"/log","#,
        r#""ports":[]}"#
    );
    let machine: MachineDescription = serde_json::from_str(json).unwrap();
    assert_eq!(machine.audio, None);
    assert!(!serde_json::to_string(&machine).unwrap().contains("audio"));
}

#[test]
fn audio_microphone_defaults_to_off() {
    let json = r#"{"device":"virtio_snd","node_name":"vmlab.lab.web-1","node_description":"vmlab lab/web-1"}"#;
    assert_eq!(
        serde_json::from_str::<Audio>(json).unwrap(),
        Audio {
            pci: None,
            device: AudioDevice::VirtioSnd,
            microphone: false,
            node_name: "vmlab.lab.web-1".to_owned(),
            node_description: "vmlab lab/web-1".to_owned(),
        }
    );
}

#[test]
fn audio_section_rejects_unknown_fields() {
    let json =
        r#"{"device":"hda","microphone":true,"node_name":"n","node_description":"d","sink":"x"}"#;
    assert!(serde_json::from_str::<Audio>(json).is_err());
}

#[test]
fn machine_description_rejects_unknown_fields() {
    let json = concat!(
        r#"{"name":"min","arch":"x86_64","accel":"kvm","vcpus":1,"memory_mib":128,"#,
        r#""boot":{"kind":"kernel","kernel":"/k","initrd":null,"cmdline":""},"#,
        r#""disks":[],"nics":[],"serial_log":"/log","ports":[],"qemu_args":[]}"#
    );
    assert!(serde_json::from_str::<MachineDescription>(json).is_err());
}
