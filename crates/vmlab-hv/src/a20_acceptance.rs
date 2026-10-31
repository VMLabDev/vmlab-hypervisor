//! P1/S1: CPU-visible A20 masking, in-place reset, and snapshot restoration.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Control, Event, Outcome};
use vmlab_hv_tests::process::ManagedChild;

// The ROM uses no BIOS services. Real-mode accesses cross 1 MiB through
// FFFF:0510; flat protected-mode accesses also test the 2/3-MiB pair.
const PROBE: &str = r#"
.section .text
.code16
.global _start
_start:
    cli
    cld
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %ss
    movw $0x8000, %sp

.macro uart base
    movw $(\base + 3), %dx
    movb $0x83, %al
    outb %al, %dx
    movw $\base, %dx
    movb $1, %al
    outb %al, %dx
    incw %dx
    xorb %al, %al
    outb %al, %dx
    movw $(\base + 3), %dx
    movb $3, %al
    outb %al, %dx
    decw %dx
    movb $7, %al
    outb %al, %dx
    movw $(\base + 4), %dx
    movb $0x0b, %al
    outb %al, %dx
.endm
    uart 0x3f8
    uart 0x2f8

    movw $0xffff, %ax
    movw %ax, %es
    movb $0x11, 0x500
    movb $0x22, %es:0x510
    cmpb $0x11, 0x500
    jne failed16
    cmpb $0x22, %es:0x510
    jne failed16

    movb $0xd1, %al
    outb %al, $0x64
    movb $1, %al
    outb %al, $0x60
    cmpb $0x11, %es:0x510
    jne failed16
    movb $0x33, %es:0x510
    cmpb $0x33, 0x500
    jne failed16
    movb $0xd1, %al
    outb %al, $0x64
    movb $3, %al
    outb %al, $0x60
    cmpb $0x22, %es:0x510
    jne failed16
    cmpb $0x33, 0x500
    jne failed16

    xorb %al, %al
    outb %al, $0x92
    cmpb $0x33, %es:0x510
    jne failed16
    movb $0x44, %es:0x510
    cmpb $0x44, 0x500
    jne failed16
    movb $2, %al
    outb %al, $0x92
    cmpb $0x22, %es:0x510
    jne failed16
    cmpb $0x44, 0x500
    jne failed16

    lgdtl %cs:(gdtr - _start)
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl $8, $protected

failed16:
    movw $(bad16 - _start), %si
    movw $0x402, %dx
1:  cs lodsb
    testb %al, %al
    jz 2f
    outb %al, %dx
    jmp 1b
2:  hlt
    jmp 2b

.balign 8
gdt:
    .quad 0
    .quad 0x00cf9b000000ffff
    .quad 0x00cf93000000ffff
    .quad 0x00009b0f0000ffff
gdt_end:
gdtr:
    .word gdt_end - gdt - 1
    .long gdt
bad16:
    .asciz "A20:FAIL:REAL\n"

.code32
protected:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl $0x8000, %esp
    call seed
    movl $boot, %esi
    call print
command:
    movw $0x3fd, %dx
    inb %dx, %al
    testb $1, %al
    jz command
    movw $0x3f8, %dx
    inb %dx, %al
    cmpb $'i', %al
    je i8042_off
    cmpb $'f', %al
    je fast_off
    cmpb $'a', %al
    je i8042_on
    cmpb $'e', %al
    je fast_on
    cmpb $'p', %al
    je persist_off
    cmpb $'o', %al
    je persist_on
    cmpb $'r', %al
    je fast_reset
    cmpb $'k', %al
    je keyboard_reset
    cmpb $'l', %al
    je benchmark
    jmp failed32

i8042_off:
    movb $0xd1, %al
    outb %al, $0x64
    movb $1, %al
    outb %al, $0x60
    call check_off
    movl $i_off, %esi
    call print
    jmp command
fast_off:
    xorb %al, %al
    outb %al, $0x92
    call check_off
    movl $f_off, %esi
    call print
    jmp command
i8042_on:
    movb $0xd1, %al
    outb %al, $0x64
    movb $3, %al
    outb %al, $0x60
    call check_on
    call seed
    movl $i_on, %esi
    call print
    jmp command
fast_on:
    movb $2, %al
    outb %al, $0x92
    call check_on
    call seed
    movl $f_on, %esi
    call print
    jmp command
persist_off:
    call check_off_persistent
    movl $p_off, %esi
    call print
    jmp command
persist_on:
    cmpl $0x11223344, 0x500
    jne failed32
    cmpl $0x55667788, 0x100500
    jne failed32
    cmpl $0xaabbccdd, 0x200500
    jne failed32
    cmpl $0xeeff0011, 0x300500
    jne failed32
    cmpl $0xdeadbeef, 0x1f0000
    jne failed32
    movl $p_on, %esi
    call print
    jmp command
fast_reset:
    movb $1, %al
    outb %al, $0x92
    jmp reset_wait
keyboard_reset:
    movb $0xfe, %al
    outb %al, $0x64
reset_wait:
    hlt
    jmp reset_wait

benchmark:
    ljmp $0x18, $(real_entry - _start)
.code16
real_entry:
    movl %cr0, %eax
    andl $0xfffffffe, %eax
    movl %eax, %cr0
    ljmp $0xf000, $(real_loop - _start)
real_loop:
    xorw %ax, %ax
    movw %ax, %ds
    movw $0xffff, %ax
    movw %ax, %es
    movl $100000000, %ecx
7:  movb %es:0x510, %al
    xorb %al, %bl
    decl %ecx
    jnz 7b
    lgdtl %cs:(gdtr - _start)
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl $8, $benchmark_return
.code32
benchmark_return:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl $loop_done, %esi
    call print
    jmp command

seed:
    movl $0x11223344, 0x500
    movl $0x55667788, 0x100500
    movl $0xaabbccdd, 0x200500
    movl $0xeeff0011, 0x300500
    movl $0xdeadbeef, 0x1f0000
    ret
check_off:
    cmpl $0x11223344, 0x500
    jne failed32
    cmpl $0x11223344, 0x100500
    jne failed32
    cmpl $0xaabbccdd, 0x200500
    jne failed32
    cmpl $0xaabbccdd, 0x300500
    jne failed32
    movl 0xf0000, %eax
    cmpl %eax, 0x1f0000
    jne failed32
    movl $0x44332211, 0x100500
    cmpl $0x44332211, 0x500
    jne failed32
    movl $0xddccbbaa, 0x300500
    cmpl $0xddccbbaa, 0x200500
    jne failed32
    ret
check_off_persistent:
    cmpl $0x44332211, 0x500
    jne failed32
    cmpl $0x44332211, 0x100500
    jne failed32
    cmpl $0xddccbbaa, 0x200500
    jne failed32
    cmpl $0xddccbbaa, 0x300500
    jne failed32
    movl 0xf0000, %eax
    cmpl %eax, 0x1f0000
    jne failed32
    ret
check_on:
    cmpl $0x44332211, 0x500
    jne failed32
    cmpl $0x55667788, 0x100500
    jne failed32
    cmpl $0xddccbbaa, 0x200500
    jne failed32
    cmpl $0xeeff0011, 0x300500
    jne failed32
    cmpl $0xdeadbeef, 0x1f0000
    jne failed32
    ret

print:
    lodsb
    testb %al, %al
    jz 5f
    movb %al, %bl
    movw $0x3fd, %dx
3:  inb %dx, %al
    testb $0x20, %al
    jz 3b
    movw $0x3f8, %dx
    movb %bl, %al
    outb %al, %dx
    movw $0x2fd, %dx
4:  inb %dx, %al
    testb $0x20, %al
    jz 4b
    movw $0x2f8, %dx
    movb %bl, %al
    outb %al, %dx
    jmp print
5:  ret
failed32:
    movl $bad32, %esi
    call print
6:  hlt
    jmp 6b

boot:  .asciz "BOOT:A20:REAL+PM:ON\n"
i_off: .asciz "I8042:OFF\n"
f_off: .asciz "FAST:OFF\n"
i_on:  .asciz "I8042:ON\n"
f_on:  .asciz "FAST:ON\n"
p_off: .asciz "OFF:PERSIST\n"
p_on:  .asciz "ON:PERSIST\n"
bad32: .asciz "A20:FAIL:PM\n"
loop_done: .asciz "LOOP:DONE\n"

.org 0xfff0
.code16
    ljmp $0xf000, $0
    .fill 11, 1, 0xff
"#;

fn build_rom(directory: &Path) -> PathBuf {
    let source = directory.join("probe.S");
    let object = directory.join("probe.o");
    let rom = directory.join("probe.bin");
    let linker = directory.join("probe.ld");
    std::fs::write(&source, PROBE).unwrap();
    std::fs::write(
        &linker,
        "SECTIONS { . = 0xf0000; .text : { *(.text) } /DISCARD/ : { *(*) } }\n",
    )
    .unwrap();
    for (program, arguments) in [
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
                "-T".into(),
                linker.as_os_str().to_owned(),
                "--oformat".into(),
                "binary".into(),
                "-o".into(),
                rom.as_os_str().to_owned(),
                object.as_os_str().to_owned(),
            ],
        ),
    ] {
        let output = Command::new(program).args(arguments).output().unwrap();
        assert!(
            output.status.success(),
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let bytes = std::fs::read(&rom).unwrap();
    assert_eq!(bytes.len(), 65536);
    assert_eq!(&bytes[0xfff0..0xfff5], &[0xea, 0, 0, 0, 0xf0]);
    rom
}

fn assembler_available() -> bool {
    for tool in ["as", "ld"] {
        if !Command::new(tool)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            eprintln!("SKIP A20 probe: GNU {tool} is unavailable");
            return false;
        }
    }
    true
}

#[test]
fn a20_probe_rom_assembles_without_guest() {
    if !assembler_available() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    build_rom(directory.path());
}

struct ProbeVm {
    child: ManagedChild,
    directory: tempfile::TempDir,
    control: Client,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Drop for ProbeVm {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let _ = self.control.call(Control::Quit);
            let _ = self.child.wait();
            eprintln!("{}", self.diagnostics());
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() || std::env::var_os("VMLAB_HV_ACCEPTANCE_KEEP").is_some() {
            self.directory.disable_cleanup(true);
            eprintln!("A20 probe artifacts: {}", self.directory.path().display());
        }
    }
}

impl ProbeVm {
    fn diagnostics(&self) -> String {
        ["stderr.log", "serial.log"]
            .map(|name| {
                format!(
                    "{name}: {}",
                    std::fs::read_to_string(self.directory.path().join(name)).unwrap_or_default()
                )
            })
            .join("\n")
    }

    fn call(&mut self, command: Control) {
        let reply = self.control.call(command).unwrap();
        assert!(
            matches!(reply, Outcome::Ok { .. }),
            "{reply:?}\n{}",
            self.diagnostics()
        );
    }

    fn expect(&mut self, expected: &str) {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap_or_else(|error| {
            panic!("waiting for {expected}: {error}\n{}", self.diagnostics())
        });
        assert_eq!(line.trim_end(), expected, "{}", self.diagnostics());
    }

    fn command(&mut self, command: u8, expected: &str) {
        self.writer.write_all(&[command]).unwrap();
        self.expect(expected);
    }

    fn load(&mut self, name: &str) {
        self.call(Control::Pause);
        self.call(Control::SnapshotLoad { name: name.into() });
        self.call(Control::Resume);
    }

    fn reset(&mut self, command: u8) {
        self.control.take_events();
        let before = std::fs::read_to_string(self.directory.path().join("serial.log"))
            .unwrap_or_default()
            .matches("BOOT:A20:REAL+PM:ON")
            .count();
        self.writer.write_all(&[command]).unwrap();
        self.expect("BOOT:A20:REAL+PM:ON");
        self.call(Control::Status);
        assert_eq!(
            self.control
                .take_events()
                .iter()
                .filter(|event| **event == Event::Reset)
                .count(),
            1,
            "reset must produce exactly one reset event\n{}",
            self.diagnostics()
        );
        let after = std::fs::read_to_string(self.directory.path().join("serial.log"))
            .unwrap()
            .matches("BOOT:A20:REAL+PM:ON")
            .count();
        assert_eq!(after, before + 1, "a fresh boot must follow reset");
        self.command(b'o', "ON:PERSIST");
    }
}

fn launch() -> Option<ProbeVm> {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP A20 CPU probe: /dev/kvm is unavailable");
        return None;
    }
    if !assembler_available() {
        return None;
    }
    let temporary_root = std::env::var_os("TMPDIR").unwrap_or_else(|| "/var/tmp".into());
    let directory = tempfile::Builder::new()
        .prefix("hv-a20-")
        .tempdir_in(temporary_root)
        .unwrap();
    let path = directory.path();
    let rom = build_rom(path);
    let disk = path.join("state.qcow2");
    vmlab_disk::ops::create_qcow2(&disk, 8 << 20, None, None).unwrap();
    let control_path = path.join("control.sock");
    let serial_path = path.join("probe.sock");
    let description = serde_json::json!({
        "name": "pc-a20-probe", "machine": "pc", "arch": "x86_64", "accel": "kvm",
        "vcpus": 1, "memory_mib": 16,
        "boot": {"kind": "firmware", "firmware": "seabios", "code": rom},
        "disks": [{"id": "state", "bus": "ide", "channel": 0, "unit": 0,
            "path": disk, "format": "qcow2", "read_only": false, "vmstate": true}],
        "ports": [{"name": "probe", "transport": "com1", "socket": serial_path}],
        "nics": [], "serial_log": path.join("serial.log"), "serial_log_port": "com2"
    });
    let machine_path = path.join("machine.json");
    std::fs::write(&machine_path, serde_json::to_vec(&description).unwrap()).unwrap();
    let stderr = std::fs::File::create(path.join("stderr.log")).unwrap();
    let binary = std::env::var_os("VMLAB_HV_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(vmlab_hv_tests::hv_binary);
    let mut child = ManagedChild::spawn(
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
            .arg(machine_path)
            .arg("--control")
            .arg(&control_path)
            .arg("--paused")
            .env("TMPDIR", path)
            .env("VMLAB_HV_DIAGNOSTICS", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr)),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let control = loop {
        if let Ok(control) = Client::connect(&control_path) {
            break control;
        }
        assert!(
            child.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "A20 probe control did not bind: {}",
            std::fs::read_to_string(path.join("stderr.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let writer = UnixStream::connect(serial_path).unwrap();
    writer
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    writer
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let reader = BufReader::new(writer.try_clone().unwrap());
    Some(ProbeVm {
        child,
        directory,
        control,
        reader,
        writer,
    })
}

#[test]
fn pc_a20_cpu_wraparound_reset_and_snapshot() {
    let Some(mut vm) = launch() else { return };
    vm.call(Control::Resume);
    vm.expect("BOOT:A20:REAL+PM:ON");
    vm.call(Control::SnapshotSave {
        name: "enabled".into(),
    });

    vm.command(b'i', "I8042:OFF");
    vm.call(Control::SnapshotSave {
        name: "i8042-off".into(),
    });
    vm.command(b'e', "FAST:ON");
    vm.load("i8042-off");
    vm.command(b'p', "OFF:PERSIST");
    vm.command(b'a', "I8042:ON");

    vm.command(b'f', "FAST:OFF");
    vm.call(Control::SnapshotSave {
        name: "fast-off".into(),
    });
    vm.command(b'a', "I8042:ON");
    vm.load("fast-off");
    vm.command(b'p', "OFF:PERSIST");
    vm.command(b'e', "FAST:ON");

    vm.command(b'i', "I8042:OFF");
    vm.load("enabled");
    vm.command(b'o', "ON:PERSIST");
    vm.command(b'i', "I8042:OFF");
    vm.reset(b'r');
    vm.command(b'f', "FAST:OFF");
    vm.reset(b'k');
    vm.call(Control::Quit);
    assert!(vm.child.wait().unwrap().success(), "{}", vm.diagnostics());
}

#[test]
fn pc_a20_real_mode_loop_cost() {
    let Some(mut vm) = launch() else { return };
    vm.call(Control::Resume);
    vm.expect("BOOT:A20:REAL+PM:ON");
    for enabled in [true, false] {
        if !enabled {
            vm.command(b'i', "I8042:OFF");
        }
        let mut samples_ms = Vec::new();
        for _ in 0..3 {
            let started = Instant::now();
            vm.command(b'l', "LOOP:DONE");
            samples_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        eprintln!(
            "A20 real-mode loop: enabled={enabled}, iterations=100000000, samples_ms={samples_ms:?}, mean_ms={}",
            samples_ms.iter().sum::<f64>() / samples_ms.len() as f64,
        );
    }
    vm.call(Control::Quit);
    assert!(vm.child.wait().unwrap().success(), "{}", vm.diagnostics());
}
