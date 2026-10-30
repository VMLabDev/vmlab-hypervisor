use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use vmlab_hv_firmware::acpi::{AcpiTables, PciDevice, Platform, generate};
use vmlab_hv_firmware::loader::{TableLoader, Zone};

struct Installed {
    data: Vec<u8>,
    address: u64,
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn name(data: &[u8]) -> String {
    String::from_utf8(data[..data.iter().position(|b| *b == 0).unwrap()].to_vec()).unwrap()
}

// Firmware-side interpreter deliberately independent of the record encoder.
fn install(script: &[u8], files: &mut BTreeMap<String, Vec<u8>>) -> BTreeMap<String, Installed> {
    assert_eq!(script.len() % 128, 0);
    let mut installed: BTreeMap<String, Installed> = BTreeMap::new();
    let mut high = 0x7ffe_0000u64;
    let mut fseg = 0xf0000u64;
    for r in script.as_chunks::<128>().0 {
        match u32_at(r, 0) {
            0 => {}
            1 => {
                let name = name(&r[4..60]);
                let alignment = u32_at(r, 60) as u64;
                assert!(alignment.is_power_of_two());
                let cursor = match r[64] {
                    1 => &mut high,
                    2 => &mut fseg,
                    _ => panic!("allocation zone"),
                };
                *cursor = cursor.div_ceil(alignment) * alignment;
                let data = files[&name].clone();
                assert!(
                    installed
                        .insert(
                            name,
                            Installed {
                                address: *cursor,
                                data: data.clone()
                            }
                        )
                        .is_none()
                );
                *cursor += data.len() as u64;
            }
            2 => {
                let dest = name(&r[4..60]);
                let source = name(&r[60..116]);
                let address = installed[&source].address;
                let offset = u32_at(r, 116) as usize;
                let size = r[120] as usize;
                assert!(matches!(size, 1 | 2 | 4 | 8));
                let data = &mut installed.get_mut(&dest).unwrap().data[offset..offset + size];
                let mut value = [0; 8];
                value[..size].copy_from_slice(data);
                let value = u64::from_le_bytes(value).wrapping_add(address);
                data.copy_from_slice(&value.to_le_bytes()[..size]);
            }
            3 => {
                let file = name(&r[4..60]);
                let offset = u32_at(r, 60) as usize;
                let start = u32_at(r, 64) as usize;
                let length = u32_at(r, 68) as usize;
                let data = &mut installed.get_mut(&file).unwrap().data;
                let sum = sum(&data[start..start + length]);
                data[offset] = data[offset].wrapping_sub(sum);
            }
            4 => {
                let dest = name(&r[4..60]);
                let source = name(&r[60..116]);
                let offset = u32_at(r, 116) as usize;
                let source_offset = u32_at(r, 120) as usize;
                let size = r[124] as usize;
                assert!(matches!(size, 1 | 2 | 4 | 8));
                assert!(source_offset < installed[&source].data.len());
                let pointer = installed[&source].address + source_offset as u64;
                assert!(size == 8 || pointer < 1 << (size * 8));
                files.get_mut(&dest).unwrap()[offset..offset + size]
                    .copy_from_slice(&pointer.to_le_bytes()[..size]);
            }
            other => panic!("unexpected generated command {other}"),
        }
    }
    installed
}

fn sum(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |a, b| a.wrapping_add(*b))
}

fn files(t: &AcpiTables) -> BTreeMap<String, Vec<u8>> {
    [
        ("etc/acpi/tables".into(), t.tables.clone()),
        ("etc/acpi/rsdp".into(), t.rsdp.clone()),
    ]
    .into()
}

fn read_table(blob: &[u8], offset: usize) -> &[u8] {
    &blob[offset..offset + u32_at(blob, offset + 4) as usize]
}

#[test]
fn modern_four_gib_pci_window_matches_the_disk_boot_capture() {
    let platform = Platform {
        ram_regions: vec![(0, 2 << 30), (1 << 32, 2 << 30)],
        pci64_base: Some(0xe00_0000_0000),
        ..Platform::default()
    };
    let generated = generate(&platform).unwrap();
    let dsdt = read_table(&generated.tables, 64);
    let captured = include_bytes!("../../../docs/hwref/q35-seabios-linux-modern/guest/acpi/DSDT");
    assert_eq!(&dsdt[0x1e62..0x1e82], &captured[0x1e62..0x1e82]);
}

#[test]
fn loader_matches_captured_seabios_order_and_packed_offsets() {
    let mut platform = Platform::default();
    platform
        .pci_devices
        .extend([0, 1, 2, 7].map(|function| PciDevice {
            device: 29,
            function,
            vga: false,
        }));
    let generated = generate(&platform).unwrap();
    let captured = include_bytes!(
        "../../../docs/hwref/q35-seabios-linux-generic/guest/fwcfg/blobs/etc/table-loader"
    );
    assert_eq!(generated.loader, captured);
}

#[test]
fn firmware_installs_relocated_checksummed_tables() {
    for p in [
        Platform::default(),
        Platform {
            vcpus: 4,
            max_cpus: 8,
            ram_regions: vec![(0, 2 << 30), (1 << 32, 6 << 30)],
            ecam_base: 0xe000_0000,
            tpm: true,
            ..Platform::default()
        },
    ] {
        let tables = generate(&p).unwrap();
        let installed = install(&tables.loader, &mut files(&tables));
        let replayed = install(&tables.loader, &mut files(&tables));
        for (name, value) in &installed {
            assert_eq!(
                value.data, replayed[name].data,
                "reproducible firmware replay"
            );
        }
        let rsdp = &installed["etc/acpi/rsdp"].data;
        assert_eq!(&rsdp[..8], b"RSD PTR ");
        assert_eq!(sum(rsdp), 0);
        let base = installed["etc/acpi/tables"].address as usize;
        let blob = &installed["etc/acpi/tables"].data;
        let rsdt = read_table(blob, u32_at(rsdp, 16) as usize - base);
        assert_eq!(&rsdt[..4], b"RSDT");
        assert_eq!(sum(rsdt), 0);
        let mut signatures = Vec::new();
        for entry in rsdt[36..].as_chunks::<4>().0 {
            let t = read_table(blob, u32_at(entry, 0) as usize - base);
            assert_eq!(sum(t), 0, "{} checksum", String::from_utf8_lossy(&t[..4]));
            assert_eq!(&t[10..16], b"BOCHS ");
            signatures.push(t[..4].to_vec());
            if &t[..4] == b"APIC" {
                let mut offset = 44;
                let mut processors = 0;
                let mut enabled = 0;
                while offset < t.len() {
                    if t[offset] == 0 {
                        processors += 1;
                        enabled += u16::from(t[offset + 4]);
                    }
                    offset += usize::from(t[offset + 1]);
                }
                assert_eq!(processors, p.max_cpus);
                assert_eq!(enabled, p.vcpus);
            }
            if &t[..4] == b"MCFG" {
                assert_eq!(
                    u64::from_le_bytes(t[44..52].try_into().unwrap()),
                    p.ecam_base
                );
            }
            if &t[..4] == b"FACP" {
                let facs = u32_at(t, 36) as usize - base;
                assert_eq!(&blob[facs..facs + 4], b"FACS");
                assert_eq!(facs % 64, 0);
                let dsdt = read_table(blob, u32_at(t, 40) as usize - base);
                assert_eq!(&dsdt[..4], b"DSDT");
                assert_eq!(sum(dsdt), 0);
                assert_eq!(
                    u64::from_le_bytes(t[140..148].try_into().unwrap()),
                    u32_at(t, 40) as u64
                );
                assert!(
                    dsdt.windows(4)
                        .any(|b| b == format!("C{:03X}", p.max_cpus - 1).as_bytes())
                );
            }
        }
        assert_eq!(signatures.len(), if p.tpm { 6 } else { 5 });
    }
}

#[test]
fn write_pointer_updates_host_file_without_allocating_destination() {
    let mut loader = TableLoader::default();
    loader.allocate("source", 64, Zone::High);
    loader.write_pointer("writable", "source", 1, 7, 8);
    let mut files = [
        ("source".into(), vec![0; 32]),
        ("writable".into(), vec![0; 16]),
    ]
    .into();
    let installed = install(&loader.finish(), &mut files);
    assert_eq!(
        u64::from_le_bytes(files["writable"][1..9].try_into().unwrap()),
        installed["source"].address + 7
    );
    assert!(!installed.contains_key("writable"));
}

fn captured_platform() -> Platform {
    let mut p = Platform::default();
    for function in [0, 1, 2, 7] {
        p.pci_devices.push(PciDevice {
            device: 29,
            function,
            vga: false,
        });
    }
    p
}

#[test]
fn generated_capture_configuration_matches_observed_raw_tables() {
    let p = captured_platform();
    let tables = generate(&p).unwrap();
    let installed = install(&tables.loader, &mut files(&tables));
    let blob = &installed["etc/acpi/tables"].data;
    let base = installed["etc/acpi/tables"].address as usize;
    let rsdt = read_table(
        blob,
        u32_at(&installed["etc/acpi/rsdp"].data, 16) as usize - base,
    );
    let reference = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/hwref/q35-seabios-linux-generic/guest/acpi");
    for entry in rsdt[36..].as_chunks::<4>().0 {
        let t = read_table(blob, u32_at(entry, 0) as usize - base);
        let signature = std::str::from_utf8(&t[..4]).unwrap();
        if signature == "FACP" {
            let dsdt = read_table(blob, u32_at(t, 40) as usize - base);
            let mut actual = dsdt.to_vec();
            let mut expected = std::fs::read(reference.join("DSDT")).unwrap();
            actual[9] = 0;
            expected[9] = 0;
            assert_eq!(
                actual, expected,
                "DSDT namespace and AML match capture exactly"
            );
            let mut actual = t.to_vec();
            let mut expected = std::fs::read(reference.join(signature)).unwrap();
            for range in [9..10, 36..44, 140..148] {
                actual[range.clone()].fill(0);
                expected[range].fill(0);
            }
            assert_eq!(
                actual, expected,
                "FADT excluding firmware allocation addresses"
            );
        } else {
            assert_eq!(
                t,
                std::fs::read(reference.join(signature)).unwrap(),
                "{signature} matches capture"
            );
        }
    }
}

#[test]
fn iasl_disassembles_generated_dsdt_when_available() {
    let tool = std::env::var_os("IASL").unwrap_or_else(|| "iasl".into());
    if Command::new(&tool).arg("-v").output().is_err() {
        eprintln!("SKIP iasl disassembly: iasl unavailable; raw capture parity checked separately");
        return;
    }
    let temporary = Command::new("mktemp").arg("-d").output().unwrap();
    assert!(temporary.status.success());
    let directory = PathBuf::from(std::str::from_utf8(&temporary.stdout).unwrap().trim());
    for (fixture, platform) in [
        captured_platform(),
        Platform::default(),
        Platform {
            vcpus: 32,
            max_cpus: 64,
            ram_regions: vec![(0, 3 << 30), (1 << 32, 5 << 30)],
            ecam_base: 0xe000_0000,
            tpm: true,
            ..Platform::default()
        },
    ]
    .into_iter()
    .enumerate()
    {
        let generated = generate(&platform).unwrap();
        let installed = install(&generated.loader, &mut files(&generated));
        let blob = &installed["etc/acpi/tables"].data;
        let dsdt = read_table(blob, 64);
        let path = directory.join(format!("DSDT{fixture}.aml"));
        std::fs::write(&path, dsdt).unwrap();
        let result = Command::new(&tool).arg("-d").arg(&path).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let disassembly = directory.join(format!("DSDT{fixture}.dsl"));
        let text = std::fs::read_to_string(&disassembly).unwrap();
        for name in [
            "PCI0", "DRAC", "HPET", "PRES", "CPUS", "C000", "C001", "LNKA", "LNKH", "GSIA", "GSIH",
            "GPE0", "PHPR", "FWCF", "SF8", "KBD", "MOU", "LPT1", "COM1", "RTC",
        ] {
            assert!(
                text.contains(name),
                "missing captured namespace name {name}"
            );
        }
        if platform.max_cpus == 64 {
            for name in ["C03F", "TPM", "_DSM", "TPFN"] {
                assert!(
                    text.contains(name),
                    "missing parameterized namespace name {name}"
                );
            }
        }
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(disassembly).unwrap();
    }
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn default_aml_differs_only_by_the_four_approved_usb_functions() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/hwref/q35-seabios-linux-generic/guest/acpi/DSDT");
    let mut expected = std::fs::read(path).unwrap();
    let removed = 0x204b - 0x2007;
    expected.drain(0x2007..0x204b);
    // Decode and update the enclosing Scope packages' AML PkgLength fields.
    for offset in [0x1f7c, 0x1f84] {
        let following = expected[offset] >> 6;
        assert_eq!(following, 1);
        let length = usize::from(expected[offset] & 15) | (usize::from(expected[offset + 1]) << 4);
        let length = length - removed;
        expected[offset] = 0x40 | (length & 15) as u8;
        expected[offset + 1] = (length >> 4) as u8;
    }
    let length = expected.len() as u32;
    expected[4..8].copy_from_slice(&length.to_le_bytes());
    expected[9] = 0;
    let generated = generate(&Platform::default()).unwrap();
    let installed = install(&generated.loader, &mut files(&generated));
    let mut actual = read_table(&installed["etc/acpi/tables"].data, 64).to_vec();
    actual[9] = 0;
    assert_eq!(actual, expected);
}

#[test]
fn rejects_impossible_topologies() {
    for p in [
        Platform {
            vcpus: 0,
            ..Platform::default()
        },
        Platform {
            max_cpus: 256,
            ..Platform::default()
        },
        Platform {
            ecam_base: 0xb001_0000,
            ..Platform::default()
        },
        Platform {
            ram_regions: vec![(0, u64::MAX)],
            ..Platform::default()
        },
    ] {
        assert!(generate(&p).is_err());
    }
}

#[test]
fn supports_three_gib_low_ram_and_approved_usb_difference() {
    let p = Platform {
        ram_regions: vec![(0, 3 << 30), (1 << 32, 5 << 30)],
        ecam_base: 0xe000_0000,
        ..Platform::default()
    };
    let generated = generate(&p).unwrap();
    let installed = install(&generated.loader, &mut files(&generated));
    let dsdt = read_table(&installed["etc/acpi/tables"].data, 64);
    for name in [b"SE8_", b"SE9_", b"SEA_", b"SEF_"] {
        assert!(!dsdt.windows(4).any(|w| w == name));
    }
    assert!(dsdt.windows(4).any(|w| w == b"S28_"));
    // Resource descriptors carry the chosen low-RAM/ECAM limits.
    assert!(
        dsdt.windows(12)
            .any(|w| w == [0, 0, 0, 0xc0, 0xff, 0xff, 0xff, 0xdf, 0, 0, 0, 0])
    );
}
