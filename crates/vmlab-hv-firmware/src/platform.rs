//! Numeric fw_cfg items and q35 firmware input files.

use crate::acpi::Platform;
use crate::smbios::{self, SmbiosConfig};

#[derive(Clone, Debug)]
pub struct FirmwareOptions {
    /// Open Firmware device paths, in boot priority order.
    pub bootorder: Vec<String>,
    /// Milliseconds before retrying a failed boot; -1 waits indefinitely.
    pub boot_fail_wait_ms: i32,
    pub boot_menu: bool,
    pub nographic: bool,
    pub smbios: SmbiosConfig,
}

impl Default for FirmwareOptions {
    fn default() -> Self {
        Self {
            bootorder: Vec::new(),
            boot_fail_wait_ms: -1,
            boot_menu: false,
            nographic: false,
            smbios: SmbiosConfig::default(),
        }
    }
}

const RESERVATIONS: [(u64, u64); 2] = [(0xfeff_c000, 0x4000), (0xfd_0000_0000, 0x3_0000_0000)];

fn e820_entry(bytes: &mut Vec<u8>, base: u64, length: u64, kind: u32) {
    bytes.extend_from_slice(&base.to_le_bytes());
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&kind.to_le_bytes());
}

fn ram_size(platform: &Platform) -> Result<u64, String> {
    if platform.vcpus == 0 || platform.max_cpus < platform.vcpus {
        return Err("invalid present/maximum vCPU count".into());
    }
    let mut ranges = platform.ram_regions.clone();
    ranges.sort_unstable();
    let mut previous_end = 0;
    let mut total = 0u64;
    for (base, length) in ranges {
        let end = base.checked_add(length).ok_or("RAM range overflows")?;
        if length == 0 || base < previous_end {
            return Err("empty or overlapping RAM range".into());
        }
        for (reserved_base, reserved_length) in RESERVATIONS {
            if base < reserved_base + reserved_length && end > reserved_base {
                return Err("RAM overlaps q35 reserved address range".into());
            }
        }
        let ecam_end = platform
            .ecam_base
            .checked_add(0x1000_0000)
            .ok_or("ECAM range overflows")?;
        if base < ecam_end && end > platform.ecam_base {
            return Err("RAM overlaps PCI ECAM".into());
        }
        previous_end = end;
        total = total.checked_add(length).ok_or("RAM size overflows")?;
    }
    if total == 0 {
        return Err("platform must contain RAM".into());
    }
    Ok(total)
}

/// Numeric items read by SeaBIOS and OVMF, without signature/feature/directory.
pub fn numeric_items(
    platform: &Platform,
    options: &FirmwareOptions,
) -> Result<Vec<(u16, Vec<u8>)>, String> {
    let ram = ram_size(platform)?;
    // No NUMA nodes: retain a zero-valued CPU map for legacy consumers.
    let numa = vec![0; 8 + usize::from(platform.max_cpus) * 8];
    let mut reservations = (RESERVATIONS.len() as u32).to_le_bytes().to_vec();
    for (base, length) in RESERVATIONS {
        e820_entry(&mut reservations, base, length, 2);
    }
    // The numeric UUID item uses RFC 4122 network order, whereas SMBIOS 2.8
    // stores the first three UUID fields little-endian.
    let mut uuid = options.smbios.uuid;
    uuid[..4].reverse();
    uuid[4..6].reverse();
    uuid[6..8].reverse();
    Ok(vec![
        (0x02, uuid.to_vec()),
        (0x03, ram.to_le_bytes().to_vec()),
        (0x04, u16::from(options.nographic).to_le_bytes().to_vec()),
        (0x05, platform.vcpus.to_le_bytes().to_vec()),
        (0x06, 0u16.to_le_bytes().to_vec()),
        (0x0d, numa),
        (0x0e, u16::from(options.boot_menu).to_le_bytes().to_vec()),
        (0x0f, platform.max_cpus.to_le_bytes().to_vec()),
        (0x8000, 0u16.to_le_bytes().to_vec()),
        (0x8001, 0u16.to_le_bytes().to_vec()),
        (0x8002, vec![1]),
        (0x8003, reservations),
        // No legacy HPET configuration records; ACPI describes the HPET.
        (0x8004, vec![0]),
    ])
}

/// Files consumed before firmware reserves its own tables and low RAM.
pub fn ancillary_files(
    platform: &Platform,
    options: &FirmwareOptions,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    ram_size(platform)?;
    let mut e820 = Vec::new();
    for (base, length) in RESERVATIONS {
        e820_entry(&mut e820, base, length, 2);
    }
    for &(base, length) in &platform.ram_regions {
        e820_entry(&mut e820, base, length, 1);
    }
    let mut bootorder = Vec::new();
    for path in &options.bootorder {
        if !path.starts_with('/') || !path.is_ascii() || path.contains(['\n', '\r', '\0']) {
            return Err("boot paths must be absolute NUL-free ASCII without newlines".into());
        }
        if !bootorder.is_empty() {
            bootorder.push(b'\n');
        }
        bootorder.extend_from_slice(path.as_bytes());
    }
    if !bootorder.is_empty() {
        bootorder.push(0);
    }
    let smbios = smbios::generate_fw_cfg(platform, &options.smbios)?;
    Ok(vec![
        ("etc/e820".into(), e820),
        ("bootorder".into(), bootorder),
        (
            "etc/boot-fail-wait".into(),
            options.boot_fail_wait_ms.to_le_bytes().to_vec(),
        ),
        (
            "etc/system-states".into(),
            vec![0x80, 0, 0, 0x81, 0x82, 0x80],
        ),
        ("etc/smbios/smbios-anchor".into(), smbios.anchor),
        ("etc/smbios/smbios-tables".into(), smbios.tables),
        ("bios-geometry".into(), Vec::new()),
    ])
}
