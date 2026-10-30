//! SMBIOS 2.8 structures (DMTF DSP0134 2.8.0) for the captured q35 identity.

use crate::acpi::Platform;

#[derive(Clone, Debug)]
pub struct SmbiosConfig {
    pub bios_vendor: String,
    pub bios_version: String,
    pub bios_date: String,
    /// SMBIOS byte order, including the little-endian UUID fields.
    pub uuid: [u8; 16],
    /// CPUID leaf 1 EAX and EDX in little-endian order.
    pub processor_id: [u8; 8],
}

impl Default for SmbiosConfig {
    fn default() -> Self {
        Self {
            bios_vendor: "SeaBIOS".into(),
            bios_version: "Arch Linux 1.17.0-2-2".into(),
            bios_date: "04/01/2014".into(),
            uuid: [0; 16],
            processor_id: [0x40, 0x0f, 0xb4, 0, 0xff, 0xfb, 0x8b, 0x17],
        }
    }
}

#[derive(Clone, Debug)]
pub struct SmbiosTables {
    /// A valid entry point with a zero table address; firmware relocates it.
    pub anchor: Vec<u8>,
    pub tables: Vec<u8>,
}

fn word(data: &mut [u8], offset: usize, value: u16) {
    data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn dword(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn qword(data: &mut [u8], offset: usize, value: u64) {
    data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn structure(kind: u8, length: u8, handle: u16) -> Vec<u8> {
    let mut result = vec![0; length.into()];
    result[0] = kind;
    result[1] = length;
    word(&mut result, 2, handle);
    result
}

fn finish(mut data: Vec<u8>, strings: &[&str]) -> Result<Vec<u8>, String> {
    for string in strings {
        if string.is_empty() || !string.is_ascii() || string.contains('\0') {
            return Err("SMBIOS strings must be nonempty NUL-free ASCII".into());
        }
        data.extend_from_slice(string.as_bytes());
        data.push(0);
    }
    data.push(0);
    if strings.is_empty() {
        data.push(0);
    }
    Ok(data)
}

/// Produce the complete table, including type 0, for installation by firmware.
pub fn generate(platform: &Platform, config: &SmbiosConfig) -> Result<SmbiosTables, String> {
    generate_records(platform, config, true)
}

/// fw_cfg carries platform records; SeaBIOS and OVMF supply their type 0.
pub(crate) fn generate_fw_cfg(
    platform: &Platform,
    config: &SmbiosConfig,
) -> Result<SmbiosTables, String> {
    let mut generated = generate_records(platform, config, false)?;
    generated.anchor[4] = 0;
    generated.anchor[21] = 0;
    Ok(generated)
}

fn generate_records(
    platform: &Platform,
    config: &SmbiosConfig,
    include_bios: bool,
) -> Result<SmbiosTables, String> {
    if platform.vcpus == 0 || platform.vcpus > 254 {
        return Err("SMBIOS 2.8 topology requires 1..=254 vCPUs per socket".into());
    }
    let ram = platform
        .ram_regions
        .iter()
        .try_fold(0u64, |sum, &(base, length)| {
            if length == 0 || base.checked_add(length).is_none() {
                return Err("invalid RAM range".to_string());
            }
            sum.checked_add(length)
                .ok_or_else(|| "RAM size overflow".to_string())
        })?;
    if ram == 0 || ram % (1024 * 1024) != 0 || ram / (1024 * 1024) > 0x7fff_ffff {
        return Err("SMBIOS RAM must be a positive whole MiB fitting the extended size".into());
    }
    let mut records = Vec::new();
    let mut bios = structure(0, 24, 0);
    bios[4..6].copy_from_slice(&[1, 2]);
    word(&mut bios, 6, 0xe800);
    bios[8] = 3;
    qword(&mut bios, 10, 1 << 3);
    bios[19] = 4;
    bios[22..24].fill(0xff);
    if include_bios {
        records.push(finish(
            bios,
            &[&config.bios_vendor, &config.bios_version, &config.bios_date],
        )?);
    }
    let mut system = structure(1, 27, 0x0100);
    system[4..7].copy_from_slice(&[1, 2, 3]);
    system[8..24].copy_from_slice(&config.uuid);
    system[24] = 6;
    records.push(finish(
        system,
        &["QEMU", "Standard PC (Q35 + ICH9, 2009)", "pc-q35-11.1"],
    )?);
    let mut chassis = structure(3, 22, 0x0300);
    chassis[4..7].copy_from_slice(&[1, 1, 2]);
    chassis[9..13].copy_from_slice(&[3, 3, 3, 2]);
    records.push(finish(chassis, &["QEMU", "pc-q35-11.1"])?);
    let mut cpu = structure(4, 42, 0x0400);
    cpu[4..8].copy_from_slice(&[1, 3, 0xfe, 2]);
    cpu[8..16].copy_from_slice(&config.processor_id);
    cpu[16] = 3;
    word(&mut cpu, 20, 2000);
    word(&mut cpu, 22, 2000);
    cpu[24..26].copy_from_slice(&[0x41, 1]);
    cpu[26..32].fill(0xff);
    cpu[35..38].fill(platform.vcpus as u8);
    word(&mut cpu, 38, 2);
    word(&mut cpu, 40, 1);
    records.push(finish(cpu, &["CPU 0", "QEMU", "pc-q35-11.1"])?);
    let mut array = structure(16, 23, 0x1000);
    array[4..7].copy_from_slice(&[1, 3, 6]);
    if ram / 1024 < 0x8000_0000 {
        dword(&mut array, 7, (ram / 1024) as u32);
    } else {
        dword(&mut array, 7, 0x8000_0000);
        qword(&mut array, 15, ram);
    }
    word(&mut array, 11, 0xfffe);
    word(&mut array, 13, 1);
    records.push(finish(array, &[])?);
    let mut dimm = structure(17, 40, 0x1100);
    word(&mut dimm, 4, 0x1000);
    word(&mut dimm, 6, 0xfffe);
    word(&mut dimm, 8, 0xffff);
    word(&mut dimm, 10, 0xffff);
    let size_mb = (ram / (1024 * 1024)) as u32;
    if size_mb < 0x7fff {
        word(&mut dimm, 12, size_mb as u16);
    } else {
        word(&mut dimm, 12, 0x7fff);
        dword(&mut dimm, 28, size_mb);
    }
    dimm[14] = 9;
    dimm[16] = 1;
    dimm[18] = 7;
    word(&mut dimm, 19, 2);
    dimm[23] = 2;
    records.push(finish(dimm, &["DIMM 0", "QEMU"])?);
    for (index, &(base, length)) in platform.ram_regions.iter().enumerate() {
        let handle = 0x1300u16
            .checked_add(u16::try_from(index).map_err(|_| "too many RAM ranges")?)
            .filter(|handle| *handle < 0x2000)
            .ok_or("too many RAM ranges")?;
        let mut range = structure(19, 31, handle);
        let end = base + length - 1;
        if base % 1024 == 0 && length % 1024 == 0 && end / 1024 < u32::MAX.into() {
            dword(&mut range, 4, (base / 1024) as u32);
            dword(&mut range, 8, (end / 1024) as u32);
        } else {
            dword(&mut range, 4, u32::MAX);
            dword(&mut range, 8, u32::MAX);
            qword(&mut range, 15, base);
            qword(&mut range, 23, end);
        }
        word(&mut range, 12, 0x1000);
        range[14] = 1;
        records.push(finish(range, &[])?);
    }
    records.push(finish(structure(32, 11, 0x2000), &[])?);
    records.push(finish(structure(127, 4, 0x7f00), &[])?);
    let mut anchor = vec![0; 31];
    anchor[..4].copy_from_slice(b"_SM_");
    anchor[5..8].copy_from_slice(&[31, 2, 8]);
    word(
        &mut anchor,
        8,
        u16::try_from(records.iter().map(Vec::len).max().unwrap_or(0))
            .map_err(|_| "SMBIOS structure too large")?,
    );
    anchor[16..21].copy_from_slice(b"_DMI_");
    word(
        &mut anchor,
        28,
        u16::try_from(records.len()).map_err(|_| "too many SMBIOS structures")?,
    );
    anchor[30] = 0x28;
    let tables: Vec<u8> = records.into_iter().flatten().collect();
    word(
        &mut anchor,
        22,
        u16::try_from(tables.len()).map_err(|_| "SMBIOS 2.8 table exceeds 64 KiB")?,
    );
    anchor[21] = 0u8.wrapping_sub(
        anchor[16..]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
    );
    anchor[4] = 0u8.wrapping_sub(anchor.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
    Ok(SmbiosTables { anchor, tables })
}
