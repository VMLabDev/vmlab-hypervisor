//! S1 process outcomes and runtime diagnostics through the real control socket.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{
    Capability, Command as Ctrl, Event, Outcome, ProcessExitReason, RunState,
};
use vmlab_hv_tests::process::ManagedChild;

struct Machine {
    directory: tempfile::TempDir,
    child: ManagedChild,
    client: Client,
}

impl Machine {
    fn launch(code: &[u8], runtime_error: bool) -> Option<Self> {
        Self::launch_with_filter(code, runtime_error, "off")
    }

    fn launch_with_filter(code: &[u8], runtime_error: bool, filter: &str) -> Option<Self> {
        if !Path::new("/dev/kvm").exists() {
            eprintln!("SKIP S1 crash diagnostics: /dev/kvm is absent");
            return None;
        }
        let directory = tempfile::tempdir().unwrap();
        // A reset-vector far jump enters our own 16-bit firmware at F000:0000.
        // No downloaded guest, host device, or kernel asset is needed.
        let mut rom = vec![0xf4; 0x10000];
        rom[..code.len()].copy_from_slice(code);
        rom[0xfff0..0xfff5].copy_from_slice(&[0xea, 0, 0, 0, 0xf0]);
        let firmware = directory.path().join("firmware.bin");
        std::fs::write(&firmware, rom).unwrap();
        let machine = directory.path().join("machine.json");
        let control = directory.path().join("control.sock");
        std::fs::write(
            &machine,
            serde_json::to_vec(&json!({
                "name":"diagnostics-test", "machine":"q35", "arch":"x86_64", "accel":"kvm",
                "vcpus":if runtime_error { 2 } else { 1 }, "memory_mib":16, "exit_on_reset":true,
                "boot":{"kind":"firmware", "firmware":"seabios", "code":firmware},
                "disks":[], "nics":[], "ports":[], "shares":[],
                "serial_log":directory.path().join("serial.log"),
            }))
            .unwrap(),
        )
        .unwrap();
        let binary = if runtime_error {
            vmlab_hv_tests::hv_binary_with(&["testing"])
        } else {
            PathBuf::from(env!("CARGO_BIN_EXE_vmlab-hv"))
        };
        // Each VM has its own cgroup cap as well as the harness's host-wide slot.
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
            .arg(binary)
            .arg("--machine")
            .arg(machine)
            .arg("--control")
            .arg(&control)
            .arg("--paused")
            .env("VMLAB_HV_LOG", filter)
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                File::create(directory.path().join("stderr.log")).unwrap(),
            ));
        if runtime_error {
            command.env("VMLAB_HV_TEST_RUNTIME_ERROR", "1");
        }
        let mut child = ManagedChild::spawn(&mut command).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let client = loop {
            if let Ok(client) = Client::connect(&control) {
                break client;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "startup failed: {}",
                std::fs::read_to_string(directory.path().join("stderr.log")).unwrap()
            );
            assert!(Instant::now() < deadline, "control startup timeout");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(client.capabilities().contains(&Capability::Diagnostics));
        Some(Self {
            directory,
            child,
            client,
        })
    }

    fn finish(&mut self, expected: ProcessExitReason) {
        let mut found = false;
        while let Some(event) = self
            .client
            .wait_event_timeout(Duration::from_secs(30))
            .unwrap()
        {
            if let Event::Exiting { reason, code, .. } = event {
                assert_eq!(reason, expected);
                assert_eq!(code, expected.exit_code());
                assert!(!found, "duplicate final event");
                found = true;
            }
        }
        assert!(found, "no final exit reason before control EOF or deadline");
        assert_eq!(
            self.child.wait().unwrap().code(),
            Some(expected.exit_code() as i32)
        );
    }

    fn dump(&mut self) -> Value {
        assert!(matches!(
            self.client.call(Ctrl::DumpDiagnostics).unwrap(),
            Outcome::Ok { .. }
        ));
        let event = self.client.take_events().into_iter().find_map(|e| match e {
            Event::DiagnosticsDumped { path } => Some(path),
            _ => None,
        });
        let path = match event {
            Some(path) => path,
            None => match self.client.wait_event().unwrap().unwrap() {
                Event::DiagnosticsDumped { path } => path,
                other => panic!("unexpected event: {other:?}"),
            },
        };
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    fn wait_for_exit_mode(&mut self, exact_rip: bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        match self.client.call(Ctrl::Status).unwrap() {
            Outcome::Ok {
                status: Some(RunState::Paused),
            } => {
                assert!(matches!(
                    self.client.call(Ctrl::Resume).unwrap(),
                    Outcome::Ok { .. }
                ));
            }
            Outcome::Ok {
                status: Some(RunState::Running),
            } => {}
            other => panic!("unexpected machine state: {other:?}"),
        }
        loop {
            std::thread::sleep(Duration::from_millis(10));
            self.client.call(Ctrl::Pause).unwrap();
            let report = self.dump();
            let cpu = &report["vm"]["vcpus"][0];
            let exits = cpu["exits"].as_array().unwrap();
            let io: Vec<_> = exits
                .iter()
                .filter(|exit| exit["reason"] == 2 && exit["address"] == 0x80)
                .collect();
            if exits.len() == 256
                && io.len() >= 254
                && io.iter().all(|exit| exit["rip"].is_u64() == exact_rip)
            {
                // Crash-time registers remain readable in either recorder mode.
                assert!(cpu["registers"]["regs"]["rip"].is_u64());
                assert!(cpu["registers"]["sregs"]["cr0"].is_u64());
                assert_eq!(
                    cpu["registers"]["sync_valid_regs"].as_u64().unwrap() & 1,
                    u64::from(exact_rip),
                    "KVM_SYNC_X86_REGS must follow the requested recording mode",
                );
                return;
            }
            assert!(
                Instant::now() < deadline,
                "recorder did not switch: {report}"
            );
            self.client.call(Ctrl::Resume).unwrap();
        }
    }
}

#[test]
fn recorder_rip_is_opt_in_at_startup_and_switches_both_directions() {
    const DEBUG: &str = "info,vmlab_hv_vmm::diagnostics::flight=debug";
    // CLI; MOV DX,80h; OUT DX,AL; JMP back to OUT.
    let code = [0xfa, 0xba, 0x80, 0, 0xee, 0xeb, 0xfd];
    for startup in ["info", DEBUG] {
        let Some(mut machine) = Machine::launch_with_filter(&code, false, startup) else {
            return;
        };
        let report = machine.dump();
        assert_eq!(report["vm"]["exit_rip"]["requested"], startup == DEBUG);
        if report["vm"]["exit_rip"]["supported"] == false {
            eprintln!("SKIP exact RIP assertions: KVM sync registers are unavailable");
            machine.wait_for_exit_mode(false);
        } else {
            machine.wait_for_exit_mode(startup == DEBUG);
            for (filter, expected) in [
                (DEBUG, true),
                ("info", false),
                (DEBUG, true),
                ("off", false),
            ] {
                // Change the filter while KVM_RUN is active, rather than only while paused.
                assert!(matches!(
                    machine.client.call(Ctrl::Resume).unwrap(),
                    Outcome::Ok { .. }
                ));
                assert!(matches!(
                    machine
                        .client
                        .call(Ctrl::SetLogFilter {
                            filter: filter.into()
                        })
                        .unwrap(),
                    Outcome::Ok { .. }
                ));
                machine.wait_for_exit_mode(expected);
                assert!(matches!(
                    machine
                        .client
                        .call(Ctrl::SetLogFilter {
                            filter: "foo=invalid".into()
                        })
                        .unwrap(),
                    Outcome::Error { .. }
                ));
                assert_eq!(machine.dump()["vm"]["exit_rip"]["requested"], expected);
            }
        }
        machine.client.call(Ctrl::Quit).unwrap();
        machine.finish(ProcessExitReason::Quit);
    }
}

#[test]
fn guest_poweroff_is_zero_and_final_event_precedes_eof() {
    // Configure Q35's LPC PMBASE and ACPI decode through CF8/CFC, then enter S5.
    // Q35's DSDT encodes S5 with SLP_TYP=0.
    let code = [
        0xfa, 0xba, 0xf8, 0x0c, 0x66, 0xb8, 0x40, 0xf8, 0, 0x80, 0x66, 0xef, 0xba, 0xfc, 0x0c,
        0x66, 0xb8, 1, 6, 0, 0, 0x66, 0xef, 0xba, 0xf8, 0x0c, 0x66, 0xb8, 0x44, 0xf8, 0, 0x80,
        0x66, 0xef, 0xba, 0xfc, 0x0c, 0xb0, 0x80, 0xee, 0xba, 4, 6, 0xb8, 0, 0x20, 0xef, 0xf4,
    ];
    let Some(mut machine) = Machine::launch(&code, false) else {
        return;
    };
    machine.client.call(Ctrl::Resume).unwrap();
    machine.finish(ProcessExitReason::GuestPoweroff);
}

#[test]
fn guest_reset_has_its_own_final_reason_and_exits_zero() {
    let code = [0xba, 0xf9, 0x0c, 0xb0, 2, 0xee, 0xb0, 6, 0xee, 0xf4];
    let Some(mut machine) = Machine::launch(&code, false) else {
        return;
    };
    machine.client.call(Ctrl::Resume).unwrap();
    machine.finish(ProcessExitReason::GuestReset);
}

#[test]
fn host_signal_has_its_own_final_reason_and_exit_code() {
    let Some(mut machine) = Machine::launch(&[0xfa, 0xf4, 0xeb, 0xfd], false) else {
        return;
    };
    let report = machine.dump();
    let pid = i32::try_from(report["pid"].as_u64().unwrap()).unwrap();
    // SAFETY: the PID comes from this fixture's authenticated local control socket.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    assert_eq!(
        machine
            .client
            .wait_event_timeout(Duration::from_secs(30))
            .unwrap(),
        Some(Event::Shutdown {
            reason: vmlab_hv_proto::control::ShutdownReason::HostRequest
        }),
        "host signals must preserve the existing shutdown event before the final event",
    );
    machine.finish(ProcessExitReason::HostSignal {
        signal: libc::SIGTERM,
    });
}

#[test]
fn runtime_filter_dump_and_quit_round_trip() {
    let Some(mut machine) = Machine::launch(&[0xfa, 0xf4, 0xeb, 0xfd], false) else {
        return;
    };
    assert!(matches!(
        machine
            .client
            .call(Ctrl::SetLogFilter {
                filter: "info,vmlab_hv_devices=debug".into()
            })
            .unwrap(),
        Outcome::Ok { .. }
    ));
    assert!(matches!(
        machine
            .client
            .call(Ctrl::SetLogFilter {
                filter: "foo=invalid".into()
            })
            .unwrap(),
        Outcome::Error { .. }
    ));
    let report = machine.dump();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["machine"]["name"], "diagnostics-test");
    assert!(
        report["vm"]["vcpus"][0]["registers"]["regs"]["rip"].is_u64(),
        "paused vCPU registers: {}",
        report["vm"]["vcpus"]
    );
    machine.client.call(Ctrl::Quit).unwrap();
    machine.finish(ProcessExitReason::Quit);
}

#[test]
fn deliberate_runtime_error_writes_report_and_exits_one() {
    let Some(mut machine) = Machine::launch(&[0xf4], true) else {
        return;
    };
    machine.client.call(Ctrl::Resume).unwrap();
    machine.finish(ProcessExitReason::RuntimeError);
    let stderr = std::fs::read_to_string(machine.directory.path().join("stderr.log")).unwrap();
    let last: Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    let path = last["message"]
        .as_str()
        .unwrap()
        .strip_prefix("crash report: ")
        .unwrap();
    let report: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(report["exit_code"], 1);
    assert_eq!(report["reason"]["kind"], "runtime_error");
    assert!(
        report["error_chain"][0]
            .as_str()
            .unwrap()
            .contains("deliberate runtime error")
    );
    assert!(report["vm"]["vcpus"].is_array());
    assert!(report["logs"].is_array());
    assert_eq!(report["vm"]["exit_rip"]["requested"], false);
    let cpus = report["vm"]["vcpus"].as_array().unwrap();
    assert_eq!(cpus.len(), 2);
    assert!(
        cpus.iter()
            .all(|cpu| cpu["registers"]["regs"]["rip"].is_u64()
                && cpu["registers"]["sregs"]["cr0"].is_u64())
    );
}

#[test]
fn startup_error_has_an_explicit_reason_without_echoing_secret_fields() {
    let directory = tempfile::tempdir().unwrap();
    let fake = (0..64)
        .map(|n| char::from(b'A' + n % 26))
        .collect::<String>();
    let machine = directory.path().join("invalid.json");
    std::fs::write(
        &machine,
        serde_json::to_vec(&json!({"name":"bad", "applesmc":{"osk":fake}})).unwrap(),
    )
    .unwrap();
    // Validation fails before /dev/kvm is opened, so this never boots a guest.
    let result = Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
        .arg("--machine")
        .arg(machine)
        .arg("--control")
        .arg(directory.path().join("control.sock"))
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2));
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(!stderr.contains(&fake));
    assert!(stderr.contains("startup_error"));
    assert!(
        std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .all(|e| e.path().extension().is_none_or(|e| e != "pending"))
    );
}
