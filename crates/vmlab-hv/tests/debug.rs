//! S1 debugger acceptance using a small in-memory guest and real KVM exits.

use std::collections::BTreeMap;
use std::fs::File;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Capability, Command as Ctrl, ErrorCode, Outcome, RunState};
use vmlab_hv_proto::debug::{
    AddressSpace, Breakpoint, BreakpointKind, DebugRequest as Request, DebugResponse as Response,
    DebugStatus, MEMORY_LIMIT, StopReason,
};
use vmlab_hv_tests::process::ManagedChild;

const PHYSICAL: AddressSpace = AddressSpace::Physical;
const CODE: &[u8] = &[
    0xb8, 0x34, 0x12, 0xa3, 0, 0x90, 0xff, 0x06, 0, 0x90, 0xa1, 0, 0x90, 0xeb, 0xf7,
];

struct Machine {
    dir: tempfile::TempDir,
    child: ManagedChild,
    client: Client,
}

impl Machine {
    fn launch(vcpus: u8) -> Option<Self> {
        Self::launch_rom(vcpus, None)
    }

    fn launch_rom(vcpus: u8, image: Option<Vec<u8>>) -> Option<Self> {
        if !Path::new("/dev/kvm").exists() {
            eprintln!("SKIP S1 debug feature: /dev/kvm is absent");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let custom = image.is_some();
        let mut rom = image.unwrap_or_else(|| vec![0xf4; 0x10000]);
        assert_eq!(rom.len(), 0x10000);
        if !custom {
            rom[0xfff0..0xfff5].copy_from_slice(&[0xea, 0, 0, 0, 0xf0]);
        }
        let firmware = dir.path().join("firmware.bin");
        std::fs::write(&firmware, rom).unwrap();
        let machine = dir.path().join("machine.json");
        let control = dir.path().join("control.sock");
        std::fs::write(
            &machine,
            serde_json::to_vec(&json!({
                "name":"debug-test", "machine":"q35", "arch":"x86_64", "accel":"kvm",
                "vcpus":vcpus, "memory_mib":16,
                "boot":{"kind":"firmware", "firmware":"seabios", "code":firmware},
                "disks":[], "nics":[], "ports":[], "shares":[],
                "serial_log":dir.path().join("serial.log"),
            }))
            .unwrap(),
        )
        .unwrap();
        let mut command = Command::new("systemd-run");
        command
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
            .arg(env!("CARGO_BIN_EXE_vmlab-hv"))
            .arg("--machine")
            .arg(machine)
            .arg("--control")
            .arg(&control)
            .arg("--paused")
            .env("VMLAB_HV_LOG", "off")
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                File::create(dir.path().join("stderr.log")).unwrap(),
            ));
        let mut child = ManagedChild::spawn(&mut command).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let client = loop {
            if let Ok(client) = Client::connect(&control) {
                break client;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "startup: {}",
                std::fs::read_to_string(dir.path().join("stderr.log")).unwrap()
            );
            assert!(Instant::now() < deadline, "control startup timeout");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(client.has_capability(Capability::Debug));
        let mut guest = Self { dir, child, client };
        guest.debug(Request::Pause);
        if !custom {
            guest.debug(Request::WriteMemory {
                space: PHYSICAL,
                address: 0x8000,
                data: CODE.to_vec(),
            });
            guest.debug(Request::SetRegisters {
                vcpu: 0,
                values: [
                    ("cs", 0),
                    ("ds", 0),
                    ("ss", 0),
                    ("rip", 0x8000),
                    ("rsp", 0x7000),
                ]
                .into_iter()
                .map(|(name, value)| (name.into(), value))
                .collect(),
            });
        }
        Some(guest)
    }

    fn debug(&mut self, request: Request) -> Response {
        match self.client.call(Ctrl::Debug { request }).unwrap() {
            Outcome::Debug { response } => response,
            other => panic!("debug request: {other:?}"),
        }
    }

    fn rejected(&mut self, request: Request, expected: ErrorCode) {
        assert!(
            matches!(self.client.call(Ctrl::Debug { request }).unwrap(), Outcome::Error { code, .. } if code == expected)
        );
    }

    fn status(&mut self) -> DebugStatus {
        match self.debug(Request::Status) {
            Response::Status { status } => status,
            other => panic!("{other:?}"),
        }
    }

    fn wait_stop(&mut self, sequence: u64) -> DebugStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = self.status();
            if status.state == RunState::Paused
                && !status.stepping
                && status
                    .stop
                    .as_ref()
                    .is_some_and(|stop| stop.sequence > sequence)
            {
                return status;
            }
            if Instant::now() >= deadline {
                self.debug(Request::Pause);
                let Response::Registers { registers, .. } =
                    self.debug(Request::ReadRegisters { vcpu: 0 })
                else {
                    panic!()
                };
                panic!(
                    "debugger stop timeout: {status:?}; registers: {:?}; system: {:?}; data: {:?}",
                    registers.general,
                    registers.system,
                    self.read(0x9000, 8)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn read(&mut self, address: u64, length: usize) -> Vec<u8> {
        match self.debug(Request::ReadMemory {
            space: PHYSICAL,
            address,
            length,
        }) {
            Response::Memory { data } => data,
            other => panic!("{other:?}"),
        }
    }

    fn finish(mut self) {
        self.client.call(Ctrl::Quit).unwrap();
        assert!(
            self.child.wait().unwrap().success(),
            "{}",
            std::fs::read_to_string(self.dir.path().join("stderr.log")).unwrap()
        );
    }
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn long_mode_high_address_breakpoints_and_eight_byte_watchpoints_through_s1() {
    if !Command::new("nasm")
        .arg("-v")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("SKIP S1 long-mode debugger check: nasm is unavailable");
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let image = fixture.path().join("debug64.bin");
    let output = Command::new("nasm")
        .args(["-f", "bin", "-o"])
        .arg(&image)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/debug64.asm"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let Some(mut guest) = Machine::launch_rom(1, Some(std::fs::read(image).unwrap())) else {
        return;
    };
    let write = Breakpoint {
        address: 0xffff800000009000,
        length: 8,
        kind: BreakpointKind::Write,
    };
    guest.debug(Request::AddBreakpoint { breakpoint: write });
    let sequence = guest.status().stop.unwrap().sequence;
    guest.debug(Request::Continue);
    let stopped = guest.wait_stop(sequence).stop.unwrap();
    assert_eq!(stopped.reason, StopReason::Breakpoint { breakpoint: write });
    let Response::Registers { registers, .. } = guest.debug(Request::ReadRegisters { vcpu: 0 })
    else {
        panic!()
    };
    assert_eq!(registers.system["cs_long"], 1);
    assert_ne!(registers.system["efer"] & 0x400, 0);
    assert_eq!(registers.general[16] >> 48, 0xffff);
    assert_eq!(guest.read(0x9000, 8), 0x12345678u64.to_le_bytes());
    assert_eq!(
        guest.debug(Request::ReadMemory {
            space: AddressSpace::Virtual { vcpu: 0 },
            address: write.address,
            length: 8
        }),
        Response::Memory {
            data: 0x12345678u64.to_le_bytes().to_vec()
        }
    );
    guest.debug(Request::RemoveBreakpoint { breakpoint: write });
    let execute = Breakpoint {
        address: registers.general[16],
        length: 1,
        kind: BreakpointKind::Execute,
    };
    guest.debug(Request::AddBreakpoint {
        breakpoint: execute,
    });
    guest.debug(Request::Continue);
    let stopped = guest.wait_stop(stopped.sequence).stop.unwrap();
    assert_eq!(
        stopped.reason,
        StopReason::Breakpoint {
            breakpoint: execute
        }
    );
    guest.debug(Request::Step { vcpu: 0 });
    let stopped = guest.wait_stop(stopped.sequence).stop.unwrap();
    assert_eq!(stopped.reason, StopReason::Step);
    assert_eq!(guest.read(0x9000, 8), 0x12345679u64.to_le_bytes());
    guest.debug(Request::RemoveBreakpoint {
        breakpoint: execute,
    });
    let access = Breakpoint {
        kind: BreakpointKind::ReadWrite,
        ..write
    };
    guest.debug(Request::AddBreakpoint { breakpoint: access });
    guest.debug(Request::Step { vcpu: 0 });
    let stopped = guest.wait_stop(stopped.sequence).stop.unwrap();
    assert_eq!(
        stopped.reason,
        StopReason::Breakpoint { breakpoint: access }
    );
    let Response::Registers { registers, .. } = guest.debug(Request::ReadRegisters { vcpu: 0 })
    else {
        panic!()
    };
    assert_eq!(registers.general[1], 0x12345679);
    guest.finish();
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn memory_registers_and_validation_through_s1() {
    let Some(mut guest) = Machine::launch(2) else {
        return;
    };
    guest.debug(Request::SetRegisters {
        vcpu: 0,
        values: BTreeMap::from([("fs".into(), 0x20), ("gs".into(), 0x30)]),
    });
    let Response::Registers { registers, .. } = guest.debug(Request::ReadRegisters { vcpu: 0 })
    else {
        panic!()
    };
    assert_eq!(registers.system["fs_base"], 0x200);
    assert_eq!(registers.system["gs_base"], 0x300);
    let data = vec![0xa5; MEMORY_LIMIT];
    guest.debug(Request::WriteMemory {
        space: PHYSICAL,
        address: 0x10ff0,
        data: data.clone(),
    });
    assert_eq!(guest.read(0x10ff0, data.len()), data);
    assert_eq!(
        guest.debug(Request::ReadMemory {
            space: AddressSpace::Virtual { vcpu: 0 },
            address: 0x10ff0,
            length: 16
        }),
        Response::Memory {
            data: vec![0xa5; 16]
        }
    );
    assert_eq!(guest.read(0xffff_fff0, 5), [0xea, 0, 0, 0, 0xf0]);
    assert_eq!(guest.read(0xffff0, 5), [0xea, 0, 0, 0, 0xf0]);
    guest.rejected(
        Request::WriteMemory {
            space: PHYSICAL,
            address: 0x9fff0,
            data: vec![1; 32],
        },
        ErrorCode::InvalidArgument,
    );
    assert_eq!(guest.read(0x9fff0, 16), vec![0; 16]);
    for (address, length) in [
        (u64::MAX, 4),
        (0, 0),
        (0, MEMORY_LIMIT + 1),
        (0xfee00000, 4),
    ] {
        guest.rejected(
            Request::ReadMemory {
                space: PHYSICAL,
                address,
                length,
            },
            ErrorCode::InvalidArgument,
        );
    }
    guest.rejected(
        Request::ReadRegisters { vcpu: 255 },
        ErrorCode::InvalidArgument,
    );
    guest.rejected(
        Request::SetRegisters {
            vcpu: 0,
            values: BTreeMap::from([("rax".into(), 42), ("unknown".into(), 1)]),
        },
        ErrorCode::InvalidArgument,
    );
    let Response::Registers { registers, .. } = guest.debug(Request::ReadRegisters { vcpu: 0 })
    else {
        panic!()
    };
    assert_ne!(registers.general[0], 42);
    guest.debug(Request::SetRegisters {
        vcpu: 0,
        values: BTreeMap::from([("rax".into(), 0x1234_5678)]),
    });
    let Response::Registers { mut registers, .. } = guest.debug(Request::ReadRegisters { vcpu: 0 })
    else {
        panic!()
    };
    assert_eq!(registers.general[0], 0x1234_5678);
    registers.xmm[7] = [0x87; 16];
    registers.st[3] = [0x33; 10];
    guest.debug(Request::WriteRegisters {
        vcpu: 0,
        registers: registers.clone(),
    });
    assert_eq!(
        guest.debug(Request::ReadRegisters { vcpu: 0 }),
        Response::Registers { vcpu: 0, registers }
    );
    guest.debug(Request::WriteMemory {
        space: PHYSICAL,
        address: 0x2fffe,
        data: b"needle".to_vec(),
    });
    assert_eq!(
        guest.debug(Request::SearchMemory {
            space: PHYSICAL,
            address: 0x20000,
            length: 0x20000,
            pattern: b"needle".to_vec()
        }),
        Response::Matches {
            addresses: vec![0x2fffe],
            truncated: false
        }
    );
    guest.debug(Request::Continue);
    guest.rejected(Request::ReadRegisters { vcpu: 0 }, ErrorCode::InvalidState);
    guest.debug(Request::Pause);
    guest.debug(Request::WriteMemory {
        space: PHYSICAL,
        address: 0x9000,
        data: b"ababa".to_vec(),
    });
    assert_eq!(
        guest.debug(Request::SearchMemory {
            space: PHYSICAL,
            address: 0x9000,
            length: 5,
            pattern: b"aba".to_vec()
        }),
        Response::Matches {
            addresses: vec![0x9000, 0x9002],
            truncated: false
        }
    );
    let Response::Matches {
        addresses,
        truncated,
    } = guest.debug(Request::SearchMemory {
        space: PHYSICAL,
        address: 0x40000,
        length: 2048,
        pattern: vec![0],
    })
    else {
        panic!()
    };
    assert_eq!(addresses.len(), 1024);
    assert!(truncated);
    guest.finish();
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn virtual_memory_uses_current_page_tables_and_preflights_every_page() {
    let Some(mut guest) = Machine::launch(1) else {
        return;
    };
    let Response::Registers {
        registers: original,
        ..
    } = guest.debug(Request::ReadRegisters { vcpu: 0 })
    else {
        panic!()
    };
    for (address, entry) in [
        (0x1000, 0x2003u64),
        (0x2000, 0x3003),
        (0x3000, 0x4003),
        (0x4028, 0x20003),
        (0x4030, 0x30003),
    ] {
        guest.debug(Request::WriteMemory {
            space: PHYSICAL,
            address,
            data: entry.to_le_bytes().to_vec(),
        });
    }
    guest.debug(Request::SetRegisters {
        vcpu: 0,
        values: BTreeMap::from([
            ("cr0".into(), original.system["cr0"] | 0x80000001),
            ("cr3".into(), 0x1000),
            ("cr4".into(), 0x20),
            ("efer".into(), 0x500),
        ]),
    });
    let space = AddressSpace::Virtual { vcpu: 0 };
    guest.debug(Request::WriteMemory {
        space,
        address: 0x5fff,
        data: vec![0xab, 0xcd],
    });
    assert_eq!(guest.read(0x20fff, 1), [0xab]);
    assert_eq!(guest.read(0x30000, 1), [0xcd]);
    guest.rejected(
        Request::WriteMemory {
            space,
            address: 0x6fff,
            data: vec![0x12, 0x34],
        },
        ErrorCode::InvalidArgument,
    );
    assert_eq!(guest.read(0x30fff, 1), [0]);
    guest.rejected(
        Request::ReadMemory {
            space,
            address: 0x7000,
            length: 1,
        },
        ErrorCode::InvalidArgument,
    );
    guest.debug(Request::WriteRegisters {
        vcpu: 0,
        registers: original,
    });
    guest.finish();
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn hardware_breakpoints_watchpoints_and_smp_step_through_s1() {
    let Some(mut guest) = Machine::launch(2) else {
        return;
    };
    let execute = Breakpoint {
        address: 0x8000,
        length: 1,
        kind: BreakpointKind::Execute,
    };
    let write = Breakpoint {
        address: 0x9000,
        length: 2,
        kind: BreakpointKind::Write,
    };
    guest.debug(Request::AddBreakpoint {
        breakpoint: execute,
    });
    guest.debug(Request::AddBreakpoint { breakpoint: write });
    let sequence = guest.status().stop.unwrap().sequence;
    guest.debug(Request::Continue);
    let status = guest.wait_stop(sequence);
    assert_eq!(
        status.stop.as_ref().unwrap().reason,
        StopReason::Breakpoint {
            breakpoint: execute
        }
    );
    assert_eq!(guest.read(0x9000, 2), [0, 0]);
    guest.debug(Request::Continue);
    let status = guest.wait_stop(status.stop.unwrap().sequence);
    assert_eq!(
        status.stop.as_ref().unwrap().reason,
        StopReason::Breakpoint { breakpoint: write }
    );
    assert_eq!(guest.read(0x9000, 2), [0x34, 0x12]);
    guest.debug(Request::RemoveBreakpoint {
        breakpoint: execute,
    });
    guest.debug(Request::RemoveBreakpoint { breakpoint: write });
    let other = guest.debug(Request::ReadRegisters { vcpu: 1 });
    guest.debug(Request::Step { vcpu: 0 });
    let status = guest.wait_stop(status.stop.unwrap().sequence);
    assert_eq!(status.stop.as_ref().unwrap().reason, StopReason::Step);
    assert_eq!(guest.read(0x9000, 2), [0x35, 0x12]);
    assert_eq!(guest.debug(Request::ReadRegisters { vcpu: 1 }), other);
    let access = Breakpoint {
        kind: BreakpointKind::ReadWrite,
        ..write
    };
    guest.debug(Request::AddBreakpoint { breakpoint: access });
    guest.debug(Request::Step { vcpu: 0 });
    let status = guest.wait_stop(status.stop.unwrap().sequence);
    assert_eq!(
        status.stop.unwrap().reason,
        StopReason::Breakpoint { breakpoint: access }
    );
    guest.debug(Request::Detach { resume: false });
    assert!(guest.status().breakpoints.is_empty());
    for address in 0x8000..0x8004 {
        guest.debug(Request::AddBreakpoint {
            breakpoint: Breakpoint { address, ..execute },
        });
    }
    guest.rejected(
        Request::AddBreakpoint {
            breakpoint: Breakpoint {
                address: 0x8004,
                ..execute
            },
        },
        ErrorCode::InvalidArgument,
    );
    guest.rejected(
        Request::AddBreakpoint {
            breakpoint: Breakpoint {
                address: 0x9001,
                ..write
            },
        },
        ErrorCode::InvalidArgument,
    );
    assert_eq!(guest.status().breakpoints.len(), 4);
    guest.finish();
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn gdb_attaches_edits_memory_and_registers_and_hits_a_breakpoint() {
    if !Command::new("gdb")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("SKIP S1 GDB client: gdb is unavailable");
        return;
    }
    let Some(mut guest) = Machine::launch(1) else {
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = Client::connect(guest.dir.path().join("control.sock")).unwrap();
    let bridge = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        vmlab_hv_proto::gdb::serve(stream, client)
    });
    let output = Command::new("timeout")
        .args(["30", "gdb", "-nx", "-nh", "--batch"])
        .args(["-ex", &format!("target remote {address}")])
        .args([
            "-ex",
            "set $rax = 0x4567",
            "-ex",
            "set {unsigned short}0x9000 = 0xabcd",
            "-ex",
            "p/x $rax",
            "-ex",
            "x/1hx 0x9000",
            "-ex",
            "hbreak *0x8003",
            "-ex",
            "continue",
            "-ex",
            "si",
            "-ex",
            "p/x $rax",
            "-ex",
            "delete breakpoints",
            "-ex",
            "detach",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let bridge_result = bridge.join().unwrap();
    assert!(
        output.status.success(),
        "GDB: {stdout}\n{stderr}\nbridge: {bridge_result:?}"
    );
    assert!(
        stdout.contains("0x4567") && stdout.contains("0xabcd") && stdout.contains("0x1234"),
        "{stdout}\n{stderr}"
    );
    assert!(stdout.contains("Breakpoint 1,"), "{stdout}\n{stderr}");
    bridge_result.unwrap();
    guest.debug(Request::Pause);
    assert_eq!(guest.read(0x8000, CODE.len()), CODE);
    assert!(guest.status().breakpoints.is_empty());
    guest.finish();
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn debugger_interrupt_stops_the_guest_and_eof_removes_breakpoints() {
    use std::io::{Read, Write};
    let Some(mut guest) = Machine::launch(2) else {
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = Client::connect(guest.dir.path().join("control.sock")).unwrap();
    let bridge = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        vmlab_hv_proto::gdb::serve(stream, client)
    });
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(b"$c#63").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while guest.status().state != RunState::Running {
        assert!(Instant::now() < deadline, "remote continue did not run");
        std::thread::sleep(Duration::from_millis(10));
    }
    stream.write_all(&[3]).unwrap();
    let mut byte = [0];
    while byte[0] != b'$' {
        stream.read_exact(&mut byte).unwrap();
    }
    let mut packet = Vec::new();
    loop {
        stream.read_exact(&mut byte).unwrap();
        if byte[0] == b'#' {
            break;
        }
        packet.push(byte[0]);
    }
    let mut checksum = [0; 2];
    stream.read_exact(&mut checksum).unwrap();
    assert!(
        packet.starts_with(b"S02") || packet.starts_with(b"T02"),
        "{packet:?}"
    );
    stream.write_all(b"+").unwrap();
    assert_eq!(guest.status().state, RunState::Paused);
    guest.debug(Request::ReadRegisters { vcpu: 1 });
    guest.debug(Request::AddBreakpoint {
        breakpoint: Breakpoint {
            address: 0x9000,
            length: 2,
            kind: BreakpointKind::Write,
        },
    });
    drop(stream);
    assert!(bridge.join().unwrap().is_err());
    assert_eq!(guest.status().state, RunState::Paused);
    assert!(guest.status().breakpoints.is_empty());
    guest.finish();
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn lldb_attaches_edits_memory_and_registers_and_steps() {
    let lldb = std::env::var_os("VMLAB_HV_TEST_LLDB").unwrap_or_else(|| "lldb".into());
    let probe = Command::new(&lldb).arg("--version").output();
    if !probe.as_ref().is_ok_and(|output| output.status.success()) {
        eprintln!(
            "SKIP S1 LLDB client: {}",
            match probe {
                Ok(output) => String::from_utf8_lossy(&output.stderr).into_owned(),
                Err(error) => error.to_string(),
            }
        );
        return;
    }
    let Some(mut guest) = Machine::launch(1) else {
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = Client::connect(guest.dir.path().join("control.sock")).unwrap();
    let bridge = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        vmlab_hv_proto::gdb::serve(stream, client)
    });
    let output = Command::new("timeout")
        .arg("30")
        .arg(lldb)
        .args([
            "--no-lldbinit",
            "--batch",
            "-o",
            &format!("gdb-remote {address}"),
            "-o",
            "register write rax 0x4567",
            "-o",
            "memory write --size 2 0x9000 0xabcd",
            "-o",
            "register read rax",
            "-o",
            "memory read --size 2 --count 1 --format x 0x9000",
            "-o",
            "thread step-inst",
            "-o",
            "register read rax",
            "-o",
            "process detach",
        ])
        .output()
        .unwrap();
    let bridge_result = bridge.join().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "LLDB: {stdout}\n{stderr}\nbridge: {bridge_result:?}"
    );
    for value in ["4567", "abcd", "1234"] {
        assert!(stdout.contains(value), "{stdout}\n{stderr}");
    }
    bridge_result.unwrap();
    guest.debug(Request::Pause);
    guest.finish();
}
