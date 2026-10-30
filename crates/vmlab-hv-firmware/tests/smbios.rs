use std::collections::BTreeMap;

use vmlab_hv_firmware::acpi::Platform;
use vmlab_hv_firmware::platform::{FirmwareOptions, ancillary_files, numeric_items};
use vmlab_hv_firmware::smbios::{SmbiosConfig, generate};

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))
}

fn records(mut data: &[u8]) -> Vec<&[u8]> {
    let mut result = Vec::new();
    while !data.is_empty() {
        let formatted = usize::from(data[1]);
        assert!(formatted >= 4 && formatted <= data.len());
        let end = formatted
            + data[formatted..]
                .windows(2)
                .position(|bytes| bytes == [0, 0])
                .expect("double NUL terminator")
            + 2;
        result.push(&data[..end]);
        data = &data[end..];
    }
    result
}

#[test]
fn smbios_matches_complete_captured_guest_table() {
    let platform = Platform {
        vcpus: 2,
        max_cpus: 2,
        ram_regions: vec![(0, 0x8000_0000)],
        ..Platform::default()
    };
    let generated = generate(&platform, &SmbiosConfig::default()).unwrap();
    let captured =
        include_bytes!("../../../docs/hwref/q35-seabios-linux-generic/guest/dmi-tables/DMI");
    assert_eq!(generated.tables, captured.as_slice());
    let structures = records(&generated.tables);
    assert_eq!(
        structures
            .iter()
            .map(|record| record[0])
            .collect::<Vec<_>>(),
        [0, 1, 3, 4, 16, 17, 19, 32, 127]
    );
    assert_eq!(&generated.anchor[..4], b"_SM_");
    assert_eq!(&generated.anchor[16..21], b"_DMI_");
    assert_eq!(&generated.anchor[6..8], &[2, 8]);
    assert_eq!(checksum(&generated.anchor), 0);
    assert_eq!(checksum(&generated.anchor[16..]), 0);
    assert_eq!(
        u16::from_le_bytes(generated.anchor[22..24].try_into().unwrap()) as usize,
        generated.tables.len()
    );
    assert_eq!(
        u16::from_le_bytes(generated.anchor[28..30].try_into().unwrap()) as usize,
        structures.len()
    );
}

#[test]
fn smbios_handles_high_memory_and_custom_identity() {
    let platform = Platform {
        vcpus: 8,
        max_cpus: 16,
        ram_regions: vec![(0, 0x8000_0000), (0x1_0000_0000, 0x480_0000_0000)],
        ..Platform::default()
    };
    let config = SmbiosConfig {
        uuid: [0x42; 16],
        processor_id: [0xa5; 8],
        ..SmbiosConfig::default()
    };
    let generated = generate(&platform, &config).unwrap();
    let records = records(&generated.tables);
    let system = records.iter().find(|record| record[0] == 1).unwrap();
    assert_eq!(&system[8..24], &[0x42; 16]);
    let cpu = records.iter().find(|record| record[0] == 4).unwrap();
    assert_eq!(&cpu[8..16], &[0xa5; 8]);
    assert_eq!(&cpu[35..38], &[8; 3]);
    let array = records.iter().find(|record| record[0] == 16).unwrap();
    assert_eq!(
        u32::from_le_bytes(array[7..11].try_into().unwrap()),
        0x8000_0000
    );
    assert_eq!(
        u64::from_le_bytes(array[15..23].try_into().unwrap()),
        0x480_8000_0000
    );
    let dimm = records.iter().find(|record| record[0] == 17).unwrap();
    assert_eq!(u16::from_le_bytes(dimm[12..14].try_into().unwrap()), 0x7fff);
    assert_eq!(
        u32::from_le_bytes(dimm[28..32].try_into().unwrap()),
        0x48_0800
    );
    let ranges: Vec<_> = records.iter().filter(|record| record[0] == 19).collect();
    assert_eq!(ranges.len(), 2);
    assert_eq!(
        u32::from_le_bytes(ranges[1][4..8].try_into().unwrap()),
        u32::MAX
    );
    assert_eq!(
        u64::from_le_bytes(ranges[1][15..23].try_into().unwrap()),
        0x1_0000_0000
    );
}

#[test]
fn ancillary_files_match_capture_bytes() {
    let platform = Platform {
        ram_regions: vec![(0, 0x8000_0000)],
        ..Platform::default()
    };
    let files: BTreeMap<_, _> = ancillary_files(&platform, &FirmwareOptions::default())
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        files["etc/e820"],
        include_bytes!("../../../docs/hwref/q35-seabios-linux-generic/guest/fwcfg/blobs/etc/e820")
    );
    assert_eq!(
        files["etc/system-states"],
        include_bytes!(
            "../../../docs/hwref/q35-seabios-linux-generic/guest/fwcfg/blobs/etc/system-states"
        )
    );
    assert_eq!(
        files["etc/boot-fail-wait"],
        include_bytes!(
            "../../../docs/hwref/q35-seabios-linux-generic/guest/fwcfg/blobs/etc/boot-fail-wait"
        )
    );
    assert!(files["bootorder"].is_empty());
    let options = FirmwareOptions {
        bootorder: vec![
            "/pci@i0cf8/scsi@2/disk@0,0".into(),
            "/pci@i0cf8/scsi@1f,2/disk@0,0".into(),
        ],
        ..FirmwareOptions::default()
    };
    let files: BTreeMap<_, _> = ancillary_files(&platform, &options)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        files["bootorder"],
        b"/pci@i0cf8/scsi@2/disk@0,0\n/pci@i0cf8/scsi@1f,2/disk@0,0\n\0"
    );
}

#[test]
fn numeric_items_have_firmware_widths_and_endianness() {
    let platform = Platform {
        vcpus: 4,
        max_cpus: 8,
        ram_regions: vec![(0, 0x8000_0000), (0x1_0000_0000, 0x1_8000_0000)],
        ..Platform::default()
    };
    let options = FirmwareOptions {
        boot_menu: true,
        nographic: true,
        smbios: SmbiosConfig {
            uuid: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
            ..SmbiosConfig::default()
        },
        ..FirmwareOptions::default()
    };
    let items: BTreeMap<_, _> = numeric_items(&platform, &options)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        items[&2],
        [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15]
    );
    assert_eq!(items[&3], 0x2_0000_0000u64.to_le_bytes());
    assert_eq!(items[&4], [1, 0]);
    assert_eq!(items[&5], [4, 0]);
    assert_eq!(items[&0x0e], [1, 0]);
    assert_eq!(items[&0x0f], [8, 0]);
    assert_eq!(items[&0x0d], vec![0; 8 + 8 * 8]);
    assert_eq!(items[&0x8002], [1]);
    assert_eq!(&items[&0x8003][..4], &[2, 0, 0, 0]);
    assert_eq!(items[&0x8003].len(), 44);
}

#[test]
fn invalid_platforms_and_strings_are_rejected() {
    let mut platform = Platform::default();
    let options = FirmwareOptions::default();
    platform.ram_regions = vec![(0, 0x8000_0000), (0x1000, 0x1000)];
    assert!(ancillary_files(&platform, &options).is_err());
    platform.ram_regions = vec![(u64::MAX - 1, 4)];
    assert!(numeric_items(&platform, &options).is_err());
    platform.ram_regions = vec![(0xfeff_c000, 0x4000)];
    assert!(numeric_items(&platform, &options).is_err());
    platform = Platform::default();
    platform.vcpus = 255;
    assert!(generate(&platform, &SmbiosConfig::default()).is_err());
    platform = Platform::default();
    let options = FirmwareOptions {
        bootorder: vec!["/pci\n/other".into()],
        ..FirmwareOptions::default()
    };
    assert!(ancillary_files(&platform, &options).is_err());
    let config = SmbiosConfig {
        bios_vendor: "bad\0string".into(),
        ..SmbiosConfig::default()
    };
    assert!(generate(&platform, &config).is_err());
}
