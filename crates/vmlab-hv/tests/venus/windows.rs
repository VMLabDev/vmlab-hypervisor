use super::*;

const WINDOWS_CRASH_QUERY: &str = r#"
$ErrorActionPreference = 'Stop'
$events = @()
try {
    $events = @(Get-WinEvent -FilterHashtable @{LogName='System';Id=1001} -ErrorAction Stop |
        Where-Object { $_.ProviderName -in @('BugCheck','Microsoft-Windows-WER-SystemErrorReporting') } |
        ForEach-Object { [ordered]@{record_id=$_.RecordId; event_id=$_.Id; provider=$_.ProviderName; time_utc=$_.TimeCreated.ToUniversalTime().ToString('o'); message=$_.Message} })
} catch {
    if ($_.FullyQualifiedErrorId -notlike 'NoMatchingEventsFound*') { throw }
}
$application = @()
try {
    $application = @(Get-WinEvent -FilterHashtable @{LogName='Application';Id=@(1000,1001)} -ErrorAction Stop |
        ForEach-Object { [ordered]@{record_id=$_.RecordId; event_id=$_.Id; provider=$_.ProviderName; time_utc=$_.TimeCreated.ToUniversalTime().ToString('o'); message=$_.Message} })
} catch {
    if ($_.FullyQualifiedErrorId -notlike 'NoMatchingEventsFound*') { throw }
}
$desktop = @(Get-CimInstance Win32_Process -Filter "Name='dwm.exe' OR Name='explorer.exe'" -ErrorAction Stop |
    ForEach-Object { [ordered]@{name=$_.Name; process_id=$_.ProcessId; session_id=$_.SessionId; start_utc=$_.CreationDate.ToUniversalTime().ToString('o')} })
$reports = @()
foreach ($directory in @('ReportQueue','ReportArchive')) {
    $root = Join-Path $env:ProgramData ('Microsoft\Windows\WER\' + $directory)
    if (-not (Test-Path -LiteralPath $root)) { continue }
    foreach ($file in @(Get-ChildItem -LiteralPath $root -Filter Report.wer -Recurse -File -ErrorAction Stop)) {
        # WER stores literal Windows paths; the string-data parser interprets escapes.
        $data = ConvertFrom-StringData -StringData ((Get-Content -LiteralPath $file.FullName -Raw -ErrorAction Stop).Replace('\','\\'))
        if (-not $data.ContainsKey('EventType')) { throw ('WER report has no EventType: ' + $file.FullName) }
        $reports += [ordered]@{path=$file.FullName; event_type=$data['EventType']; code=$data['Sig[0].Value']; details=$data}
    }
}
$health = [ordered]@{system_events=$events; application_events=$application; desktop_processes=$desktop; wer_reports=$reports}
"#;

fn windows_crash_script(tail: &str) -> String {
    format!("{WINDOWS_CRASH_QUERY}\n{tail}")
}

fn windows_crash_report(stdout: &[u8]) -> Result<serde_json::Value, String> {
    let stdout = stdout.strip_prefix(b"\xef\xbb\xbf").unwrap_or(stdout);
    let report: serde_json::Value = serde_json::from_slice(stdout)
        .map_err(|error| format!("invalid crash report JSON: {error}"))?;
    let events = report["system_events"]
        .as_array()
        .ok_or("crash report is missing system_events array")?;
    let reports = report["wer_reports"]
        .as_array()
        .ok_or("crash report is missing wer_reports array")?;
    let mut failures = Vec::new();
    for event in events {
        let event_id = event["event_id"]
            .as_u64()
            .ok_or("System event has no numeric event_id")?;
        let provider = event["provider"]
            .as_str()
            .ok_or("System event has no provider")?;
        let record_id = event["record_id"]
            .as_u64()
            .ok_or("System event has no numeric record_id")?;
        let time = event["time_utc"]
            .as_str()
            .ok_or("System event has no time_utc")?;
        let message = event["message"]
            .as_str()
            .ok_or("System event has no message")?;
        if event_id == 1001
            && matches!(
                provider,
                "BugCheck" | "Microsoft-Windows-WER-SystemErrorReporting"
            )
        {
            failures.push(format!(
                "System event {record_id} at {time}, provider {provider}: {message}"
            ));
        }
    }
    for wer in reports {
        let path = wer["path"].as_str().ok_or("WER report has no path")?;
        let event_type = wer["event_type"]
            .as_str()
            .ok_or("WER report has no event_type")?;
        if event_type.eq_ignore_ascii_case("BlueScreen") {
            failures.push(format!("WER BlueScreen at {path}, code {}", wer["code"]));
        }
        if event_type.eq_ignore_ascii_case("APPCRASH")
            && let Some(details) = wer["details"].as_object()
        {
            for (key, name) in details {
                if name != "Fault Module Name" {
                    continue;
                }
                let Some(prefix) = key.strip_suffix(".Name") else {
                    continue;
                };
                let module = details
                    .get(&format!("{prefix}.Value"))
                    .and_then(serde_json::Value::as_str)
                    .ok_or("APPCRASH fault module has no string Value")?;
                let normalized = module.to_ascii_lowercase();
                if normalized.starts_with("viogpu") && normalized.ends_with(".dll") {
                    failures.push(format!(
                        "WER owned graphics driver APPCRASH at {path}, module {module}: {}",
                        wer["details"]
                    ));
                }
            }
        }
    }
    if failures.is_empty() {
        Ok(report)
    } else {
        Err(format!(
            "Windows crash history is not clean; existing history is also fatal because setup may crash before the agent first answers:\n{}",
            failures.join("\n")
        ))
    }
}

fn save_windows_health(path: &Path, stage: &str, stdout: &[u8]) -> std::io::Result<PathBuf> {
    let stage = match stage {
        "setup-poll" => "setup-poll",
        "first-boot setup" => "first-boot",
        "guest reboot" => "reboot",
        "Vulkan verification" => "vulkan",
        "before-tdr" => "before-tdr-render",
        "disconnected forced TDR" => "disconnected-tdr",
        "forced TDR" => "forced-tdr",
        "post-TDR Vulkan verification" => "post-tdr-vulkan",
        "after-tdr" => "after-tdr-render",
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unknown Windows health stage",
            ));
        }
    };
    for sequence in 0..u32::MAX {
        let target = path.join(format!("windows-health-{stage}-{sequence:03}.json"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(mut file) => {
                std::io::Write::write_all(&mut file, stdout)?;
                return Ok(target);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "Windows health snapshot sequence exhausted",
    ))
}

fn assert_windows_crash_free(agent: &mut AgentClient, stage: &str, path: &Path) {
    let output = powershell(
        agent,
        &windows_crash_script(
            r"$health['graphics_events'] = @(Get-WinEvent -LogName System -MaxEvents 128 -ErrorAction Stop | Where-Object { $_.ProviderName -match 'Display|Dxg|Kernel-Power' } | ForEach-Object { [ordered]@{record_id=$_.RecordId; provider=$_.ProviderName; time_utc=$_.TimeCreated.ToUniversalTime().ToString('o'); message=$_.Message} }); $health | ConvertTo-Json -Depth 6 -Compress",
        ),
    );
    save_windows_health(path, stage, &output.stdout).unwrap();
    assert_eq!(
        output.exit_code, 0,
        "Windows crash query failed after {stage}: {output:?}"
    );
    let report = windows_crash_report(&output.stdout)
        .unwrap_or_else(|error| panic!("Windows crash gate failed after {stage}: {error}"));
    eprintln!("Windows crash gate after {stage}: {report}");
}

fn write_guest_file(agent: &mut AgentClient, guest_path: &str, bytes: &[u8]) {
    let output = powershell(
        agent,
        &format!("[IO.File]::WriteAllBytes('{guest_path}', [byte[]]@())"),
    );
    assert_eq!(output.exit_code, 0, "{output:?}");
    // Bound each command below Windows' command-line length limit.
    for bytes in bytes.chunks(2000) {
        let data = bytes
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let output = powershell(
            agent,
            &format!(
                "$f=[IO.File]::Open('{guest_path}',[IO.FileMode]::Append); try {{ $b=[byte[]]@({data}); $f.Write($b,0,$b.Length) }} finally {{ $f.Dispose() }}"
            ),
        );
        assert_eq!(output.exit_code, 0, "{output:?}");
    }
}

fn forced_tdr_report(stdout: &[u8]) -> Result<serde_json::Value, String> {
    let report: serde_json::Value = serde_json::from_slice(stdout)
        .map_err(|error| format!("invalid forced-TDR JSON: {error}"))?;
    if report["schema"] != 1
        || report["probe"] != "tdr"
        || report["result"] != "request_accepted"
        || report["request_completed"] != true
        || report["escape_status"] != "0x00000000"
        || report["last_reset_status"] != "0x00000000"
        || report["last_restart_status"] != "0x00000000"
        || report["recovery_state"] != 0
        || report["test_mode_restore_status"] != 0
        || report["adapter"]["vendor_id"] != "0x1af4"
        || report["adapter"]["device_id"] != "0x1050"
    {
        return Err(format!(
            "forced-TDR request did not complete successfully: {report}"
        ));
    }
    let counter = |name: &str| {
        report[name]
            .as_u64()
            .ok_or_else(|| format!("forced-TDR telemetry is missing {name}"))
    };
    if report["recovery_observed"] != true
        || counter("reset_after")? <= counter("reset_before")?
        || counter("restart_after")? <= counter("restart_before")?
    {
        return Err(format!(
            "forced TDR did not prove reset and restart recovery: {report}"
        ));
    }
    Ok(report)
}

fn capture_tdr_diagnostics(agent: &mut AgentClient, stage: &str, path: &Path) {
    for mode in ["status", "trace"] {
        let output = agent.exec(
            &[
                "C:\\Windows\\Temp\\vmlab-force-tdr.exe",
                &format!("--{mode}"),
            ],
            Duration::from_secs(30),
        );
        let prefix = path.join(format!("windows-tdr-{stage}-{mode}"));
        match output {
            Ok(output) => {
                std::fs::write(prefix.with_extension("json"), &output.stdout).unwrap();
                std::fs::write(prefix.with_extension("stderr"), &output.stderr).unwrap();
                std::fs::write(
                    prefix.with_extension("exit-code"),
                    output.exit_code.to_string(),
                )
                .unwrap();
                eprintln!(
                    "Windows TDR {mode} at {stage}, exit {}: {}",
                    output.exit_code,
                    String::from_utf8_lossy(&output.stdout)
                );
            }
            Err(error) => {
                std::fs::write(prefix.with_extension("error"), &error).unwrap();
                eprintln!("Windows TDR {mode} capture failed at {stage}: {error}");
            }
        }
    }
}

fn export_windows_artifacts(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::create_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let diagnostic = (name.starts_with("windows-health-") && name.ends_with(".json"))
            || ((name.starts_with("windows-tdr-")
                || name.starts_with("windows-render-")
                || name.starts_with("windows-forced-tdr."))
                && matches!(
                    entry
                        .path()
                        .extension()
                        .and_then(|extension| extension.to_str()),
                    Some("json" | "stderr" | "exit-code" | "error")
                ));
        if !diagnostic && !matches!(name, "hv.log" | "serial.log" | "machine.json") {
            continue;
        }
        let mut input = File::open(entry.path())?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination.join(name))?;
        std::io::copy(&mut input, &mut output)?;
    }
    Ok(())
}

fn hardware_render_probe_report(
    stdout: &[u8],
    probe: &str,
    api: &str,
) -> Result<serde_json::Value, String> {
    let stdout = stdout.strip_prefix(b"\xef\xbb\xbf").unwrap_or(stdout);
    let report: serde_json::Value = serde_json::from_slice(stdout)
        .map_err(|error| format!("invalid {api} rendering JSON: {error}"))?;
    if report["schema"] != 1
        || report["probe"] != probe
        || report["api"] != api
        || report["result"] != "pass"
        || report["exit_code"] != 0
        || report["adapter"]["software"] != false
        || !report["session_id"].as_u64().is_some_and(|id| id > 0)
        || !report["frames_presented"]
            .as_u64()
            .is_some_and(|frames| frames > 1)
        || report["window_closed_early"] != false
        || report["readback"]["ok"] != true
        || !report["errors"].as_array().is_some_and(Vec::is_empty)
    {
        return Err(format!("hardware {api} rendering failed: {report}"));
    }
    let tolerance = report["readback"]["tolerance"]
        .as_u64()
        .filter(|value| *value <= 2)
        .ok_or("rendering readback tolerance is missing or exceeds two")?;
    let pixels = report["readback"]["pixels"]
        .as_array()
        .ok_or("rendering readback has no pixels")?;
    for (name, expected) in [
        ("red_top_left", [255_u64, 0, 0]),
        ("blue_top_right", [0, 0, 255]),
        ("green_bottom_left", [0, 255, 0]),
    ] {
        let matches: Vec<_> = pixels
            .iter()
            .filter(|pixel| pixel["name"] == name)
            .collect();
        if matches.len() != 1 {
            return Err(format!(
                "rendering readback requires exactly one {name} marker"
            ));
        }
        let pixel = matches[0];
        let actual = pixel["actual"]
            .as_array()
            .filter(|values| values.len() == 3)
            .ok_or_else(|| format!("rendering marker {name} has no RGB readback"))?;
        if pixel["match"] != true || pixel["expected"] != json!(expected) {
            return Err(format!(
                "rendering marker {name} has incorrect expectations: {pixel}"
            ));
        }
        for (actual, expected) in actual.iter().zip(expected) {
            if !actual
                .as_u64()
                .is_some_and(|value| value <= 255 && value.abs_diff(expected) <= tolerance)
            {
                return Err(format!(
                    "rendering marker {name} has incorrect pixels: {pixel}"
                ));
            }
        }
    }
    Ok(report)
}

fn render_probe_report(stdout: &[u8]) -> Result<serde_json::Value, String> {
    let report = hardware_render_probe_report(stdout, "vulkan", "Vulkan")?;
    if !report["adapter"]["name"]
        .as_str()
        .is_some_and(|name| name.contains("Venus"))
        || report["device"]["driver_name"] != "venus"
    {
        return Err(format!("hardware Venus rendering failed: {report}"));
    }
    Ok(report)
}

fn d3d11_probe_report(stdout: &[u8], arch: &str) -> Result<serde_json::Value, String> {
    let report = hardware_render_probe_report(stdout, "d3d11", "Direct3D 11")?;
    if report["arch"] != arch
        || report["adapter"]["vendor_id"] != "0x1af4"
        || report["adapter"]["device_id"] != "0x1050"
        || report["adapter"]["dxgi_flag_software"] != false
        || !report["adapter"]["name"]
            .as_str()
            .is_some_and(|name| !name.is_empty())
        || report["device"]["driver_type"] != "HARDWARE"
        || report["args"]["warp"] != false
    {
        return Err(format!(
            "hardware virtio D3D11 {arch} rendering failed: {report}"
        ));
    }
    Ok(report)
}

#[derive(Clone, Copy)]
enum RenderProbe {
    Vulkan,
    D3d11(&'static str),
}

impl RenderProbe {
    fn guest_path(self) -> String {
        match self {
            Self::Vulkan => "C:\\Windows\\Temp\\vmlab-render-probe.exe".to_owned(),
            Self::D3d11(arch) => format!("C:\\Windows\\Temp\\vmlab-d3d11-{arch}-probe.exe"),
        }
    }
}

fn run_render_probe(agent: &mut AgentClient, stage: &str, path: &Path, probe: RenderProbe) {
    let name = match probe {
        RenderProbe::Vulkan => stage.to_owned(),
        RenderProbe::D3d11(arch) => format!("d3d11-{arch}-{stage}"),
    };
    let guest_probe = probe.guest_path();
    let output = agent
        .exec(
            &[
                "powershell.exe",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                "C:\\Windows\\Temp\\vmlab-render-runner.ps1",
                "-Path",
                &guest_probe,
                "-Seconds",
                "10",
                "-OutDir",
                "C:\\Windows\\Temp\\vmlab-render-results",
                "-Name",
                &name,
            ],
            Duration::from_secs(360),
        )
        .unwrap();
    std::fs::write(
        path.join(format!("windows-render-{name}.run.json")),
        &output.stdout,
    )
    .unwrap();
    std::fs::write(
        path.join(format!("windows-render-{name}.stderr")),
        &output.stderr,
    )
    .unwrap();
    assert_eq!(
        output.exit_code, 0,
        "interactive rendering failed at {name}: {output:?}"
    );
    let output = powershell(
        agent,
        &format!(
            "$ErrorActionPreference='Stop'; Get-Content -LiteralPath 'C:\\Windows\\Temp\\vmlab-render-results\\{name}.json' -Raw"
        ),
    );
    std::fs::write(
        path.join(format!("windows-render-{name}.json")),
        &output.stdout,
    )
    .unwrap();
    assert_eq!(
        output.exit_code, 0,
        "cannot read rendering result at {name}: {output:?}"
    );
    let report = match probe {
        RenderProbe::Vulkan => render_probe_report(&output.stdout),
        RenderProbe::D3d11(arch) => d3d11_probe_report(&output.stdout, arch),
    }
    .unwrap_or_else(|error| panic!("rendering gate failed at {name}: {error}"));
    eprintln!("Windows rendering at {name}: {report}");
    assert_windows_crash_free(agent, stage, path);
}

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
    let mut agent =
        AgentClient::connect(&path.join("agent.sock"), Duration::from_secs(240)).unwrap();
    assert_windows_crash_free(&mut agent, "guest reboot", path);
    agent
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
    let tdr_probe = std::env::var_os("VMLAB_HV_TEST_WIN3D_TDR_PROBE").map(PathBuf::from);
    let render_probe = std::env::var_os("VMLAB_HV_TEST_WIN3D_RENDER_PROBE").map(PathBuf::from);
    let d3d11_probes = std::env::var_os("VMLAB_HV_TEST_WIN3D_D3D11_PROBES").map(PathBuf::from);
    let probe_runner = std::env::var_os("VMLAB_HV_TEST_WIN3D_PROBE_RUNNER").map(PathBuf::from);
    let artifact_dir = std::env::var_os("VMLAB_HV_TEST_WIN3D_ARTIFACT_DIR").map(PathBuf::from);
    let mut render_probes = Vec::new();
    if let Some(probe) = render_probe {
        render_probes.push((probe, RenderProbe::Vulkan));
    }
    if let Some(directory) = d3d11_probes {
        for arch in ["x64", "x86"] {
            render_probes.push((
                directory.join(arch).join("d3d11_probe.exe"),
                RenderProbe::D3d11(arch),
            ));
        }
    }
    assert_eq!(
        !render_probes.is_empty(),
        probe_runner.is_some(),
        "set VMLAB_HV_TEST_WIN3D_PROBE_RUNNER when enabling Vulkan or D3D11 rendering probes"
    );
    for probe in render_probes
        .iter()
        .map(|(path, _)| path)
        .chain(probe_runner.iter())
    {
        assert!(
            probe.is_file(),
            "rendering probe input does not exist: {}",
            probe.display()
        );
    }
    if let Some(probe) = &tdr_probe {
        assert!(
            probe.is_file(),
            "forced-TDR probe does not exist: {}",
            probe.display()
        );
    }
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
    let hv_binary = std::env::var_os("VMLAB_HV_TEST_HV_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_vmlab-hv")));
    assert!(
        hv_binary.is_file(),
        "hypervisor binary does not exist: {}",
        hv_binary.display()
    );
    std::fs::copy(hv_binary, &binary).unwrap();
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
                &windows_crash_script(r"
    $s = Get-ItemProperty HKLM:\SYSTEM\Setup
    $w = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    $l = Select-String -Path C:\Windows\Panther\UnattendGC\setupact.log -Pattern 'fEnableAutologon = 1|Clearing Auto-logon values' -ErrorAction SilentlyContinue | Select-Object -Last 1
    $u = Get-LocalUser -Name vmlab -ErrorAction SilentlyContinue
    $health['setup'] = @{setup=$s.SystemSetupInProgress; oobe=$s.OOBEInProgress; systemAutologon=$w.SystemAutoLogon; userEnabled=$u.Enabled; lastOobeLog=$l.Line}
    $health | ConvertTo-Json -Depth 6 -Compress
    if (-not ($s.SystemSetupInProgress -or $s.OOBEInProgress) -and -not $w.SystemAutoLogon -and (-not $l -or $l.Line -match 'Clearing') -and $u.Enabled) { exit 0 }
    exit 1
")],
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
            save_windows_health(path, "setup-poll", &ready.stdout).unwrap();
            windows_crash_report(&ready.stdout).unwrap_or_else(|error| {
                panic!("Windows crash gate failed during first-boot setup: {error}; {ready:?}")
            });
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
        assert_windows_crash_free(&mut agent, "first-boot setup", path);
        if let Some(probe) = &tdr_probe {
            write_guest_file(
                &mut agent,
                "C:\\Windows\\Temp\\vmlab-force-tdr.exe",
                &std::fs::read(probe).unwrap(),
            );
            capture_tdr_diagnostics(&mut agent, "first-boot", path);
        }
        let pid = child.id();
        agent = reboot_guest(&mut control, agent, path);
        assert_eq!(child.id(), pid);
        assert!(child.try_wait().unwrap().is_none());
        eprintln!("Windows in-place reset complete; hypervisor PID unchanged");
        let script = std::fs::read(verify).unwrap();
        let guest_script = "C:\\Windows\\Temp\\vmlab-contexts-verify.ps1";
        write_guest_file(&mut agent, guest_script, &script);
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
        assert_windows_crash_free(&mut agent, "Vulkan verification", path);
        if let Some(runner) = &probe_runner {
            write_guest_file(
                &mut agent,
                "C:\\Windows\\Temp\\vmlab-render-runner.ps1",
                &std::fs::read(runner).unwrap(),
            );
            for (host_probe, probe) in &render_probes {
                let guest_probe = probe.guest_path();
                write_guest_file(
                    &mut agent,
                    &guest_probe,
                    &std::fs::read(host_probe).unwrap(),
                );
                let permissions = powershell(
                    &mut agent,
                    &format!(
                        "$ErrorActionPreference='Stop'; & icacls.exe '{guest_probe}' /grant '*S-1-5-32-545:RX' /Q | Out-Null; if ($LASTEXITCODE -ne 0) {{ throw 'Cannot grant interactive users probe execution' }}"
                    ),
                );
                assert_eq!(permissions.exit_code, 0, "{permissions:?}");
                run_render_probe(&mut agent, "before-tdr", path, *probe);
            }
        }
        if tdr_probe.is_some() {
            let guest_probe = "C:\\Windows\\Temp\\vmlab-force-tdr.exe";
            let guest_result = "C:\\Windows\\Temp\\vmlab-force-tdr.json";
            capture_tdr_diagnostics(&mut agent, "before-tdr", path);
            control.call(Ctrl::Status).unwrap();
            control.take_events();
            let forced = agent.exec(
                &[guest_probe, "--force-tdr", "--out", guest_result],
                Duration::from_secs(180),
            );
            let forced = match forced {
                Ok(output) => output,
                Err(error) => {
                    drop(agent);
                    agent =
                        AgentClient::connect(&path.join("agent.sock"), Duration::from_secs(240))
                            .unwrap();
                    capture_tdr_diagnostics(&mut agent, "disconnected-tdr", path);
                    assert_windows_crash_free(&mut agent, "disconnected forced TDR", path);
                    panic!("forced-TDR probe lost its agent connection: {error}");
                }
            };
            std::fs::write(path.join("windows-forced-tdr.json"), &forced.stdout).unwrap();
            std::fs::write(path.join("windows-forced-tdr.stderr"), &forced.stderr).unwrap();
            capture_tdr_diagnostics(&mut agent, "after-tdr", path);
            assert_windows_crash_free(&mut agent, "forced TDR", path);
            assert_eq!(forced.exit_code, 0, "forced-TDR probe failed: {forced:?}");
            let telemetry = forced_tdr_report(&forced.stdout)
                .unwrap_or_else(|error| panic!("forced-TDR recovery check failed: {error}"));
            eprintln!("Windows forced-TDR recovery: {telemetry}");
            control.call(Ctrl::Status).unwrap();
            assert!(
                !control
                    .take_events()
                    .contains(&vmlab_hv_proto::control::Event::Reset),
                "forced TDR rebooted Windows instead of recovering the display adapter"
            );
            assert_eq!(child.id(), pid);
            assert!(child.try_wait().unwrap().is_none());
            let after = run_verify(&mut agent);
            assert_eq!(
                after.exit_code, 0,
                "Vulkan failed after forced TDR: {after:?}"
            );
            let after_text = String::from_utf8_lossy(&after.stdout);
            assert!(
                after_text.contains("ok: vulkaninfo x64 lists")
                    && after_text.contains("ok: vulkaninfo x86 lists"),
                "{after_text}"
            );
            assert_windows_crash_free(&mut agent, "post-TDR Vulkan verification", path);
            for (_, probe) in &render_probes {
                run_render_probe(&mut agent, "after-tdr", path, *probe);
            }
        }
        let log = std::fs::read_to_string(path.join("hv.log")).unwrap();
        assert!(
            !log.contains("invalid context id") && !log.contains("GPU command failed:"),
            "Windows GPU command failure:\n{log}"
        );
        control.call(Ctrl::Quit).unwrap();
        assert!(child.wait().unwrap().success());
        if let Some(destination) = &artifact_dir {
            export_windows_artifacts(path, destination).unwrap_or_else(|error| {
                panic!(
                    "cannot export Windows evidence to {}: {error}",
                    destination.display()
                )
            });
            eprintln!(
                "Windows Venus evidence exported to {}",
                destination.display()
            );
        }
    }));
    if result.is_err() {
        eprintln!(
            "Windows Venus hypervisor:\n{}\nserial:\n{}",
            std::fs::read_to_string(path.join("hv.log")).unwrap_or_default(),
            std::fs::read_to_string(path.join("serial.log")).unwrap_or_default()
        );
        eprintln!(
            "Windows Venus failure artifacts retained at {}",
            dir.keep().display()
        );
    }
    result.unwrap();
}

#[test]
fn windows_bugcheck_gate_rejects_system_events_even_with_old_clock() {
    for provider in ["BugCheck", "Microsoft-Windows-WER-SystemErrorReporting"] {
        let output = json!({
            "system_events": [{"record_id": 2324, "event_id": 1001, "provider": provider,
                "time_utc": "1999-01-01T00:00:00Z", "message": "bugcheck 0x00000116"}],
            "wer_reports": []
        });
        let error = windows_crash_report(&serde_json::to_vec(&output).unwrap()).unwrap_err();
        assert!(
            error.contains("2324") && error.contains("0x00000116"),
            "{error}"
        );
    }
}

#[test]
fn windows_bugcheck_gate_rejects_queued_and_archived_bluescreens() {
    for directory in ["ReportQueue", "ReportArchive"] {
        let output = json!({
            "system_events": [],
            "wer_reports": [{"path": format!("C:\\ProgramData\\Microsoft\\Windows\\WER\\{directory}\\Kernel_116\\Report.wer"),
                "event_type": "BlueScreen", "code": "116"}]
        });
        let error = windows_crash_report(&serde_json::to_vec(&output).unwrap()).unwrap_err();
        assert!(
            error.contains("BlueScreen") && error.contains(directory),
            "{error}"
        );
    }
}

#[test]
fn windows_bugcheck_gate_accepts_live_kernel_reports_without_bugchecks() {
    let output = json!({
        "system_events": [{"record_id": 1, "event_id": 1001, "provider": "OtherProvider",
            "time_utc": "2026-11-02T00:00:00Z", "message": "unrelated event"}],
        "wer_reports": [{"path": "C:\\WER\\Kernel_117\\Report.wer", "event_type": "LiveKernelEvent", "code": "117"},
            {"path":"C:\\WER\\AppCrash_dwm\\Report.wer","event_type":"APPCRASH","code":"dwm.exe",
                "details":{"Sig[3].Name":"Fault Module Name","Sig[3].Value":"dwmcore.dll","Sig[6].Value":"c0000005"}}],
        "application_events":[{"record_id": 123,"event_id":1000,"provider":"Application Error",
            "time_utc":"2026-11-02T00:00:00Z","message":"Faulting application dwm.exe"}]
    });
    let parsed = windows_crash_report(&serde_json::to_vec(&output).unwrap()).unwrap();
    assert_eq!(
        parsed["wer_reports"][1]["details"]["Sig[3].Value"],
        "dwmcore.dll"
    );
    assert_eq!(parsed["application_events"][0]["record_id"], 123);
    windows_crash_report(br#"{"system_events":[],"wer_reports":[]}"#).unwrap();
}

#[test]
fn windows_bugcheck_gate_rejects_owned_driver_appcrash_signatures() {
    let captured = json!({
        "system_events": [],
        "wer_reports": [{
            "path":"C:\\ProgramData\\Microsoft\\Windows\\WER\\ReportQueue\\AppCrash_Explorer.EXE\\Report.wer",
            "event_type":"APPCRASH", "code":"Explorer.EXE",
            "details": {
                "Sig[0].Name":"Application Name", "Sig[0].Value":"Explorer.EXE",
                "Sig[1].Name":"Application Version", "Sig[1].Value":"10.0.26100.9711",
                "Sig[2].Name":"Application Timestamp", "Sig[2].Value":"bee7e13f",
                "Sig[3].Name":"Fault Module Name", "Sig[3].Value":"viogpu_d3d10.dll",
                "Sig[4].Name":"Fault Module Version", "Sig[4].Value":"0.0.0.0",
                "Sig[5].Name":"Fault Module Timestamp", "Sig[5].Value":"00000000",
                "Sig[6].Name":"Exception Code", "Sig[6].Value":"c0000005",
                "Sig[7].Name":"Exception Offset", "Sig[7].Value":"000000000024b56d"
            }
        }]
    });
    for module in ["viogpu_d3d10.dll", "VIOGPU_D3D10.DLL", "viogpu_d3d12.dll"] {
        let mut report = captured.clone();
        report["wer_reports"][0]["details"]["Sig[3].Value"] = json!(module);
        let error = windows_crash_report(&serde_json::to_vec(&report).unwrap()).unwrap_err();
        assert!(
            error.contains(module)
                && error.contains("c0000005")
                && error.contains("000000000024b56d"),
            "{error}"
        );
    }
    let mut moved = captured.clone();
    let details = moved["wer_reports"][0]["details"].as_object_mut().unwrap();
    let name = details.remove("Sig[3].Name").unwrap();
    let value = details.remove("Sig[3].Value").unwrap();
    details.insert("Sig[11].Name".to_owned(), name);
    details.insert("Sig[11].Value".to_owned(), value);
    assert!(windows_crash_report(&serde_json::to_vec(&moved).unwrap()).is_err());

    for module in ["dwmcore.dll", "kernelbase.dll", "viogpu_helper.exe"] {
        let mut unrelated = captured.clone();
        unrelated["wer_reports"][0]["details"]["Sig[3].Value"] = json!(module);
        windows_crash_report(&serde_json::to_vec(&unrelated).unwrap()).unwrap();
    }
    let mut unrelated_field = captured.clone();
    unrelated_field["wer_reports"][0]["details"]["Sig[3].Name"] = json!("Application Name");
    windows_crash_report(&serde_json::to_vec(&unrelated_field).unwrap()).unwrap();
    let mut appx = captured;
    appx["wer_reports"][0]["event_type"] = json!("AppxDeploymentFailureBlue");
    windows_crash_report(&serde_json::to_vec(&appx).unwrap()).unwrap();
}

#[test]
fn windows_bugcheck_gate_rejects_incomplete_query_output() {
    for output in [
        b"not JSON".as_slice(),
        br#"{}"#,
        br#"{"system_events":[],"wer_reports":null}"#,
        br#"{"system_events":[{}],"wer_reports":[]}"#,
        br#"{"system_events":[],"wer_reports":[{}]}"#,
    ] {
        assert!(windows_crash_report(output).is_err());
    }
}

#[test]
fn windows_bugcheck_gate_requires_forced_reset_and_restart_telemetry() {
    let good = json!({"schema":1,"probe":"tdr","result":"request_accepted",
        "request_completed":true,"escape_status":"0x00000000","test_mode_restore_status":0,
        "last_reset_status":"0x00000000","last_restart_status":"0x00000000","recovery_state":0,
        "adapter":{"vendor_id":"0x1af4","device_id":"0x1050"},"recovery_observed":true,
        "reset_before":2,"reset_after":3,"restart_before":2,"restart_after":3});
    forced_tdr_report(&serde_json::to_vec(&good).unwrap()).unwrap();
    for field in ["recovery_observed", "reset_after", "restart_after"] {
        let mut bad = good.clone();
        bad[field] = if field == "recovery_observed" {
            false.into()
        } else {
            2.into()
        };
        assert!(forced_tdr_report(&serde_json::to_vec(&bad).unwrap()).is_err());
    }
    let mut no_telemetry = good;
    no_telemetry.as_object_mut().unwrap().remove("reset_before");
    assert!(forced_tdr_report(&serde_json::to_vec(&no_telemetry).unwrap()).is_err());
}

#[test]
fn windows_bugcheck_gate_accepts_powershell_utf8_bom() {
    windows_crash_report(b"\xef\xbb\xbf{\"system_events\":[],\"wer_reports\":[]}\r\n").unwrap();
}

#[test]
fn windows_bugcheck_gate_requires_hardware_rendering_and_pixel_readback() {
    let good = json!({
        "schema":1,"probe":"vulkan","api":"Vulkan","arch":"x64","result":"pass","exit_code":0,
        "session_id":1,"adapter":{"name":"Virtio-GPU Venus (NVIDIA GeForce RTX 5090)","software":false},
        "device":{"driver_name":"venus"},"frames_presented":419,"window_closed_early":false,
        "readback":{"ok":true,"tolerance":2,"pixels":[
            {"name":"red_top_left","expected":[255,0,0],"actual":[255,0,0],"match":true},
            {"name":"blue_top_right","expected":[0,0,255],"actual":[0,0,255],"match":true},
            {"name":"green_bottom_left","expected":[0,255,0],"actual":[0,255,0],"match":true}
        ]},"errors":[]
    });
    render_probe_report(&serde_json::to_vec(&good).unwrap()).unwrap();
    for (pointer, value) in [
        ("/adapter/software", json!(true)),
        ("/adapter/name", json!("llvmpipe")),
        ("/device/driver_name", json!("lavapipe")),
        ("/frames_presented", json!(0)),
        ("/session_id", json!(0)),
        ("/readback/tolerance", json!(255)),
        ("/readback/pixels/0/actual", json!([0, 0, 0])),
        ("/readback/pixels/0/actual", json!([256, 0, 0])),
        ("/readback/pixels/0/expected", json!([0, 0, 0])),
        ("/readback/pixels/0/name", json!("blue_top_right")),
        ("/errors", json!(["rendering failed"])),
    ] {
        let mut bad = good.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(
            render_probe_report(&serde_json::to_vec(&bad).unwrap()).is_err(),
            "accepted invalid field {pointer}: {bad}"
        );
    }
    for output in [b"not JSON".as_slice(), br#"{}"#, br#"{"schema":1}"#] {
        assert!(render_probe_report(output).is_err());
    }
    let mut near = good;
    near["readback"]["pixels"][0]["actual"] = json!([253, 2, 0]);
    render_probe_report(&serde_json::to_vec(&near).unwrap()).unwrap();
}

#[test]
fn windows_bugcheck_gate_requires_d3d11_architecture_and_hardware_readback() {
    let good = json!({
        "schema":1,"probe":"d3d11","api":"Direct3D 11","arch":"x64","result":"pass","exit_code":0,
        "session_id":1,"adapter":{"name":"Red Hat VirtIO GPU","software":false,
            "vendor_id":"0x1af4","device_id":"0x1050","dxgi_flag_software":false},
        "device":{"driver_type":"HARDWARE","feature_level":"11_0"},"args":{"warp":false},
        "frames_presented":419,"window_closed_early":false,
        "readback":{"ok":true,"tolerance":2,"pixels":[
            {"name":"red_top_left","expected":[255,0,0],"actual":[255,0,0],"match":true},
            {"name":"blue_top_right","expected":[0,0,255],"actual":[0,0,255],"match":true},
            {"name":"green_bottom_left","expected":[0,255,0],"actual":[0,255,0],"match":true}
        ]},"errors":[]
    });
    for arch in ["x64", "x86"] {
        let mut report = good.clone();
        report["arch"] = json!(arch);
        d3d11_probe_report(&serde_json::to_vec(&report).unwrap(), arch).unwrap();
    }
    for (pointer, value) in [
        ("/arch", json!("x86")),
        ("/probe", json!("vulkan")),
        ("/api", json!("Vulkan")),
        ("/adapter/software", json!(true)),
        ("/adapter/dxgi_flag_software", json!(true)),
        ("/adapter/vendor_id", json!("0x1414")),
        ("/adapter/device_id", json!("0x1111")),
        ("/adapter/name", json!("")),
        ("/device/driver_type", json!("WARP")),
        ("/args/warp", json!(true)),
        ("/readback/pixels/0/actual", json!([0, 0, 0])),
        ("/readback/pixels/1/name", json!("red_top_left")),
        ("/readback/tolerance", json!(255)),
        ("/frames_presented", json!(0)),
    ] {
        let mut bad = good.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(
            d3d11_probe_report(&serde_json::to_vec(&bad).unwrap(), "x64").is_err(),
            "accepted invalid D3D11 field {pointer}: {bad}"
        );
    }
    let mut missing_marker = good;
    missing_marker["readback"]["pixels"]
        .as_array_mut()
        .unwrap()
        .pop();
    assert!(d3d11_probe_report(&serde_json::to_vec(&missing_marker).unwrap(), "x64").is_err());
    assert!(d3d11_probe_report(b"{}", "x64").is_err());
}

#[test]
fn windows_bugcheck_gate_exports_only_evidence_without_overwriting() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let destination = temporary.path().join("evidence");
    std::fs::create_dir(&source).unwrap();
    let evidence = [
        "hv.log",
        "serial.log",
        "machine.json",
        "windows-health-forced-tdr-000.json",
        "windows-forced-tdr.json",
        "windows-forced-tdr.stderr",
        "windows-tdr-before-tdr-trace.json",
        "windows-tdr-before-tdr-trace.exit-code",
        "windows-tdr-disconnected-tdr-status.error",
        "windows-render-after-tdr.run.json",
        "windows-render-after-tdr.json",
        "windows-render-after-tdr.stderr",
        "windows-render-d3d11-x64-before-tdr.json",
        "windows-render-d3d11-x86-after-tdr.json",
    ];
    let excluded = [
        "disk.qcow2",
        "disk.json",
        "vars.fd",
        "vmlab-hv",
        "windows-render-probe.exe",
    ];
    for name in evidence.iter().chain(&excluded) {
        std::fs::write(source.join(name), name).unwrap();
    }
    let first = save_windows_health(&source, "guest reboot", b"first reboot").unwrap();
    let second = save_windows_health(&source, "guest reboot", b"second reboot").unwrap();
    assert_ne!(first, second);
    assert_eq!(std::fs::read(&first).unwrap(), b"first reboot");
    assert_eq!(std::fs::read(&second).unwrap(), b"second reboot");
    assert!(save_windows_health(&source, "../../unsafe", b"invalid").is_err());
    std::fs::create_dir(source.join("tpm")).unwrap();
    std::fs::create_dir(source.join("windows-render-directory.json")).unwrap();
    std::os::unix::fs::symlink(
        source.join("disk.qcow2"),
        source.join("windows-tdr-link.json"),
    )
    .unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(source.join("control.sock")).unwrap();
    export_windows_artifacts(&source, &destination).unwrap();
    assert_eq!(
        std::fs::read_dir(&destination).unwrap().count(),
        evidence.len() + 2
    );
    for name in evidence {
        assert_eq!(
            std::fs::read(destination.join(name)).unwrap(),
            name.as_bytes()
        );
    }
    assert!(source.join("disk.qcow2").is_file());
    std::fs::write(destination.join("hv.log"), b"preserve existing evidence").unwrap();
    assert_eq!(
        export_windows_artifacts(&source, &destination)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(
        std::fs::read(destination.join("hv.log")).unwrap(),
        b"preserve existing evidence"
    );
}
