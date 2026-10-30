//! Black-box clock oracle using vmlab's container argv and unmodified guest.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use vmlab_cinit_proto::CtlEvent;
use vmlab_hv_tests::{MachineSpec, agent::AgentClient, ctl::CtlClient, probe_assets};

struct Qemu(Child);

impl Drop for Qemu {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn fixed_base_matches_qemu_container() {
    let assets = probe_assets();
    if let Some(reason) = assets.skip_reason() {
        eprintln!("skipping QEMU clock oracle: {reason}");
        return;
    }
    if Command::new("qemu-system-x86_64")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping QEMU clock oracle: qemu-system-x86_64 missing");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name).display().to_string();
    vmlab_disk::ops::create_qcow2(&dir.path().join("scratch.qcow2"), 2 << 30, None, None).unwrap();
    let log = std::fs::File::create(dir.path().join("qemu.log")).unwrap();
    let mut qemu = Qemu(
        Command::new("qemu-system-x86_64")
            .args([
                "-machine",
                "q35",
                "-accel",
                "kvm",
                "-cpu",
                "host",
                "-smp",
                "1",
                "-m",
                "512M",
                "-rtc",
                "base=2020-01-01T00:00:00Z",
                "-display",
                "none",
                "-vga",
                "none",
                "-kernel",
                &assets.kernel.display().to_string(),
                "-initrd",
                &assets.initrd.display().to_string(),
                "-append",
                &vmlab_hv_tests::CMDLINE.replace("quiet loglevel=3", "loglevel=7"),
                "-serial",
                &format!("file:{}", path("serial.log")),
                "-monitor",
                "none",
                "-nic",
                "none",
                "-chardev",
                &format!(
                    "socket,id=ctl0,path={},server=on,wait=off",
                    path("ctl.sock")
                ),
                "-chardev",
                &format!(
                    "socket,id=agent0,path={},server=on,wait=off",
                    path("agent.sock")
                ),
                "-device",
                "virtio-serial-pci",
                "-device",
                "virtserialport,chardev=ctl0,name=vmlab.ctl.0",
                "-device",
                "virtserialport,chardev=agent0,name=vmlab.agent.0",
                "-blockdev",
                &format!(
                    "driver=raw,node-name=rootfs,read-only=on,file.driver=file,file.filename={}",
                    assets.rootfs.unwrap().display()
                ),
                "-device",
                "virtio-blk-pci,drive=rootfs",
                "-blockdev",
                &format!(
                    "driver=qcow2,node-name=scratch,file.driver=file,file.filename={}",
                    path("scratch.qcow2")
                ),
                "-device",
                "virtio-blk-pci,drive=scratch",
                "-trace",
                &format!("enable=mc146818_rtc_ioport_*,file={}", path("rtc.trace")),
            ])
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let spec = MachineSpec::idle("clock-oracle").container_spec();
    let mut ctl = CtlClient::connect(&dir.path().join("ctl.sock"), Duration::from_secs(15))
        .unwrap_or_else(|e| {
            panic!(
                "{e}: {}",
                std::fs::read_to_string(dir.path().join("qemu.log")).unwrap()
            )
        });
    ctl.send_spec(&spec).unwrap();
    ctl.wait_event(
        &spec,
        Duration::from_secs(120),
        |e| matches!(e, CtlEvent::Idle),
        |_| {},
        || std::fs::read_to_string(dir.path().join("serial.log")).unwrap(),
    );
    let mut agent =
        AgentClient::connect(&dir.path().join("agent.sock"), Duration::from_secs(60)).unwrap();
    let out = agent
        .exec(&["/.vmlab/busybox", "date", "+%s"], Duration::from_secs(15))
        .unwrap();
    assert_eq!(out.exit_code, 0);
    let seconds: i64 = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    eprintln!("QEMU guest epoch seconds: {seconds}");
    let serial = std::fs::read_to_string(dir.path().join("serial.log")).unwrap();
    assert!(
        serial.lines().any(|line| line.contains("rtc_cmos")
            && line.contains("setting system clock to 2020-01-01T00:00:00")),
        "the kernel must initialize wall time from CMOS before cinit starts: {serial}"
    );
    for line in serial
        .lines()
        .filter(|l| l.contains("clock") || l.contains("rtc") || l.contains("2020"))
    {
        eprintln!("{line}");
    }
    qemu.0.kill().unwrap();
    qemu.0.wait().unwrap();
    let trace = std::fs::read_to_string(dir.path().join("rtc.trace")).unwrap();
    assert!(
        trace.contains("mc146818_rtc_ioport_read [0x09] -> 0x20"),
        "the guest must read the CMOS year: {trace}"
    );
    eprintln!(
        "RTC trace (last reads):\n{}",
        trace.lines().rev().take(24).collect::<Vec<_>>().join("\n")
    );
    assert!(
        (1_577_836_800..1_577_836_920).contains(&seconds),
        "{seconds}"
    );
}
