use super::*;

fn powershell(agent: &mut AgentClient, script: &str) -> vmlab_hv_tests::agent::ExecOutput {
    agent
        .exec(
            &[
                "powershell.exe",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ],
            Duration::from_secs(360),
        )
        .unwrap()
}

fn reboot_guest(control: &mut Client, mut agent: AgentClient, path: &Path) -> AgentClient {
    control.call(Ctrl::Status).unwrap();
    control.take_events();
    agent.shutdown("reboot", Duration::from_secs(20)).unwrap();
    drop(agent);
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let event = control
            .wait_event_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
            .expect("Windows reboot timed out");
        if event == vmlab_hv_proto::control::Event::Reset {
            break;
        }
    }
    AgentClient::connect(&path.join("agent.sock"), Duration::from_secs(240)).unwrap()
}

#[test]
fn windows_win3d_venus_context_cleanup() {
    let Some(node) = std::env::var_os("VMLAB_HV_TEST_VENUS_RENDER_NODE") else {
        eprintln!("SKIP Windows Venus S1: set VMLAB_HV_TEST_VENUS_RENDER_NODE");
        return;
    };
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let template = std::env::var_os("VMLAB_HV_TEST_WIN3D_DISK").map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share/vmlab/templates/x86_64/windows-11/26100.1742.6/variants/win3d/disk.qcow2"));
    let verify = std::env::var_os("VMLAB_HV_TEST_WIN3D_VERIFY")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("dev/vmlab-templates/windows-11/scripts/win3d-verify.ps1"));
    let firmware = home.join(".local/share/vmlab/guest/firmware/x86_64");
    if !template.is_file()
        || !verify.is_file()
        || !Path::new("/dev/kvm").exists()
        || !firmware.join("OVMF_CODE_4M.fd").is_file()
        || !firmware.join("OVMF_VARS_4M.fd").is_file()
        || !Command::new("swtpm")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    {
        eprintln!(
            "SKIP Windows Venus S1: win3d template, verify script, KVM, OVMF or swtpm missing"
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let binary = path.join("vmlab-hv");
    std::fs::copy(env!("CARGO_BIN_EXE_vmlab-hv"), &binary).unwrap();
    let disk = path.join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &disk).unwrap();
    let vars = path.join("vars.fd");
    std::fs::copy(firmware.join("OVMF_VARS_4M.fd"), &vars).unwrap();
    let tpm_socket = path.join("tpm.sock");
    let tpm_state = path.join("tpm");
    std::fs::create_dir(&tpm_state).unwrap();
    let mut tpm = ManagedChild::helper(
        Command::new("swtpm")
            .args(["socket", "--tpm2", "--tpmstate"])
            .arg(format!("dir={}", tpm_state.display()))
            .arg("--ctrl")
            .arg(format!("type=unixio,path={}", tpm_socket.display()))
            .stdout(Stdio::null()),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !tpm_socket.exists() {
        assert!(
            tpm.try_wait().unwrap().is_none(),
            "swtpm exited before binding"
        );
        assert!(Instant::now() < deadline, "swtpm did not bind");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut machine = json!({
        "name": "venus-win3d-s1", "machine": "q35", "arch": "x86_64", "accel": "kvm",
        "vcpus": 4, "memory_mib": 8192,
        "hyperv": ["relaxed", "vapic", "spinlocks", "vpindex", "runtime", "time", "synic", "stimer", "frequencies", "reset", "tlbflush", "ipi"],
        "tpm": {"socket": tpm_socket},
        "boot": {"kind": "firmware", "firmware": "ovmf", "code": firmware.join("OVMF_CODE_4M.fd"), "vars": vars, "secure_boot": false},
        "console_pci": {"bus": 0, "device": 1, "function": 0},
        "display": {"device": "virtio-vga", "pci": {"bus": 0, "device": 3, "function": 0}, "gpu": "venus", "render_node": PathBuf::from(node)},
        "disks": [{"id": "root", "path": disk, "format": "qcow2", "read_only": false, "vmstate": true, "pci": {"bus": 0, "device": 2, "function": 0}}],
        "nics": [], "serial_log": path.join("serial.log"),
        "ports": [{"name": "vmlab.agent.0", "socket": path.join("agent.sock")}]
    });
    if vmlab_hv_vmm::hyperv::host_apic_virtualization()
        && std::env::var_os("VMLAB_HV_TEST_NO_AVIC").is_none()
    {
        machine["hyperv"]
            .as_array_mut()
            .unwrap()
            .push("avic".into());
    }
    std::fs::write(
        path.join("machine.json"),
        serde_json::to_vec(&machine).unwrap(),
    )
    .unwrap();
    let log = File::create(path.join("hv.log")).unwrap();
    let mut child = ManagedChild::spawn(
        Command::new(binary)
            .arg("--machine")
            .arg(path.join("machine.json"))
            .arg("--control")
            .arg(path.join("control.sock"))
            .env(
                "VMLAB_HV_LOG",
                "info,vmlab_hv_devices::virtio::gpu::renderer=debug",
            )
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log)),
    )
    .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut control = loop {
            if let Ok(control) = Client::connect(path.join("control.sock")) {
                break control;
            }
            assert!(
                Instant::now() < deadline && child.try_wait().unwrap().is_none(),
                "Windows Venus VM did not start"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut agent =
            AgentClient::connect(&path.join("agent.sock"), Duration::from_secs(240)).unwrap();
        // The agent starts before specialize and user OOBE finish. Rebooting
        // sooner can interrupt setup and discard the verifier's autologon.
        let deadline = Instant::now() + Duration::from_secs(1500);
        let mut last_state = Vec::new();
        loop {
            let ready = agent.exec(
                &["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                r"
    $s = Get-ItemProperty HKLM:\SYSTEM\Setup
    $w = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    $l = Select-String -Path C:\Windows\Panther\UnattendGC\setupact.log -Pattern 'fEnableAutologon = 1|Clearing Auto-logon values' -ErrorAction SilentlyContinue | Select-Object -Last 1
    $u = Get-LocalUser -Name vmlab -ErrorAction SilentlyContinue
    @{setup=$s.SystemSetupInProgress; oobe=$s.OOBEInProgress; systemAutologon=$w.SystemAutoLogon; userEnabled=$u.Enabled; lastOobeLog=$l.Line} | ConvertTo-Json -Compress
    if (-not ($s.SystemSetupInProgress -or $s.OOBEInProgress) -and -not $w.SystemAutoLogon -and (-not $l -or $l.Line -match 'Clearing') -and $u.Enabled) { exit 0 }
    exit 1
"],
                Duration::from_secs(15),
            );
            let ready = match ready {
                Ok(ready) => ready,
                Err(error) => {
                    eprintln!("Windows setup agent disconnected: {error}");
                    drop(agent);
                    agent = loop {
                        match AgentClient::connect(
                            &path.join("agent.sock"),
                            Duration::from_secs(30),
                        ) {
                            Ok(agent) => break agent,
                            Err(error) => {
                                assert!(
                                    Instant::now() < deadline,
                                    "setup agent did not return: {error}"
                                );
                            }
                        }
                    };
                    continue;
                }
            };
            if ready.exit_code == 0 {
                eprintln!("Windows first-boot setup complete");
                break;
            }
            if last_state != ready.stdout {
                eprintln!(
                    "Windows first-boot: {}",
                    String::from_utf8_lossy(&ready.stdout)
                );
                last_state.clone_from(&ready.stdout);
            }
            assert!(Instant::now() < deadline, "setup did not finish: {ready:?}");
            std::thread::sleep(Duration::from_secs(2));
        }
        let pid = child.id();
        agent = reboot_guest(&mut control, agent, path);
        assert_eq!(child.id(), pid);
        assert!(child.try_wait().unwrap().is_none());
        eprintln!("Windows in-place reset complete; hypervisor PID unchanged");
        let script = std::fs::read(verify).unwrap();
        let guest_script = "C:\\Windows\\Temp\\vmlab-contexts-verify.ps1";
        let output = powershell(
            &mut agent,
            &format!("[IO.File]::WriteAllBytes('{guest_script}', [byte[]]@())"),
        );
        assert_eq!(output.exit_code, 0, "{output:?}");
        // Bound each command below Windows' command-line length limit.
        for bytes in script.chunks(2000) {
            let data = bytes
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let output = powershell(
                &mut agent,
                &format!(
                    "$f=[IO.File]::Open('{guest_script}',[IO.FileMode]::Append); try {{ $b=[byte[]]@({data}); $f.Write($b,0,$b.Length) }} finally {{ $f.Dispose() }}"
                ),
            );
            assert_eq!(output.exit_code, 0, "{output:?}");
        }
        let run_verify = |agent: &mut AgentClient| {
            agent
                .exec(
                    &[
                        "powershell.exe",
                        "-NoProfile",
                        "-NonInteractive",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-File",
                        guest_script,
                        "-Phase",
                        "Vulkan",
                    ],
                    Duration::from_secs(360),
                )
                .unwrap()
        };
        let mut output = run_verify(&mut agent);
        if output.exit_code == 3 {
            agent = reboot_guest(&mut control, agent, path);
            let ready = powershell(
                &mut agent,
                "for ($n=0; $n -lt 90; $n++) { if (Get-Process explorer -ErrorAction SilentlyContinue | Where-Object SessionId -ne 0) { exit 0 }; Start-Sleep -Seconds 2 }; exit 1",
            );
            assert_eq!(ready.exit_code, 0, "console user did not log on: {ready:?}");
            output = run_verify(&mut agent);
        }
        eprintln!(
            "Windows win3d verification:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.exit_code, 0);
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains("ok: vulkaninfo x64 lists") && text.contains("ok: vulkaninfo x86 lists"),
            "{text}"
        );
        let log = std::fs::read_to_string(path.join("hv.log")).unwrap();
        assert!(
            !log.contains("invalid context id") && !log.contains("GPU command failed:"),
            "Windows GPU command failure:\n{log}"
        );
        control.call(Ctrl::Quit).unwrap();
        assert!(child.wait().unwrap().success());
    }));
    if result.is_err() {
        eprintln!(
            "Windows Venus hypervisor:\n{}\nserial:\n{}",
            std::fs::read_to_string(path.join("hv.log")).unwrap_or_default(),
            std::fs::read_to_string(path.join("serial.log")).unwrap_or_default()
        );
    }
    result.unwrap();
}
