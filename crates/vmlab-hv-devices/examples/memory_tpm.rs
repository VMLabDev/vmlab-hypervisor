//! Exercise the existing TPM device/backend during the helper memory probe.

use std::time::{Duration, Instant};
use vmlab_hv_devices::{BusDevice, Snapshot, tpm::TpmTis};

fn execute(device: &mut TpmTis, command: u32, payload: &[u8]) {
    let mut packet = vec![0x80, 1];
    packet.extend_from_slice(&(10 + payload.len() as u32).to_be_bytes());
    packet.extend_from_slice(&command.to_be_bytes());
    packet.extend_from_slice(payload);
    device.write(0, &[2]);
    device.write(0x18, &[0x40]);
    device.write(0x24, &packet);
    device.write(0x18, &[0x20]);
    assert_eq!(device.last_error(), None);
    let mut header = [0; 10];
    device.read(0x24, &mut header);
    assert_eq!(&header[6..], &[0; 4], "TPM response {header:x?}");
    let length = u32::from_be_bytes(header[2..6].try_into().unwrap()) as usize;
    assert!((10..=4096).contains(&length));
    device.read(0x24, &mut vec![0; length - 10]);
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let socket = arguments.next().expect("swtpm control socket");
    let seconds = arguments
        .next()
        .expect("duration in seconds")
        .parse::<u64>()
        .unwrap();
    let mut device = TpmTis::connect(socket, None).unwrap();
    execute(&mut device, 0x144, &[0, 0]);
    let start = Instant::now();
    let mut commands = 0;
    while start.elapsed() < Duration::from_secs(seconds) {
        execute(&mut device, 0x17b, &[0, 16]);
        commands += 1;
        if commands % 100 == 0 {
            let state = device.save_state().unwrap();
            device.restore_state(&state).unwrap();
            println!("TPM commands={commands}, snapshot bytes={}", state.len());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    device.shutdown().unwrap();
}
