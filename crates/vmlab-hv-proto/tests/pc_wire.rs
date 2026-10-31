use vmlab_hv_proto::machine::{
    ComPort, DiskBus, DisplayDevice, Machine, MachineDescription, NicModel, SerialTransport,
};

fn base() -> serde_json::Value {
    serde_json::json!({
        "name":"legacy", "machine":"pc", "arch":"x86_64", "accel":"kvm",
        "vcpus":1,"memory_mib":64,
        "boot":{"kind":"firmware","firmware":"seabios","code":"/bios"},
        "disks":[],"nics":[],"ports":[],"serial_log":"/serial.log"
    })
}

#[test]
fn pc_ide_and_com1_agent_wire_roundtrip() {
    let mut json = base();
    json["cpu_model"] = "pentium3".into();
    json["rtc_base"] = "localtime".into();
    json["serial_log_port"] = "com2".into();
    json["ports"] =
        serde_json::json!([{"name":"vmlab.agent.0","socket":"/agent.sock","transport":"com1"}]);
    json["disks"] = serde_json::json!([{"id":"disk0","bus":"ide","channel":0,"unit":1,"path":"/disk.qcow2","format":"qcow2","read_only":false,"vmstate":true}]);
    let desc: MachineDescription = serde_json::from_value(json).unwrap();
    assert_eq!(desc.machine, Machine::Pc);
    assert_eq!(desc.disks[0].bus, DiskBus::Ide);
    assert_eq!(
        (desc.disks[0].channel, desc.disks[0].unit),
        (Some(0), Some(1))
    );
    assert_eq!(desc.serial_log_port, ComPort::Com2);
    assert_eq!(desc.ports[0].transport, SerialTransport::Com1);
    assert_eq!(desc.cpu_model.as_deref(), Some("pentium3"));
    assert_eq!(desc.rtc_base.as_deref(), Some("localtime"));
    assert_eq!(
        serde_json::from_slice::<MachineDescription>(&serde_json::to_vec(&desc).unwrap()).unwrap(),
        desc
    );
}

#[test]
fn additive_defaults_preserve_existing_descriptions() {
    let desc: MachineDescription = serde_json::from_value(base()).unwrap();
    assert_eq!(desc.serial_log_port, ComPort::Com1);
    assert_eq!(desc.cpu_model, None);
    assert_eq!(desc.floppy, None);
    let json = serde_json::to_value(desc).unwrap();
    for absent in ["cpu_model", "floppy", "serial_log_port"] {
        assert!(json.get(absent).is_none());
    }
}

#[test]
fn later_phase_device_wire_names_are_explicit() {
    assert_eq!(
        serde_json::to_string(&DisplayDevice::CirrusVga).unwrap(),
        "\"cirrus-vga\""
    );
    assert_eq!(
        serde_json::to_string(&NicModel::E1000).unwrap(),
        "\"e1000\""
    );
    assert_eq!(
        serde_json::to_string(&NicModel::Pcnet).unwrap(),
        "\"pcnet\""
    );
}
