use vmlab_hv_proto::machine::MachineDescription;

fn machine(bios: &std::path::Path) -> serde_json::Value {
    serde_json::json!({"name":"pc","machine":"pc","arch":"x86_64","accel":"kvm",
        "vcpus":1,"memory_mib":64,"boot":{"kind":"firmware","firmware":"seabios","code":bios},
        "disks":[],"nics":[],"ports":[],"serial_log":"/serial.log"})
}

fn validation(json: serde_json::Value) -> Result<(), String> {
    let machine: MachineDescription = serde_json::from_value(json).unwrap();
    vmlab_hv_vmm::validate(&machine).map_err(|error| error.to_string())
}

#[test]
fn pc_accepts_four_explicit_ide_slots_and_com1_agent_with_com2_log() {
    let bios = tempfile::NamedTempFile::new().unwrap();
    let mut desc = machine(bios.path());
    desc["cpu_model"] = "pentium3".into();
    desc["rtc_base"] = "localtime".into();
    desc["serial_log_port"] = "com2".into();
    desc["ports"] = serde_json::json!([{"name":"agent","socket":"/agent","transport":"com1"}]);
    desc["disks"] = serde_json::Value::Array(
        (0..4)
            .map(|slot| {
                serde_json::json!({
        "id":format!("disk{slot}"),"path":"/disk","format":"qcow2","read_only":false,
        "bus":"ide","channel":slot/2,"unit":slot%2})
            })
            .collect(),
    );
    validation(desc.clone()).unwrap();
    let duplicate = desc["disks"][0].clone();
    desc["disks"].as_array_mut().unwrap().push(duplicate);
    assert!(validation(desc).unwrap_err().contains("at most four"));
}

#[test]
fn phase_boundaries_refuse_undelivered_devices_and_invalid_machine_combinations() {
    let bios = tempfile::NamedTempFile::new().unwrap();
    let base = machine(bios.path());
    for (field, value, error) in [
        (
            "serial_log_port",
            serde_json::json!("com2"),
            "requires a COM1 agent",
        ),
        ("floppy", serde_json::json!("/floppy.img"), "phase 4c"),
        (
            "tablet",
            serde_json::json!({"bus":0,"device":1,"function":2}),
            "phase 4c",
        ),
        (
            "cpu_model",
            serde_json::json!("arbitrary-host-argument"),
            "cpu_model",
        ),
        (
            "ports",
            serde_json::json!([{"name":"agent","socket":"/agent","transport":"com1"}]),
            "occupied",
        ),
    ] {
        let mut desc = base.clone();
        desc[field] = value;
        assert!(validation(desc).unwrap_err().contains(error), "{field}");
    }
    let mut q35 = base.clone();
    q35["machine"] = "q35".into();
    q35["floppy"] = "/floppy".into();
    assert!(
        validation(q35)
            .unwrap_err()
            .contains("floppy requires machine=pc")
    );
    let mut q35 = base;
    q35["machine"] = "q35".into();
    q35["disks"] = serde_json::json!([{"id":"disk","bus":"ide","channel":0,"unit":0,"format":"raw","path":"/disk","read_only":false}]);
    assert!(validation(q35).unwrap_err().contains("IDE on q35"));
}

#[test]
fn ide_addressing_is_unique_bounded_and_independent_of_pci_and_ahci_ports() {
    let bios = tempfile::NamedTempFile::new().unwrap();
    let mut base = machine(bios.path());
    base["disks"] = serde_json::json!([{"id":"cd","bus":"ide","channel":1,"unit":1,"cdrom":true,"format":"raw","read_only":true}]);
    validation(base.clone()).unwrap();
    for (field, value) in [
        ("channel", serde_json::json!(2)),
        ("unit", serde_json::json!(null)),
        ("port", serde_json::json!(0)),
        ("pci", serde_json::json!({"bus":0,"device":1,"function":1})),
    ] {
        let mut desc = base.clone();
        desc["disks"][0][field] = value;
        assert!(
            validation(desc)
                .unwrap_err()
                .contains("IDE requires channel and unit")
        );
    }
    let mut desc = base;
    desc["disks"][0]["read_only"] = false.into();
    assert!(
        validation(desc)
            .unwrap_err()
            .contains("CD-ROM must be read_only")
    );
}
