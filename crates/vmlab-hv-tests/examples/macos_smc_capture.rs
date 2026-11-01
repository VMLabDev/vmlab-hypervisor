//! Black-box SMC key metadata and nonsecret values, using an explicitly fake OSK.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use vmlab_hv_tests::process::ManagedChild;

struct Qtest(BufReader<UnixStream>);
impl Qtest {
    fn call(&mut self, command: &str) -> String {
        writeln!(self.0.get_mut(), "{command}").unwrap();
        loop {
            let mut line = String::new();
            self.0.read_line(&mut line).unwrap();
            assert!(!line.is_empty(), "qtest disconnected");
            if line.starts_with("IRQ ") {
                continue;
            }
            return line
                .trim()
                .strip_prefix("OK")
                .expect("qtest request failed")
                .trim()
                .into();
        }
    }
    fn out(&mut self, port: u16, value: u8) {
        self.call(&format!("outb {port:#x} {value:#x}"));
    }
    fn input(&mut self, port: u16) -> u8 {
        u8::from_str_radix(
            self.call(&format!("inb {port:#x}"))
                .trim_start_matches("0x"),
            16,
        )
        .unwrap()
    }
    fn smc(&mut self, command: u8, key: &[u8; 4], length: u8) -> serde_json::Value {
        self.out(0x304, command);
        let mut sequence = vec![self.input(0x304)];
        for byte in key {
            self.out(0x300, *byte);
            sequence.push(self.input(0x304));
        }
        self.out(0x300, length);
        let status = self.input(0x304);
        sequence.push(status);
        let mut data = Vec::new();
        while self.input(0x304) & 1 != 0 && data.len() < usize::from(length) {
            data.push(self.input(0x300));
        }
        serde_json::json!({"status":status,"error":self.input(0x31e),"data":data,"sequence":sequence,"completed":self.input(0x304)})
    }
}
fn main() {
    let output = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .expect("output JSON path required"),
    );
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("qtest.sock");
    let fake = "FAKE".repeat(16);
    let mut command = Command::new("systemd-run");
    command
        .args([
            "--user", "--scope", "-q", "-p", "MemoryMax=24G", "-p", "MemorySwapMax=0", "--",
            "qemu-system-x86_64", "-machine", "q35", "-accel", "kvm", "-m", "8192M", "-smp", "4",
            "-cpu",
        ])
        .arg("Skylake-Client-v4,-hle,-rtm,kvm=on,vendor=GenuineIntel,vmware-cpuid-freq=on,+invtsc,tsc-frequency=4291934000")
        .args(["-nodefaults", "-display", "none", "-S", "-device"])
        .arg(format!("isa-applesmc,osk={fake}"))
        .arg("-qtest")
        .arg(format!("unix:{},server=on,wait=off", socket.display()))
        .args(["-qtest-log", "/dev/null"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ManagedChild::spawn(&mut command).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(child.try_wait().unwrap().is_none(), "QEMU failed to start");
        assert!(Instant::now() < deadline, "QEMU qtest timeout");
        std::thread::sleep(Duration::from_millis(20));
    }
    let stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut qtest = Qtest(BufReader::new(stream));
    let mut commands = serde_json::Map::new();
    for command in [0x10, 0x11, 0x12, 0x13, 0xff] {
        qtest.out(0x304, command);
        let statuses: Vec<_> = (0..8).map(|_| qtest.input(0x304)).collect();
        commands.insert(format!("{command:02x}"), serde_json::json!(statuses));
    }
    let mut records = serde_json::Map::new();
    for key in [b"#KEY", b"BEMB", b"MSSD", b"MSSW", b"NATJ", b"OSWD"] {
        let length = if key == b"#KEY" { 4 } else { 1 };
        let record = qtest.smc(0x10, key, length);
        records.insert(std::str::from_utf8(key).unwrap().into(), record);
    }
    // Read the firmware directory and e820 without booting or retaining RAM.
    qtest.call("outw 0x510 0x19");
    let mut read = |n| -> Vec<u8> { (0..n).map(|_| qtest.input(0x511)).collect() };
    let count = u32::from_be_bytes(read(4).try_into().unwrap());
    let directory = read(count as usize * 64);
    let e820 = directory
        .as_chunks::<64>()
        .0
        .iter()
        .find(|entry| entry[8..16] == *b"etc/e820")
        .unwrap();
    let selector = u16::from_be_bytes(e820[4..6].try_into().unwrap());
    let size = u32::from_be_bytes(e820[..4].try_into().unwrap());
    qtest.call(&format!("outw 0x510 {selector:#x}"));
    let e820: Vec<_> = (0..size).map(|_| qtest.input(0x511)).collect();
    let result = serde_json::json!({"qemu":"11.1.2","fake_osk":true,"keys":records,"command_status":commands,"e820":e820});
    std::fs::write(output, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    drop(child);
    println!("SMC nonsecret reads and e820 captured; OSK keys were not queried");
}
