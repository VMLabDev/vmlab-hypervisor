use std::collections::BTreeMap;
use std::path::PathBuf;

use vm_memory::GuestAddress;
use vmlab_hv_devices::{BusDevice, GuestMemory};
use vmlab_hv_firmware::acpi::AcpiTables;
use vmlab_hv_firmware::pc::{PciDevice, Platform};
use vmlab_hv_firmware::{FirmwareOptions, FwCfg, pc, smbios};

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn sum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))
}

fn table(bytes: &[u8], offset: usize) -> &[u8] {
    &bytes[offset..offset + u32_at(bytes, offset + 4) as usize]
}

fn filename(bytes: &[u8]) -> &str {
    std::str::from_utf8(&bytes[..bytes.iter().position(|&byte| byte == 0).unwrap()]).unwrap()
}

// Interpret the firmware command stream independently of its encoder.
fn install(generated: AcpiTables) -> BTreeMap<String, (u64, Vec<u8>)> {
    let mut files = BTreeMap::from([
        ("etc/acpi/rsdp".to_string(), (0xf0000, generated.rsdp)),
        ("etc/acpi/tables".to_string(), (0x100000, generated.tables)),
    ]);
    let mut allocations = Vec::new();
    for record in generated.loader.as_chunks::<128>().0 {
        let command = u32_at(record, 0);
        if command == 0 {
            continue;
        }
        let dest = filename(&record[4..60]).to_string();
        match command {
            1 => {
                assert!(!allocations.contains(&dest));
                allocations.push(dest.clone());
                let alignment = u32_at(record, 60) as u64;
                assert!(alignment.is_power_of_two());
                assert_eq!(files[&dest].0 % alignment, 0);
                assert_eq!(record[64], if dest.ends_with("rsdp") { 2 } else { 1 });
            }
            2 => {
                let source = filename(&record[60..116]);
                assert!(allocations.iter().any(|name| name == source));
                let address = files[source].0;
                let offset = u32_at(record, 116) as usize;
                let width = record[120] as usize;
                assert!(matches!(width, 4 | 8));
                let bytes = &mut files.get_mut(&dest).unwrap().1[offset..offset + width];
                let mut value = [0; 8];
                value[..width].copy_from_slice(bytes);
                let value = u64::from_le_bytes(value) + address;
                assert!(width == 8 || value < 1 << 32);
                bytes.copy_from_slice(&value.to_le_bytes()[..width]);
            }
            3 => {
                let offset = u32_at(record, 60) as usize;
                let start = u32_at(record, 64) as usize;
                let len = u32_at(record, 68) as usize;
                let bytes = &mut files.get_mut(&dest).unwrap().1;
                bytes[offset] = bytes[offset].wrapping_sub(sum(&bytes[start..start + len]));
            }
            _ => panic!("unexpected loader command {command}"),
        }
    }
    files
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/hwref")
        .join(name)
}

fn capture_platform(name: &str) -> Platform {
    let reference = fixture(name);
    let pci: serde_json::Value =
        serde_json::from_slice(&std::fs::read(reference.join("query-pci.json")).unwrap()).unwrap();
    let pci_devices = pci[0]["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|device| {
            let slot = device["slot"].as_u64().unwrap() as u8;
            let function = device["function"].as_u64().unwrap() as u8;
            let display = device["class_info"]["class"].as_u64().unwrap() == 0x300;
            let standard_vga = display && device["id"]["device"].as_u64().unwrap() == 0x1111;
            PciDevice {
                device: slot,
                function,
                vga: standard_vga,
                hotplug: (slot > 1 && !display) || (slot, function) == (1, 2),
            }
        })
        .collect();
    let (vcpus, ram_regions) = match name {
        "pc-templeos" => (2, vec![(0, 512 << 20)]),
        "pc-freedos-1.3" => (1, vec![(0, 64 << 20)]),
        "pc-windows-7" => (2, vec![(0, 3 << 30), (1 << 32, 1 << 30)]),
        "pc-windows-98-se" => (1, vec![(0, 128 << 20)]),
        _ => panic!("unknown fixture"),
    };
    Platform {
        vcpus,
        max_cpus: vcpus,
        ram_regions,
        pci_devices,
        pci64_base: if name == "pc-windows-7" {
            Some(0xe00_0000_0000)
        } else {
            None
        },
        com2: name != "pc-windows-7",
        physical_address_bits: if name == "pc-windows-98-se" { 32 } else { 48 },
    }
}

#[test]
fn all_four_captured_pc_acpi_sets_match_after_relocation() {
    for name in [
        "pc-templeos",
        "pc-freedos-1.3",
        "pc-windows-7",
        "pc-windows-98-se",
    ] {
        let platform = capture_platform(name);
        let installed = install(pc::generate(&platform).unwrap());
        let rsdp = &installed["etc/acpi/rsdp"].1;
        assert_eq!(rsdp.len(), 20);
        assert_eq!(rsdp[15], 0);
        assert_eq!(sum(rsdp), 0);
        let mut rsdp_header = rsdp.clone();
        let mut expected_rsdp = std::fs::read(fixture(name).join("acpi/RSDP")).unwrap();
        for bytes in [&mut rsdp_header, &mut expected_rsdp] {
            bytes[8] = 0;
            bytes[16..20].fill(0);
        }
        assert_eq!(rsdp_header, expected_rsdp, "{name} RSDP header");
        let base = installed["etc/acpi/tables"].0 as usize;
        let blob = &installed["etc/acpi/tables"].1;
        let rsdt = table(blob, u32_at(rsdp, 16) as usize - base);
        assert_eq!(sum(rsdt), 0);
        assert_eq!(rsdt.len(), 52);
        let expected_rsdt = std::fs::read(fixture(name).join("acpi/RSDT")).unwrap();
        assert_eq!(&rsdt[..9], &expected_rsdt[..9]);
        assert_eq!(&rsdt[10..36], &expected_rsdt[10..36]);
        let mut signatures = Vec::new();
        for entry in rsdt[36..].as_chunks::<4>().0 {
            let actual = table(blob, u32_at(entry, 0) as usize - base);
            let signature = std::str::from_utf8(&actual[..4]).unwrap();
            signatures.push(signature.to_string());
            assert_eq!(sum(actual), 0, "{name} {signature} checksum");
            let mut expected = std::fs::read(fixture(name).join("acpi").join(signature)).unwrap();
            let mut actual = actual.to_vec();
            if signature == "FACP" {
                assert_eq!(actual[8], 1);
                let dsdt = table(blob, u32_at(&actual, 40) as usize - base);
                assert_eq!(sum(dsdt), 0);
                let mut dsdt = dsdt.to_vec();
                let mut capture = std::fs::read(fixture(name).join("acpi/DSDT")).unwrap();
                dsdt[9] = 0;
                capture[9] = 0;
                assert_bytes(&dsdt, &capture, &format!("{name} DSDT"));
                let facs = u32_at(&actual, 36) as usize - base;
                assert_eq!(facs % 64, 0);
                assert_eq!(
                    &blob[facs..facs + 64],
                    std::fs::read(fixture(name).join("acpi/FACS")).unwrap()
                );
                // Firmware allocation addresses vary with RAM and table placement.
                for range in [9..10, 36..44] {
                    actual[range.clone()].fill(0);
                    expected[range].fill(0);
                }
            }
            assert_bytes(&actual, &expected, &format!("{name} {signature}"));
        }
        assert_eq!(signatures, ["FACP", "APIC", "HPET", "WAET"]);
    }
}

fn assert_bytes(actual: &[u8], expected: &[u8], label: &str) {
    let first = actual.iter().zip(expected).position(|(a, b)| a != b);
    assert!(
        first.is_none() && actual.len() == expected.len(),
        "{label}: first difference {first:#x?}, actual {} bytes, expected {} bytes",
        actual.len(),
        expected.len()
    );
}

#[test]
fn pc_smbios_matches_all_four_complete_captured_tables() {
    for name in [
        "pc-templeos",
        "pc-freedos-1.3",
        "pc-windows-7",
        "pc-windows-98-se",
    ] {
        let platform = capture_platform(name);
        let mut config = smbios::SmbiosConfig::default();
        if name == "pc-freedos-1.3" {
            // That capture masks LM in its otherwise host CPUID policy.
            config.processor_id[7] = 7;
        } else if name == "pc-windows-98-se" {
            config.processor_id = [0x73, 6, 0, 0, 0xff, 0xfb, 0x83, 3];
        }
        let generated = smbios::generate_pc(&platform, &config).unwrap();
        assert_eq!(sum(&generated.anchor), 0);
        assert_eq!(sum(&generated.anchor[16..]), 0);
        let mut anchor = generated.anchor.clone();
        let mut captured_anchor = std::fs::read(fixture(name).join("smbios-entry.bin")).unwrap();
        for bytes in [&mut anchor, &mut captured_anchor] {
            bytes[4] = 0;
            bytes[21] = 0;
            bytes[24..28].fill(0);
        }
        assert_eq!(anchor, captured_anchor, "{name} SMBIOS entry point");
        let captured = std::fs::read(fixture(name).join("smbios-tables.bin")).unwrap();
        assert_bytes(&generated.tables, &captured, name);
    }
}

#[test]
fn fw_cfg_file_names_sizes_and_selectors_match_captures_except_omitted_vapic() {
    for name in [
        "pc-templeos",
        "pc-freedos-1.3",
        "pc-windows-7",
        "pc-windows-98-se",
    ] {
        let platform = capture_platform(name);
        let options = FirmwareOptions {
            bootorder: vec!["/pci@i0cf8/ide@1,1/drive@0/disk@0".into()],
            ..Default::default()
        };
        let memory = GuestMemory::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        let mut firmware = FwCfg::for_pc(memory, &platform, &options).unwrap();
        firmware.write(0, &0x19u16.to_le_bytes());
        let mut directory = vec![0; 4];
        for byte in &mut directory {
            firmware.read(1, std::slice::from_mut(byte));
        }
        let count = u32::from_be_bytes(directory.try_into().unwrap()) as usize;
        let mut directory = vec![0; count * 64];
        for byte in &mut directory {
            firmware.read(1, std::slice::from_mut(byte));
        }
        let actual: BTreeMap<_, _> = directory
            .as_chunks::<64>()
            .0
            .iter()
            .map(|entry| {
                (
                    filename(&entry[8..]).to_string(),
                    (
                        u32::from_be_bytes(entry[..4].try_into().unwrap()),
                        u16::from_be_bytes(entry[4..6].try_into().unwrap()),
                    ),
                )
            })
            .collect();
        let reference: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture(name).join("fwcfg-files.json")).unwrap())
                .unwrap();
        for entry in reference.as_array().unwrap() {
            let key = entry["name"].as_str().unwrap();
            if key == "genroms/kvmvapic.bin" {
                assert!(
                    !actual.contains_key(key),
                    "phase-4 D3 deliberately omits vapic ROM"
                );
                continue;
            }
            let expected_size = if key == "bootorder" {
                // This test attaches one disk, not each capture's disk/CD set.
                34
            } else {
                entry["bytes"].as_u64().unwrap() as u32
            };
            assert_eq!(
                actual[key],
                (expected_size, entry["selector"].as_u64().unwrap() as u16),
                "{name} {key}"
            );
        }
        assert_eq!(actual.len() + 1, reference.as_array().unwrap().len());
    }
}

#[test]
fn pc_ram_reservations_boot_order_and_policy_match_legacy_probe() {
    let platform = Platform {
        vcpus: 1,
        max_cpus: 1,
        ram_regions: vec![(0, 256 << 20)],
        ..Default::default()
    };
    let options = FirmwareOptions::default();
    let files: BTreeMap<_, _> = pc::ancillary_files(&platform, &options)
        .unwrap()
        .into_iter()
        .collect();
    for name in [
        "etc/e820",
        "etc/system-states",
        "etc/boot-fail-wait",
        "bios-geometry",
    ] {
        let captured = std::fs::read(
            fixture("pc-seabios-legacy")
                .join("guest/fwcfg/blobs")
                .join(name),
        )
        .unwrap();
        assert_eq!(files[name], captured, "{name}");
    }
    let numeric: BTreeMap<_, _> = pc::numeric_items(&platform, &options)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(numeric[&3], (256u64 << 20).to_le_bytes());
    assert_eq!(numeric[&5], 1u16.to_le_bytes());
    let options = FirmwareOptions {
        bootorder: vec![
            "/pci@i0cf8/ide@1,1/drive@1/disk@0".into(),
            "/pci@i0cf8/ide@1,1/drive@0/disk@0".into(),
        ],
        ..Default::default()
    };
    let files: BTreeMap<_, _> = pc::ancillary_files(&platform, &options)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        files["bootorder"],
        b"/pci@i0cf8/ide@1,1/drive@1/disk@0\n/pci@i0cf8/ide@1,1/drive@0/disk@0\0"
    );
}

#[test]
fn pc_rejects_bad_topology_and_supports_many_cpus() {
    for platform in [
        Platform {
            vcpus: 0,
            ..Default::default()
        },
        Platform {
            max_cpus: 256,
            ..Default::default()
        },
        Platform {
            ram_regions: vec![],
            ..Default::default()
        },
        Platform {
            ram_regions: vec![(0, u64::MAX)],
            ..Default::default()
        },
        Platform {
            ram_regions: vec![(0, 64 << 20), (32 << 20, 1 << 20)],
            ..Default::default()
        },
        Platform {
            pci64_base: Some(0x1_0000_1000),
            ..Default::default()
        },
        Platform {
            physical_address_bits: 31,
            ..Default::default()
        },
        Platform {
            physical_address_bits: 64,
            ..Default::default()
        },
    ] {
        assert!(pc::generate(&platform).is_err());
    }
    let platform = Platform {
        vcpus: 8,
        max_cpus: 64,
        ..Default::default()
    };
    let installed = install(pc::generate(&platform).unwrap());
    let dsdt = table(&installed["etc/acpi/tables"].1, 64);
    assert!(dsdt.windows(4).any(|name| name == b"C03F"));
    assert_eq!(sum(dsdt), 0);
    assert!(!dsdt.windows(4).any(|name| name == b"MCFG"));
}
