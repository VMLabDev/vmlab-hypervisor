use vm_memory::{Bytes, GuestAddress};
use vmlab_hv_devices::{BusDevice, GuestMemory, Snapshot};
use vmlab_hv_firmware::FwCfg;

fn setup() -> (FwCfg, GuestMemory) {
    let memory = GuestMemory::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
    (FwCfg::new(memory.clone()), memory)
}

fn select(device: &mut FwCfg, key: u16) {
    device.write(0, &key.to_le_bytes());
}

fn read(device: &mut FwCfg, n: usize) -> Vec<u8> {
    (0..n)
        .map(|_| {
            let mut b = [0];
            device.read(1, &mut b);
            b[0]
        })
        .collect()
}

fn dma(device: &mut FwCfg, mem: &GuestMemory, control: u32, length: u32, address: u64) -> u32 {
    let mut descriptor = control.to_be_bytes().to_vec();
    descriptor.extend(length.to_be_bytes());
    descriptor.extend(address.to_be_bytes());
    mem.write_slice(&descriptor, GuestAddress(0x1000)).unwrap();
    device.write(4, &0u32.to_be_bytes());
    device.write(8, &0x1000u32.to_be_bytes());
    u32::from_be(mem.read_obj::<u32>(GuestAddress(0x1000)).unwrap())
}

#[test]
fn selector_stream_directory_and_dma_signature() {
    let (mut device, _) = setup();
    assert_eq!(read(&mut device, 6), b"QEMU\0\0");
    select(&mut device, 0);
    assert_eq!(read(&mut device, 4), b"QEMU");
    select(&mut device, 1);
    assert_eq!(read(&mut device, 4), 3u32.to_le_bytes());
    select(&mut device, 0x1234);
    assert_eq!(read(&mut device, 4), [0; 4]);
    device.add_numeric(0x8002, vec![1, 2]).unwrap();
    select(&mut device, 0xc002);
    assert_eq!(read(&mut device, 2), [1, 2]);
    let z = device.add_file("z", vec![3; 5], false).unwrap();
    let a = device.add_file("a", vec![4; 7], true).unwrap();
    select(&mut device, 0x19);
    let directory = read(&mut device, 132);
    assert_eq!(&directory[..4], &2u32.to_be_bytes());
    for (offset, size, key, name) in [(4, 7u32, a, b'a'), (68, 5, z, b'z')] {
        assert_eq!(&directory[offset..offset + 4], &size.to_be_bytes());
        assert_eq!(&directory[offset + 4..offset + 6], &key.to_be_bytes());
        assert_eq!(&directory[offset + 6..offset + 8], &[0; 2]);
        assert_eq!(directory[offset + 8], name);
        assert!(directory[offset + 9..offset + 64].iter().all(|b| *b == 0));
    }
    let mut signature = [0; 8];
    device.read(4, &mut signature);
    assert_eq!(&signature, b"QEMU CFG");
    assert!(device.add_file("a", vec![], false).is_err());
    assert!(device.add_file(&"x".repeat(56), vec![], false).is_err());
    assert!(device.add_numeric(0, vec![]).is_err());
}

#[test]
fn dma_read_skip_select_write_and_errors() {
    let (mut device, mem) = setup();
    let key = device.add_file("rw", b"abcdef".to_vec(), true).unwrap();
    assert_eq!(
        dma(&mut device, &mem, ((key as u32) << 16) | 8 | 4, 2, u64::MAX),
        0
    );
    assert_eq!(dma(&mut device, &mem, 2, 6, 0x2000), 0);
    let mut data = [0; 6];
    mem.read_slice(&mut data, GuestAddress(0x2000)).unwrap();
    assert_eq!(&data, b"cdef\0\0");
    mem.write_slice(b"XYZ", GuestAddress(0x2000)).unwrap();
    assert_eq!(
        dma(&mut device, &mem, ((key as u32) << 16) | 8 | 16, 3, 0x2000),
        0
    );
    assert_eq!(device.file("rw"), Some(b"XYZdef".as_slice()));
    assert_eq!(dma(&mut device, &mem, 16, 4, 0x2000), 1);
    assert_eq!(device.file("rw"), Some(b"XYZdef".as_slice()));
    assert_eq!(dma(&mut device, &mem, 8 | 16, 1, 0x2000), 1);
    assert_eq!(dma(&mut device, &mem, 8 | 2, 4, 0xffff), 1);
    assert_eq!(dma(&mut device, &mem, 8 | 2, 4, u64::MAX), 1);
    assert_eq!(
        dma(&mut device, &mem, ((key as u32) << 16) | 8 | 16, 3, 0xffff),
        1
    );
    assert_eq!(device.file("rw"), Some(b"XYZdef".as_slice()));
    select(&mut device, key);
    device.write(1, b"!");
    assert_eq!(read(&mut device, 1), b"X");
}

#[test]
fn dma_64bit_trigger_precedence_and_restore() {
    let (mut device, mem) = setup();
    let key = device.add_file("rw", b"abcd".to_vec(), true).unwrap();
    select(&mut device, key);
    assert_eq!(read(&mut device, 1), b"a");
    device.write(4, &0x1234u32.to_be_bytes());
    let state = device.save_state().unwrap();
    select(&mut device, 0);
    device.restore_state(&state).unwrap();
    assert_eq!(read(&mut device, 1), b"b");
    let mut descriptor = (8u32 | 2 | 16 | 4).to_be_bytes().to_vec();
    descriptor.extend(4u32.to_be_bytes());
    descriptor.extend(0x2000u64.to_be_bytes());
    mem.write_slice(&descriptor, GuestAddress(0x1000)).unwrap();
    device.write(4, &0x1000u64.to_be_bytes());
    let mut data = [0; 4];
    mem.read_slice(&mut data, GuestAddress(0x2000)).unwrap();
    assert_eq!(&data, b"QEMU");
    assert_eq!(
        dma(&mut device, &mem, ((key as u32) << 16) | 8 | 16, 2, 0x2000),
        0
    );
    device.restore_state(&state).unwrap();
    assert_eq!(device.file("rw"), Some(b"abcd".as_slice()));
    let mut incompatible = FwCfg::new(mem);
    assert!(incompatible.restore_state(&state).is_err());
    assert!(device.restore_state(b"bad").is_err());
}

#[test]
fn complete_platform_inputs_are_reachable_through_directory() {
    let (_, mem) = setup();
    let mut device = FwCfg::for_platform(mem, &Default::default(), &Default::default()).unwrap();
    select(&mut device, 0x19);
    let count = u32::from_be_bytes(read(&mut device, 4).try_into().unwrap());
    let entries = read(&mut device, count as usize * 64);
    for entry in entries.as_chunks::<64>().0 {
        let size = u32::from_be_bytes(entry[..4].try_into().unwrap()) as usize;
        let key = u16::from_be_bytes(entry[4..6].try_into().unwrap());
        let end = entry[8..].iter().position(|b| *b == 0).unwrap();
        let name = std::str::from_utf8(&entry[8..8 + end]).unwrap();
        let expected = device.file(name).unwrap().to_vec();
        select(&mut device, key);
        assert_eq!(read(&mut device, size), expected);
    }
    for name in [
        "etc/acpi/rsdp",
        "etc/acpi/tables",
        "etc/table-loader",
        "etc/smbios/smbios-anchor",
        "etc/smbios/smbios-tables",
        "etc/e820",
        "bootorder",
        "etc/boot-fail-wait",
        "etc/system-states",
    ] {
        assert!(device.file(name).is_some(), "{name}");
    }
}

#[test]
fn dma_chunk_boundaries_and_invalid_register_accesses() {
    let (mut device, mem) = setup();
    let expected: Vec<_> = (0..8193).map(|i| (i % 251) as u8).collect();
    let key = device.add_file("large", expected.clone(), false).unwrap();
    assert_eq!(
        dma(
            &mut device,
            &mem,
            ((key as u32) << 16) | 8 | 2,
            8194,
            0x2000
        ),
        0
    );
    let mut actual = vec![0; 8194];
    mem.read_slice(&mut actual, GuestAddress(0x2000)).unwrap();
    assert_eq!(&actual[..8193], expected);
    assert_eq!(actual[8193], 0);
    let mut data = [0xff; 8];
    device.read(u64::MAX, &mut data);
    assert_eq!(data, [0; 8]);
    device.write(4, &u64::MAX.to_be_bytes());
    select(&mut device, 0);
    assert_eq!(read(&mut device, 4), b"QEMU");
    assert_eq!(dma(&mut device, &mem, 8, 0, u64::MAX), 0);
    assert_eq!(read(&mut device, 4), b"QEMU");
}
