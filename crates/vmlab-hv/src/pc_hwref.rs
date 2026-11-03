//! P2/S1 differential of BIOS-installed pc firmware and chipset state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vmlab_hv_devices::BusDevice;
use vmlab_hv_firmware::{FirmwareOptions, pc, smbios};
use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Control, Outcome};
use vmlab_hv_tests::process::ManagedChild;

const MAGIC: &[u8; 8] = b"VMLABPC1";
const PROBE: &str = include_str!("../tests/fixtures/pc-hwref/probe.S");
const CAPTURES: [&str; 4] = [
    "pc-templeos",
    "pc-freedos-1.3",
    "pc-windows-7",
    "pc-windows-98-se",
];

fn le16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn le64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn sum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))
}

fn fail(message: impl Into<String>) -> Result<(), String> {
    Err(message.into())
}

fn same(actual: &[u8], expected: &[u8], label: &str) -> Result<(), String> {
    if actual == expected {
        return Ok(());
    }
    let offset = actual.iter().zip(expected).position(|(a, b)| a != b);
    fail(format!(
        "{label}: first difference {offset:#x?}, actual {} bytes, expected {} bytes",
        actual.len(),
        expected.len()
    ))
}

fn record_check(problems: &mut Vec<String>, result: Result<(), String>) {
    if let Err(message) = result {
        problems.push(message);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FwEntry {
    name: String,
    selector: u16,
    size: usize,
}

fn fw_directory(bytes: &[u8]) -> Result<Vec<FwEntry>, String> {
    if bytes.len() < 4 {
        return Err("truncated fw_cfg directory".into());
    }
    let count = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    if count > 512 || bytes.len() != 4 + count * 64 {
        return Err("fw_cfg directory length does not match its count".into());
    }
    let mut names = BTreeSet::new();
    let mut selectors = BTreeSet::new();
    let mut entries = Vec::new();
    for record in bytes[4..].as_chunks::<64>().0 {
        let size = u32::from_be_bytes(record[..4].try_into().unwrap()) as usize;
        let selector = u16::from_be_bytes(record[4..6].try_into().unwrap());
        let name_end = record[8..]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or("unterminated fw_cfg filename")?;
        let name = std::str::from_utf8(&record[8..8 + name_end])
            .map_err(|_| "non-UTF8 fw_cfg filename")?
            .to_string();
        if name.is_empty()
            || !name.is_ascii()
            || !(0x20..0x4000).contains(&selector)
            || record[6..8] != [0; 2]
            || record[8 + name_end..].iter().any(|byte| *byte != 0)
            || !names.insert(name.clone())
            || !selectors.insert(selector)
        {
            return Err("invalid or duplicate fw_cfg directory record".into());
        }
        entries.push(FwEntry {
            name,
            selector,
            size,
        });
    }
    Ok(entries)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct E820 {
    base: u64,
    length: u64,
    kind: u32,
}

fn e820(bytes: &[u8]) -> Result<Vec<E820>, String> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(20) {
        return Err("empty or misaligned BIOS E820 map".into());
    }
    let mut entries = Vec::new();
    for record in bytes.as_chunks::<20>().0 {
        let entry = E820 {
            base: le64(record, 0),
            length: le64(record, 8),
            kind: le32(record, 16),
        };
        if entry.length == 0
            || entry.base.checked_add(entry.length).is_none()
            || !(1..=5).contains(&entry.kind)
        {
            return Err("invalid BIOS E820 entry".into());
        }
        entries.push(entry);
    }
    entries.sort_unstable();
    if entries
        .windows(2)
        .any(|pair| pair[0].base + pair[0].length > pair[1].base)
    {
        return Err("overlapping BIOS E820 entries".into());
    }
    Ok(entries)
}

#[derive(Default)]
struct Dump {
    records: BTreeMap<u8, Vec<u8>>,
    pci: BTreeMap<u16, Vec<u8>>,
    acpi: BTreeMap<[u8; 4], Vec<u8>>,
    acpi_order: Vec<[u8; 4]>,
    files: BTreeMap<u16, Vec<u8>>,
}

impl Dump {
    fn finish(self) -> Result<Self, String> {
        for (tag, length) in [(3, 20), (5, 31), (9, 12), (10, 128)] {
            if self
                .records
                .get(&tag)
                .is_none_or(|bytes| bytes.len() != length)
            {
                return Err(format!("missing or malformed probe record {tag}"));
            }
        }
        let rsdp = &self.records[&3];
        if &rsdp[..8] != b"RSD PTR " || rsdp[15] != 0 || sum(rsdp) != 0 {
            return Err("invalid installed ACPI 1.0 RSDP".into());
        }
        let anchor = &self.records[&5];
        if &anchor[..4] != b"_SM_"
            || &anchor[16..21] != b"_DMI_"
            || sum(anchor) != 0
            || sum(&anchor[16..]) != 0
        {
            return Err("invalid installed SMBIOS 2.x entry point".into());
        }
        if self
            .records
            .get(&6)
            .is_none_or(|bytes| bytes.len() != le16(anchor, 22) as usize)
        {
            return Err("SMBIOS table length does not match the installed entry point".into());
        }
        e820(self.records.get(&1).ok_or("missing BIOS E820 map")?)?;
        let directory = fw_directory(self.records.get(&7).ok_or("missing fw_cfg directory")?)?;
        if directory.len() != self.files.len()
            || directory.iter().any(|entry| {
                self.files
                    .get(&entry.selector)
                    .is_none_or(|bytes| bytes.len() != entry.size)
            })
        {
            return Err("fw_cfg file payloads do not match the directory".into());
        }
        let expected = [
            *b"RSDT", *b"FACP", *b"DSDT", *b"FACS", *b"APIC", *b"HPET", *b"WAET",
        ];
        if self.acpi_order != expected {
            return Err(format!(
                "unexpected installed ACPI table order {:?}",
                self.acpi_order
            ));
        }
        for (signature, table) in &self.acpi {
            if table.len() < 8
                || le32(table, 4) as usize != table.len()
                || (signature != b"FACS" && (table.len() < 36 || sum(table) != 0))
            {
                return Err(format!("invalid installed ACPI table {:?}", signature));
            }
        }
        for bdf in [0, 8, 9, 11] {
            if !self.pci.contains_key(&bdf) {
                return Err(format!("chipset function {bdf:#x} is missing"));
            }
        }
        Ok(self)
    }
}

fn parse(bytes: &[u8]) -> Result<Option<Dump>, String> {
    let Some(start) = bytes
        .windows(MAGIC.len())
        .position(|window| window == MAGIC)
    else {
        return Ok(None);
    };
    let mut position = start + MAGIC.len();
    let mut dump = Dump::default();
    loop {
        if bytes.len() - position < 5 {
            return Ok(None);
        }
        let tag = bytes[position];
        let length = le32(bytes, position + 1) as usize;
        position += 5;
        if length > 1 << 20 {
            return Err("oversized probe TLV".into());
        }
        if bytes.len() - position < length {
            return Ok(None);
        }
        let value = &bytes[position..position + length];
        position += length;
        match tag {
            0 if length == 0 => return dump.finish().map(Some),
            2 if length == 258 => {
                let bdf = le16(value, 0);
                if bdf >= 256 || dump.pci.insert(bdf, value[2..].to_vec()).is_some() {
                    return Err("invalid or duplicate PCI probe record".into());
                }
            }
            4 if length >= 8 => {
                let signature: [u8; 4] = value[..4].try_into().unwrap();
                if dump.acpi.insert(signature, value.to_vec()).is_some() {
                    return Err("duplicate ACPI probe record".into());
                }
                dump.acpi_order.push(signature);
            }
            8 if length >= 2 => {
                if dump
                    .files
                    .insert(le16(value, 0), value[2..].to_vec())
                    .is_some()
                {
                    return Err("duplicate fw_cfg file probe record".into());
                }
            }
            1 | 3 | 5 | 6 | 7 | 9 | 10 => {
                if dump.records.insert(tag, value.to_vec()).is_some() {
                    return Err(format!("duplicate probe record {tag}"));
                }
            }
            255 => {
                return Err(format!(
                    "guest probe failed: {}",
                    String::from_utf8_lossy(value)
                ));
            }
            _ => return Err(format!("unknown or malformed probe TLV tag {tag}")),
        }
    }
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/hwref")
        .join(name)
}

struct Case {
    name: &'static str,
    full: pc::Platform,
    staged: pc::Platform,
    cdrom: bool,
    processor_id: [u8; 8],
}

impl Case {
    fn dram_boundary(&self) -> u8 {
        let ram: u64 = self.full.ram_regions.iter().map(|(_, size)| size).sum();
        (ram / (8 << 20)).min(u64::from(u8::MAX)) as u8
    }

    fn new(name: &'static str) -> Self {
        let reference = fixture(name);
        let pci: serde_json::Value =
            serde_json::from_slice(&std::fs::read(reference.join("query-pci.json")).unwrap())
                .unwrap();
        let devices = pci[0]["devices"].as_array().unwrap();
        let full_devices: Vec<_> = devices
            .iter()
            .map(|device| {
                let slot = device["slot"].as_u64().unwrap() as u8;
                let function = device["function"].as_u64().unwrap() as u8;
                let display = device["class_info"]["class"].as_u64().unwrap() == 0x300;
                pc::PciDevice {
                    device: slot,
                    function,
                    vga: display && device["id"]["device"].as_u64().unwrap() == 0x1111,
                    hotplug: (slot > 1 && !display) || (slot, function) == (1, 2),
                }
            })
            .collect();
        let (vcpus, memory) = match name {
            "pc-templeos" => (2, vec![(0, 512 << 20)]),
            "pc-freedos-1.3" => (1, vec![(0, 64 << 20)]),
            "pc-windows-7" => (2, vec![(0, 3 << 30), (1 << 32, 1 << 30)]),
            "pc-windows-98-se" => (1, vec![(0, 128 << 20)]),
            _ => unreachable!(),
        };
        let full = pc::Platform {
            vcpus,
            max_cpus: vcpus,
            ram_regions: memory,
            pci_devices: full_devices,
            pci64_base: (name == "pc-windows-7").then_some(0xe00_0000_0000),
            com2: name != "pc-windows-7",
            physical_address_bits: if name == "pc-windows-98-se" { 32 } else { 48 },
        };
        let mut staged = full.clone();
        staged.pci_devices.clear();
        for (device, description) in devices.iter().zip(&full.pci_devices) {
            let class = device["class_info"]["class"].as_u64().unwrap();
            if class == 0x200 || (description.device, description.function) == (1, 2) {
                continue;
            }
            let mut kept = description.clone();
            if class == 0x300 {
                kept.vga = true;
            }
            staged.pci_devices.push(kept);
        }
        let mut processor_id = smbios::SmbiosConfig::default().processor_id;
        if name == "pc-freedos-1.3" {
            processor_id[7] = 7;
        }
        if name == "pc-windows-98-se" {
            processor_id = [0x73, 6, 0, 0, 0xff, 0xfb, 0x83, 3];
        }
        Self {
            name,
            full,
            staged,
            cdrom: name != "pc-templeos",
            processor_id,
        }
    }

    fn options(&self, processor_id: [u8; 8]) -> FirmwareOptions {
        let mut bootorder = Vec::new();
        if self.cdrom {
            bootorder.push("/pci@i0cf8/ide@1,1/drive@0/disk@1".into());
        }
        bootorder.push("/pci@i0cf8/ide@1,1/drive@0/disk@0".into());
        FirmwareOptions {
            bootorder,
            smbios: smbios::SmbiosConfig {
                processor_id,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn description(&self, path: &Path, bios: &Path, vga: &Path) -> serde_json::Value {
        let display = self
            .staged
            .pci_devices
            .iter()
            .find(|device| device.vga)
            .unwrap();
        let mut description = serde_json::json!({
            "name":self.name,"machine":"pc","arch":"x86_64","accel":"kvm",
            "vcpus":self.staged.vcpus,
            "memory_mib":self.staged.ram_regions.iter().map(|(_,size)|size).sum::<u64>() >>20,
            "boot":{"kind":"firmware","firmware":"seabios","code":bios},
            "disks":[{"id":"probe","bus":"ide","channel":0,"unit":0,
                "path":path.join("probe.raw"),"format":"raw","read_only":true}],
            "display":{"device":"std-vga","pci":{"bus":0,"device":display.device,"function":display.function},"rom":vga},
            "nics":[],"serial_log":path.join("probe.log"),"ports":[]
        });
        if self.cdrom {
            description["disks"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "id":"cdrom","bus":"ide","channel":0,"unit":1,"cdrom":true,
                    "format":"raw","read_only":true
                }));
        }
        if self.full.com2 {
            description["ports"] = serde_json::json!([{"name":"hwref-com2","transport":"com2","socket":path.join("unused-com2.sock")}]);
        } else {
            description["console_pci"] = serde_json::json!({"bus":0,"device":2,"function":0});
            description["ports"] = serde_json::json!([{"name":"vmlab.agent.0","socket":path.join("unused-agent.sock")}]);
        }
        if self.name == "pc-windows-98-se" {
            description["cpu_model"] = "pentium3".into();
        }
        description
    }
}

fn normalize_acpi(bytes: &[u8]) -> Vec<u8> {
    let mut result = bytes.to_vec();
    if &bytes[..4] == b"FACS" {
        return result;
    }
    result[9] = 0;
    if &bytes[..4] == b"FACP" {
        result[36..44].fill(0);
    }
    if &bytes[..4] == b"RSDT" {
        result[36..].fill(0);
    }
    result
}

fn generated_tables(generated: &vmlab_hv_firmware::acpi::AcpiTables) -> BTreeMap<[u8; 4], Vec<u8>> {
    let mut tables = BTreeMap::new();
    let mut position = 0;
    while generated.tables[position..position + 4] != [0; 4] {
        let length = le32(&generated.tables, position + 4) as usize;
        assert!(length >= 8 && position + length <= generated.tables.len());
        let table = &generated.tables[position..position + length];
        assert!(
            tables
                .insert(table[..4].try_into().unwrap(), table.to_vec())
                .is_none()
        );
        position += length;
    }
    tables
}

fn capture_anchor(case: &Case) -> Result<(), String> {
    let generated = pc::generate(&case.full)?;
    for (signature, table) in generated_tables(&generated) {
        let filename = std::str::from_utf8(&signature).unwrap();
        let captured = std::fs::read(fixture(case.name).join("acpi").join(filename))
            .map_err(|e| e.to_string())?;
        same(
            &normalize_acpi(&table),
            &normalize_acpi(&captured),
            &format!("{} full captured {filename}", case.name),
        )?;
    }
    let expected = smbios::generate_pc(&case.full, &case.options(case.processor_id).smbios)?;
    let captured =
        std::fs::read(fixture(case.name).join("smbios-tables.bin")).map_err(|e| e.to_string())?;
    same(
        &expected.tables,
        &captured,
        &format!("{} complete captured SMBIOS", case.name),
    )
}

fn expected_e820(case: &Case) -> Result<Vec<E820>, String> {
    // The Linux superset capture records the complete BIOS map. Substitute only
    // template RAM ranges and its captured FACS allocation below low RAM's top.
    let reference =
        std::fs::read_to_string(fixture("pc-seabios-legacy").join("guest/e820-memmap.txt"))
            .map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    for line in reference.lines() {
        let tokens: Vec<_> = line.split_whitespace().collect();
        if tokens.len() < 4 {
            return Err("malformed captured e820 row".into());
        }
        let base = u64::from_str_radix(tokens[1].strip_prefix("0x").ok_or("e820 base")?, 16)
            .map_err(|e| e.to_string())?;
        let end = u64::from_str_radix(tokens[2].strip_prefix("0x").ok_or("e820 end")?, 16)
            .map_err(|e| e.to_string())?;
        let kind = match tokens[3..].join(" ").as_str() {
            "System RAM" => 1,
            "Reserved" => 2,
            _ => return Err("unexpected captured e820 kind".into()),
        };
        if base == 0x100000 || base == 0xffe0000 {
            continue;
        }
        if base >= 0xfd_0000_0000 && case.full.physical_address_bits < 40 {
            continue;
        }
        entries.push(E820 {
            base,
            length: end.checked_sub(base).ok_or("e820 range")? + 1,
            kind,
        });
    }
    let fadt = std::fs::read(fixture(case.name).join("acpi/FACP")).map_err(|e| e.to_string())?;
    let facs = u64::from(le32(&fadt, 36));
    let low_top = case.full.ram_regions[0].1;
    if facs < 0x100000 || facs >= low_top {
        return Err("captured FACS outside low RAM".into());
    }
    entries.push(E820 {
        base: 0x100000,
        length: facs - 0x100000,
        kind: 1,
    });
    entries.push(E820 {
        base: facs,
        length: low_top - facs,
        kind: 2,
    });
    for &(base, length) in &case.full.ram_regions[1..] {
        entries.push(E820 {
            base,
            length,
            kind: 1,
        });
    }
    entries.sort_unstable();
    Ok(entries)
}

fn lspci_configs(text: &str) -> Result<BTreeMap<u16, Vec<u8>>, String> {
    let mut result = BTreeMap::new();
    let mut current = None;
    let mut masks = BTreeMap::<u16, [bool; 256]>::new();
    for line in text.lines() {
        let tokens: Vec<_> = line.split_whitespace().collect();
        let Some(first) = tokens.first() else {
            continue;
        };
        if first.len() == 7 && first.as_bytes()[2] == b':' && first.as_bytes()[5] == b'.' {
            let bus = u8::from_str_radix(&first[..2], 16).map_err(|e| e.to_string())?;
            let device = u8::from_str_radix(&first[3..5], 16).map_err(|e| e.to_string())?;
            let function = u8::from_str_radix(&first[6..], 16).map_err(|e| e.to_string())?;
            if bus != 0 || device >= 32 || function >= 8 {
                return Err("invalid captured BDF".into());
            }
            let bdf = u16::from(device) * 8 + u16::from(function);
            if result.insert(bdf, vec![0; 256]).is_some() {
                return Err("duplicate captured BDF".into());
            }
            masks.insert(bdf, [false; 256]);
            current = Some(bdf);
        } else if first.len() == 3
            && first.ends_with(':')
            && tokens.len() == 17
            && first[..2].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            let bdf = current.ok_or("config row before BDF")?;
            let offset = usize::from_str_radix(&first[..2], 16).map_err(|e| e.to_string())?;
            if offset % 16 != 0 || offset > 240 {
                return Err("invalid captured config offset".into());
            }
            for (index, value) in tokens[1..].iter().enumerate() {
                let value = u8::from_str_radix(value, 16).map_err(|e| e.to_string())?;
                if std::mem::replace(&mut masks.get_mut(&bdf).unwrap()[offset + index], true) {
                    return Err("duplicate captured config byte".into());
                }
                result.get_mut(&bdf).unwrap()[offset + index] = value;
            }
        }
    }
    if result.is_empty()
        || masks
            .values()
            .any(|mask| mask.iter().any(|present| !present))
    {
        return Err("incomplete captured PCI config".into());
    }
    Ok(result)
}

fn pci_exception(
    bdf: u16,
    offset: usize,
    actual: &[u8],
    expected: &[u8],
    com2: bool,
) -> Option<&'static str> {
    match (bdf, offset) {
        (8, 0x82) if actual[offset] == 0 && expected[offset] == 2 => Some(
            "Linux quirk_passive_release sets ISA 82h bit1 after BIOS; the BIOS-only probe retains its measured reset value",
        ),
        (11, 0x5f) if actual[offset] == 0x10 && expected[offset] == 0x90 => Some(
            "parallel I/O is excluded in 4a; the captured LPT resource-decode enable bit is clear",
        ),
        (11, 0x67) if !com2 && actual[offset] == 0x08 && expected[offset] == 0x98 => Some(
            "this template has COM1 only; the legacy Linux baseline also enables COM2 decoding",
        ),
        (0, 0x59..=0x5f)
            if (actual[offset] ^ expected[offset])
                & !(if offset == 0x59 { 0x30 } else { 0x33 })
                == 0 =>
        {
            Some(
                "PAM shadow permissions depend on the executed std-VGA ROM and BIOS allocations; register masks remain equal",
            )
        }
        (8, 0x60..=0x63)
            if actual[offset] & 0x70 == 0
                && (actual[offset] & 0x80 != 0
                    || matches!(actual[offset]&15,3..=7|9..=12|14..=15)) =>
        {
            Some(
                "PIRQ selections/disable bits reflect the deferred NIC/UHCI functions and BIOS-only probe, rather than a running Linux PCI/ACPI stack",
            )
        }
        (9, 4) if (actual[offset] ^ expected[offset]) & !4 == 0 => Some(
            "the captured Linux ata_piix driver enables bus mastering; this probe uses BIOS PIO",
        ),
        (9, 0x20..=0x23)
            if le32(actual, 0x20) & 15 == 1
                && (0x1000..=0xfff0).contains(&(le32(actual, 0x20) & !15)) =>
        {
            Some("IDE BAR4 allocation moves when NIC/UHCI BAR consumers are absent")
        }
        (9, 0x40..=0x44)
            if (actual[offset] ^ expected[offset])
                & !(if matches!(offset, 0x41 | 0x43) {
                    0xf3
                } else {
                    0xff
                })
                == 0 =>
        {
            Some(
                "captured ata_piix driver programs IDE timing registers; the probe leaves SeaBIOS PIO timings",
            )
        }
        _ => None,
    }
}

fn compare(case: &Case, dump: &Dump) -> Result<Vec<String>, String> {
    capture_anchor(case)?;
    let mut problems = Vec::new();
    let mut differences=vec![
        "4c: UHCI 00:01.2 and its AML/hotplug notification are absent".into(),
        "D3: genroms/kvmvapic.bin is absent".into(),
        "the original boot disk has fixture contents/capacity; CMOS disk geometry describes that disk".into(),
    ];
    if case.name != "pc-templeos" {
        differences.push("4b: captured e1000/PCnet function and its AML are absent".into());
    }
    if matches!(case.name, "pc-freedos-1.3" | "pc-windows-98-se") {
        differences.push("4d: captured Cirrus is replaced at the same BDF by std-VGA, including its three ACPI sleep methods".into());
    }
    let expected_bdfs: BTreeSet<_> = case
        .staged
        .pci_devices
        .iter()
        .map(|device| u16::from(device.device) * 8 + u16::from(device.function))
        .collect();
    let actual_bdfs: BTreeSet<_> = dump.pci.keys().copied().collect();
    if actual_bdfs != expected_bdfs {
        return Err(format!(
            "{} PCI inventory differs: {actual_bdfs:?} != {expected_bdfs:?}",
            case.name
        ));
    }
    let reference_pci: serde_json::Value = serde_json::from_slice(
        &std::fs::read(fixture(case.name).join("query-pci.json"))
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    for device in reference_pci[0]["devices"].as_array().unwrap() {
        let bdf = device["slot"].as_u64().unwrap() as u16 * 8
            + device["function"].as_u64().unwrap() as u16;
        let Some(actual) = dump.pci.get(&bdf) else {
            continue;
        };
        let class = device["class_info"]["class"].as_u64().unwrap() as u16;
        let (identity, subsystem, pin) = if class == 0x300 {
            (0x1111_1234u32, 0x1100_1af4u32, 0u8)
        } else {
            (
                (device["id"]["device"].as_u64().unwrap() as u32) << 16
                    | device["id"]["vendor"].as_u64().unwrap() as u32,
                (device["id"]["subsystem"].as_u64().unwrap_or(0) as u32) << 16
                    | device["id"]["subsystem-vendor"].as_u64().unwrap_or(0) as u32,
                device["irq_pin"].as_u64().unwrap() as u8,
            )
        };
        if le32(actual, 0) != identity
            || le32(actual, 0x2c) != subsystem
            || le16(actual, 0xa) != class
            || actual[0x3d] != pin
        {
            return Err(format!(
                "{} {:02x}.{} PCI identity/class/subsystem/pin differs from capture",
                case.name,
                bdf / 8,
                bdf % 8
            ));
        }
    }
    let baseline = lspci_configs(
        &std::fs::read_to_string(fixture("pc-seabios-legacy").join("guest/lspci-nnnvvvxxxx.txt"))
            .map_err(|e| e.to_string())?,
    )?;
    for bdf in [0, 8, 9, 11] {
        let actual = &dump.pci[&bdf];
        let mut expected = baseline[&bdf].clone();
        if bdf == 0 {
            let ram: u64 = case.full.ram_regions.iter().map(|(_, size)| size).sum();
            // DRB7 is a byte-sized boundary in 8-MiB units. The baseline has
            // 256 MiB; each profile must report its own installed RAM.
            expected[0x57] = case.dram_boundary();
            if expected[0x57] != baseline[&bdf][0x57] {
                differences.push(format!(
                    "00.0 DRB7: profile RAM {ram} bytes requires {:#04x} in 8-MiB units, rather than the 256-MiB baseline's {:#04x}",
                    expected[0x57], baseline[&bdf][0x57]
                ));
            }
        }
        let mut reasons = BTreeSet::new();
        for offset in 0..256 {
            if actual[offset] != expected[offset] {
                if let Some(reason) =
                    pci_exception(bdf, offset, actual, &expected, case.staged.com2)
                {
                    reasons.insert(reason);
                } else {
                    problems.push(format!(
                        "{} chipset {:02x}.{} config[{offset:#x}]={:#04x}, captured {:#04x}",
                        case.name,
                        bdf / 8,
                        bdf % 8,
                        actual[offset],
                        expected[offset]
                    ));
                }
            }
        }
        differences.extend(
            reasons
                .into_iter()
                .map(|reason| format!("{:02x}.{}: {reason}", bdf / 8, bdf % 8)),
        );
    }
    let cpuid = &dump.records[&9];
    record_check(
        &mut problems,
        same(
            &cpuid[..8],
            &case.processor_id,
            &format!("{} guest CPUID leaf1 EAX/EDX", case.name),
        ),
    );
    let generated = pc::generate(&case.staged)?;
    let expected_tables = generated_tables(&generated);
    for (signature, expected) in &expected_tables {
        let actual = dump
            .acpi
            .get(signature)
            .ok_or_else(|| format!("missing installed ACPI {signature:?}"))?;
        record_check(
            &mut problems,
            same(
                &normalize_acpi(actual),
                &normalize_acpi(expected),
                &format!(
                    "{} installed {}",
                    case.name,
                    String::from_utf8_lossy(signature)
                ),
            ),
        );
    }
    let mut actual_rsdp = dump.records[&3].clone();
    let mut expected_rsdp = generated.rsdp.clone();
    for rsdp in [&mut actual_rsdp, &mut expected_rsdp] {
        rsdp[8] = 0;
        rsdp[16..20].fill(0);
    }
    record_check(
        &mut problems,
        same(&actual_rsdp, &expected_rsdp, &format!("{} RSDP", case.name)),
    );
    let options = case.options(cpuid[..8].try_into().unwrap());
    let expected_smbios = smbios::generate_pc(&case.staged, &options.smbios)?;
    record_check(
        &mut problems,
        same(
            &dump.records[&6],
            &expected_smbios.tables,
            &format!("{} installed complete SMBIOS", case.name),
        ),
    );
    let mut actual_anchor = dump.records[&5].clone();
    let mut expected_anchor = expected_smbios.anchor;
    for anchor in [&mut actual_anchor, &mut expected_anchor] {
        anchor[4] = 0;
        anchor[21] = 0;
        anchor[24..28].fill(0);
    }
    record_check(
        &mut problems,
        same(
            &actual_anchor,
            &expected_anchor,
            &format!("{} installed SMBIOS entry point", case.name),
        ),
    );
    let mut expected_files: BTreeMap<_, _> = pc::ancillary_files(&case.staged, &options)?
        .into_iter()
        .collect();
    expected_files.insert("etc/acpi/rsdp".into(), generated.rsdp);
    expected_files.insert("etc/acpi/tables".into(), generated.tables);
    expected_files.insert("etc/table-loader".into(), generated.loader);
    let directory = fw_directory(&dump.records[&7])?;
    if directory.len() != expected_files.len() {
        return Err("unexpected fw_cfg file inventory".into());
    }
    let reference: serde_json::Value = serde_json::from_slice(
        &std::fs::read(fixture(case.name).join("fwcfg-files.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    for entry in &directory {
        let expected = expected_files
            .get(&entry.name)
            .ok_or_else(|| format!("unexpected fw_cfg file {}", entry.name))?;
        record_check(
            &mut problems,
            same(
                &dump.files[&entry.selector],
                expected,
                &format!("{} fw_cfg {}", case.name, entry.name),
            ),
        );
        let captured = reference
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"].as_str() == Some(&entry.name))
            .ok_or_else(|| format!("fw_cfg file {} missing from capture", entry.name))?;
        if captured["selector"].as_u64() != Some(u64::from(entry.selector))
            || captured["bytes"].as_u64() != Some(entry.size as u64)
        {
            return Err(format!(
                "{} fw_cfg directory metadata differs for {}",
                case.name, entry.name
            ));
        }
    }
    if reference.as_array().unwrap().len() != directory.len() + 1 {
        return Err("capture fw_cfg has unaccounted files beyond the D3 vapic omission".into());
    }
    let actual_e820 = e820(&dump.records[&1])?;
    let expected_e820 = expected_e820(case)?;
    if actual_e820 != expected_e820 {
        problems.push(format!(
            "{} BIOS E820 differs:\nactual={actual_e820:#x?}\nexpected={expected_e820:#x?}",
            case.name
        ));
    }
    let observed: serde_json::Value = serde_json::from_slice(
        &std::fs::read(fixture(case.name).join("cmos-observed.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    for index in [
        0x15u8, 0x16, 0x17, 0x18, 0x30, 0x31, 0x34, 0x35, 0x5b, 0x5c, 0x5d, 0x5f,
    ] {
        let name = format!("{index:#04x}");
        if let Some(values) = observed[&name]["read_values"].as_array() {
            let actual = format!("{:#04x}", dump.records[&10][index as usize]);
            if !values.iter().any(|value| value.as_str() == Some(&actual)) {
                problems.push(format!(
                    "{} CMOS[{index:#x}]={actual} not in captured {values:?}",
                    case.name
                ));
            }
        }
    }
    differences.push("CMOS time/status and fixture disk geometry are collected but not compared to the non-atomic historical samples; observed RAM/CPU fields are compared".into());
    if problems.is_empty() {
        Ok(differences)
    } else {
        Err(problems.join("\n"))
    }
}

fn build_probe(directory: &Path) -> PathBuf {
    let source = directory.join("probe.S");
    let object = directory.join("probe.o");
    let linker = directory.join("probe.ld");
    let image = directory.join("probe.raw");
    std::fs::write(&source, PROBE).unwrap();
    std::fs::write(&linker,"SECTIONS { .boot 0x7c00 : AT(0) { *(.boot) } .payload 0x10000 : AT(512) { *(.payload) } /DISCARD/ : { *(*) } }\n").unwrap();
    for (program, args) in [
        (
            "as",
            vec![
                "--32".into(),
                "-o".into(),
                object.as_os_str().to_owned(),
                source.as_os_str().to_owned(),
            ],
        ),
        (
            "ld",
            vec![
                "-m".into(),
                "elf_i386".into(),
                "--oformat".into(),
                "binary".into(),
                "-T".into(),
                linker.as_os_str().to_owned(),
                "-o".into(),
                image.as_os_str().to_owned(),
                object.as_os_str().to_owned(),
            ],
        ),
    ] {
        let result = Command::new(program).args(args).output().unwrap();
        assert!(
            result.status.success(),
            "{program}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let bytes = std::fs::read(&image).unwrap();
    assert_eq!(&bytes[510..512], &[0x55, 0xaa]);
    assert!(
        bytes.len() > 512 && bytes.len() <= 512 + 32 * 512,
        "payload must fit its EDD read"
    );
    std::fs::OpenOptions::new()
        .write(true)
        .open(&image)
        .unwrap()
        .set_len(8 << 20)
        .unwrap();
    image
}

fn tools_available() -> bool {
    for program in ["as", "ld"] {
        if !Command::new(program)
            .arg("--version")
            .output()
            .is_ok_and(|result| result.status.success())
        {
            eprintln!("SKIP pc hwref probe: GNU {program} is unavailable");
            return false;
        }
    }
    true
}

fn tlv_record(output: &mut Vec<u8>, tag: u8, bytes: &[u8]) {
    output.push(tag);
    output.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    output.extend_from_slice(bytes);
}

fn parser_fixture(case: &Case) -> Vec<u8> {
    let mut output = MAGIC.to_vec();
    let mut map = Vec::new();
    for entry in expected_e820(case).unwrap() {
        map.extend_from_slice(&entry.base.to_le_bytes());
        map.extend_from_slice(&entry.length.to_le_bytes());
        map.extend_from_slice(&entry.kind.to_le_bytes());
    }
    tlv_record(&mut output, 1, &map);
    let configs = lspci_configs(
        &std::fs::read_to_string(fixture("pc-seabios-legacy").join("guest/lspci-nnnvvvxxxx.txt"))
            .unwrap(),
    )
    .unwrap();
    for bdf in [0u16, 8, 9, 11] {
        let mut bytes = bdf.to_le_bytes().to_vec();
        bytes.extend_from_slice(&configs[&bdf]);
        tlv_record(&mut output, 2, &bytes);
    }
    tlv_record(
        &mut output,
        3,
        &std::fs::read(fixture(case.name).join("acpi/RSDP")).unwrap(),
    );
    for signature in ["RSDT", "FACP", "DSDT", "FACS", "APIC", "HPET", "WAET"] {
        tlv_record(
            &mut output,
            4,
            &std::fs::read(fixture(case.name).join("acpi").join(signature)).unwrap(),
        );
    }
    for (tag, name) in [(5, "smbios-entry.bin"), (6, "smbios-tables.bin")] {
        tlv_record(
            &mut output,
            tag,
            &std::fs::read(fixture(case.name).join(name)).unwrap(),
        );
    }
    let mut firmware = vmlab_hv_firmware::FwCfg::for_pc(
        vmlab_hv_devices::GuestMemory::new(),
        &case.full,
        &case.options(case.processor_id),
    )
    .unwrap();
    firmware.write(0, &0x19u16.to_le_bytes());
    let mut directory = vec![0; 4];
    for byte in &mut directory {
        firmware.read(1, std::slice::from_mut(byte));
    }
    let count = u32::from_be_bytes(directory[..4].try_into().unwrap()) as usize;
    directory.resize(4 + 64 * count, 0);
    for byte in &mut directory[4..] {
        firmware.read(1, std::slice::from_mut(byte));
    }
    tlv_record(&mut output, 7, &directory);
    for entry in fw_directory(&directory).unwrap() {
        let mut bytes = entry.selector.to_le_bytes().to_vec();
        bytes.extend_from_slice(firmware.file(&entry.name).unwrap());
        tlv_record(&mut output, 8, &bytes);
    }
    let mut cpuid = case.processor_id.to_vec();
    cpuid.extend_from_slice(&u32::from(case.full.physical_address_bits).to_le_bytes());
    tlv_record(&mut output, 9, &cpuid);
    tlv_record(&mut output, 10, &[0; 128]);
    tlv_record(&mut output, 0, &[]);
    output
}

struct ProbeVm {
    process: ManagedChild,
    control: Client,
    directory: tempfile::TempDir,
}

impl Drop for ProbeVm {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        if std::thread::panicking() || std::env::var_os("VMLAB_HV_ACCEPTANCE_KEEP").is_some() {
            self.directory.disable_cleanup(true);
            eprintln!("pc hwref artifacts: {}", self.directory.path().display());
        }
    }
}

impl ProbeVm {
    fn boot(case: &Case, bios: &Path, vga: &Path) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("hv-p2-")
            .tempdir_in("/var/tmp")
            .unwrap();
        eprintln!("{} P2 artifacts: {}", case.name, directory.path().display());
        build_probe(directory.path());
        let machine = directory.path().join("machine.json");
        std::fs::write(
            &machine,
            serde_json::to_vec_pretty(&case.description(directory.path(), bios, vga)).unwrap(),
        )
        .unwrap();
        let control_path = directory.path().join("control.sock");
        let stderr = std::fs::File::create(directory.path().join("stderr.log")).unwrap();
        let binary = std::env::var_os("VMLAB_HV_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(vmlab_hv_tests::hv_binary);
        let mut process = ManagedChild::spawn(
            Command::new("systemd-run")
                .args([
                    "--user",
                    "--scope",
                    "-q",
                    "-p",
                    "MemoryMax=24G",
                    "-p",
                    "MemorySwapMax=0",
                    "--",
                ])
                .arg(binary)
                .arg("--machine")
                .arg(machine)
                .arg("--control")
                .arg(&control_path)
                .arg("--paused")
                .env("TMPDIR", directory.path())
                .stdout(Stdio::null())
                .stderr(stderr),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let control = loop {
            if let Ok(control) = Client::connect(&control_path) {
                break control;
            }
            assert!(
                process.try_wait().unwrap().is_none() && Instant::now() < deadline,
                "P2 control unavailable: {}",
                std::fs::read_to_string(directory.path().join("stderr.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        Self {
            process,
            control,
            directory,
        }
    }

    fn collect(&mut self) -> Dump {
        assert!(matches!(
            self.control.call(Control::Resume).unwrap(),
            Outcome::Ok { .. }
        ));
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let bytes = std::fs::read(self.directory.path().join("probe.log")).unwrap_or_default();
            match parse(&bytes) {
                Ok(Some(dump)) => return dump,
                Ok(None) => {}
                Err(message) => panic!("{message}; artifacts {}", self.directory.path().display()),
            }
            assert!(
                self.process.try_wait().unwrap().is_none() && Instant::now() < deadline,
                "P2 probe did not finish after {} serial bytes: {}",
                bytes.len(),
                std::fs::read_to_string(self.directory.path().join("stderr.log"))
                    .unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn quit(&mut self) {
        assert!(matches!(
            self.control.call(Control::Quit).unwrap(),
            Outcome::Ok { .. }
        ));
        assert!(self.process.wait().unwrap().success());
    }
}

#[test]
fn pc_hwref_probe_assembles_without_guest() {
    if !tools_available() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let image = build_probe(directory.path());
    assert_eq!(std::fs::metadata(image).unwrap().len(), 8 << 20);
}

#[test]
fn pc_hwref_reference_anchors_and_parsers_without_guest() {
    for name in CAPTURES {
        let case = Case::new(name);
        assert_eq!(
            case.dram_boundary(),
            match name {
                "pc-templeos" => 0x40,
                "pc-freedos-1.3" => 0x08,
                "pc-windows-7" => 0xff,
                "pc-windows-98-se" => 0x10,
                _ => unreachable!(),
            },
        );
        capture_anchor(&case).unwrap();
        let encoded = parser_fixture(&case);
        let decoded = parse(&encoded)
            .unwrap()
            .expect("complete captured TLV stream");
        assert_eq!(decoded.acpi.len(), 7);
        assert_eq!(decoded.pci.len(), 4);
        assert_eq!(decoded.files.len(), 11);
        for prefix in [0, 8, 12, encoded.len() - 1] {
            assert!(parse(&encoded[..prefix]).unwrap().is_none());
        }
        let expected = expected_e820(&case).unwrap();
        assert_eq!(
            expected
                .iter()
                .filter(|entry| entry.kind == 1)
                .map(|entry| entry.length)
                .sum::<u64>(),
            case.full
                .ram_regions
                .iter()
                .map(|(_, size)| size)
                .sum::<u64>()
                - 0x20000
                - 0x100000
                + 0x9fc00
        );
    }
    let configs = lspci_configs(
        &std::fs::read_to_string(fixture("pc-seabios-legacy").join("guest/lspci-nnnvvvxxxx.txt"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(le32(&configs[&0], 0), 0x1237_8086);
    assert_eq!(le32(&configs[&8], 0), 0x7000_8086);
    assert_eq!(le32(&configs[&9], 0), 0x7010_8086);
    assert_eq!(le32(&configs[&11], 0), 0x7113_8086);
    assert!(lspci_configs("00:00.0\n00: 86 80").is_err());
    assert!(fw_directory(&[0, 0, 0, 1]).is_err());
    assert!(parse(b"noiseVMLABPC1\x00\x00\x00\x00\x00").is_err());
    assert!(parse(b"noiseVMLABPC1\x04\x08\x00").unwrap().is_none());
    assert!(parse(b"VMLABPC1\xff\x03\x00\x00\x00bad").is_err());
}

#[test]
#[cfg_attr(vmlab_ci_no_host_assets, ignore = "requires VMLAB_HV_PC_HWREF_DUMP")]
fn pc_hwref_existing_dump_without_guest() {
    let Some(path) = std::env::var_os("VMLAB_HV_PC_HWREF_DUMP") else {
        eprintln!("SKIP offline P2 replay: set VMLAB_HV_PC_HWREF_DUMP to an existing probe.log");
        return;
    };
    let name = std::env::var("VMLAB_HV_PC_HWREF_CAPTURE").unwrap_or_else(|_| "pc-templeos".into());
    let name = CAPTURES
        .into_iter()
        .find(|candidate| *candidate == name)
        .expect("VMLAB_HV_PC_HWREF_CAPTURE must name a committed pc capture");
    let bytes = std::fs::read(path).unwrap();
    let dump = parse(&bytes)
        .unwrap()
        .expect("complete existing P2 probe dump");
    for reason in compare(&Case::new(name), &dump).unwrap() {
        eprintln!("{name}: {reason}");
    }
}

#[test]
fn pc_hwref_pci_exceptions_are_bounded_without_guest() {
    let mut actual = [0; 256];
    let mut expected = [0; 256];
    expected[0x82] = 2;
    assert!(pci_exception(8, 0x82, &actual, &expected, true).is_some());
    expected[0x82] = 3;
    assert!(pci_exception(8, 0x82, &actual, &expected, true).is_none());
    actual[0x5f] = 0x10;
    expected[0x5f] = 0x90;
    assert!(pci_exception(11, 0x5f, &actual, &expected, true).is_some());
    actual[0x5f] = 0x11;
    assert!(pci_exception(11, 0x5f, &actual, &expected, true).is_none());
    actual[0x67] = 0x08;
    expected[0x67] = 0x98;
    assert!(pci_exception(11, 0x67, &actual, &expected, false).is_some());
    assert!(pci_exception(11, 0x67, &actual, &expected, true).is_none());
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn pc_hwref_differential_all_four_captures() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP P2 pc differential: /dev/kvm is unavailable");
        return;
    }
    if !tools_available() {
        return;
    }
    let bios = std::env::var_os("VMLAB_HV_SEABIOS")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/usr/share/qemu/bios-256k.bin".into());
    let vga = std::env::var_os("VMLAB_HV_STDVGABIOS")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/usr/share/qemu/vgabios-stdvga.bin".into());
    for path in [&bios, &vga] {
        if !path.is_file() {
            eprintln!(
                "SKIP P2 pc differential: firmware {} is unavailable",
                path.display()
            );
            return;
        }
    }
    for name in CAPTURES {
        let case = Case::new(name);
        let mut vm = ProbeVm::boot(&case, &bios, &vga);
        let dump = vm.collect();
        let allowed = compare(&case, &dump).unwrap_or_else(|message| {
            panic!("{message}; artifacts {}", vm.directory.path().display())
        });
        eprintln!("{name} P2 exact owned state passed; bounded differences:");
        for reason in allowed {
            eprintln!("  {reason}");
        }
        vm.quit();
    }
}
