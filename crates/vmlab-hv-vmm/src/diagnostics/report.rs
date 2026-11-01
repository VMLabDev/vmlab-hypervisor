//! Process-wide logging and crash reports. No device state or guest payload is serialized.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use log::{LevelFilter, Log, Metadata, Record};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use vmlab_hv_proto::control::ProcessExitReason;

const LOG_CAPACITY: usize = 1024;
const MESSAGE_LIMIT: usize = 4096;
static LOGGER: OnceLock<Recorder> = OnceLock::new();
static CONTEXT: OnceLock<Context> = OnceLock::new();
static STDERR: Mutex<()> = Mutex::new(());
static FINISHED: AtomicBool = AtomicBool::new(false);
static EXIT_RIP: AtomicBool = AtomicBool::new(false);
const RECORDER_TARGET: &str = "vmlab_hv_vmm::diagnostics::flight";
type StateReader = Arc<dyn Fn() -> Value + Send + Sync>;
type ExitSink = Arc<dyn Fn(ProcessExitReason, Vec<String>) + Send + Sync>;

pub struct Recorder {
    filter: RwLock<env_filter::Filter>,
    logs: Mutex<VecDeque<Value>>,
    secrets: RwLock<Vec<String>>,
}

struct Context {
    directory: PathBuf,
    machine: Value,
    state: RwLock<Option<StateReader>>,
    exit_sink: RwLock<Option<ExitSink>>,
}

fn parse_filter(filter: &str) -> Result<env_filter::Filter, String> {
    let mut builder = env_filter::Builder::new();
    builder.try_parse(filter).map_err(|e| e.to_string())?;
    Ok(builder.build())
}

fn recorder_debug(filter: &env_filter::Filter) -> bool {
    filter.enabled(
        &Metadata::builder()
            .level(log::Level::Debug)
            .target(RECORDER_TARGET)
            .build(),
    )
}

/// Cached target/level selection; the exit loop never locks the log filter.
#[inline]
pub(crate) fn capture_exit_rip() -> bool {
    EXIT_RIP.load(Ordering::Relaxed)
}

pub fn init_logging() -> Result<(), String> {
    let filter = std::env::var("VMLAB_HV_LOG").unwrap_or_else(|_| "info".into());
    let recorder = Recorder {
        filter: RwLock::new(parse_filter(&filter)?),
        logs: Mutex::new(VecDeque::with_capacity(LOG_CAPACITY)),
        secrets: RwLock::new(Vec::new()),
    };
    let logger = LOGGER.get_or_init(|| recorder);
    log::set_logger(logger).map_err(|e| e.to_string())?;
    EXIT_RIP.store(recorder_debug(&logger.filter.read()), Ordering::Relaxed);
    // All log records go to the ring, independently of the stderr filter.
    log::set_max_level(LevelFilter::Trace);
    Ok(())
}

pub(crate) fn set_log_filter(filter: &str) -> Result<(), String> {
    let parsed = parse_filter(filter)?;
    let logger = LOGGER.get().ok_or("logging is not initialized")?;
    let mut filter = logger.filter.write();
    EXIT_RIP.store(recorder_debug(&parsed), Ordering::Relaxed);
    *filter = parsed;
    drop(filter);
    log::info!("stderr log filter changed");
    Ok(())
}

impl Log for Recorder {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        let mut message = self.scrub(&record.args().to_string());
        truncate(&mut message, MESSAGE_LIMIT);
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let entry = json!({
            "unix_ns": super::flight::unix_ns(), "level": record.level().as_str(),
            "thread": name, "vcpu": name.strip_prefix("vcpu-").and_then(|n| n.parse::<u8>().ok()),
            "target": record.target(), "message": message,
        });
        {
            let mut logs = self.logs.lock();
            if logs.len() == LOG_CAPACITY {
                logs.pop_front();
            }
            logs.push_back(entry.clone());
        }
        // Evaluate message regexes against the already formatted, redacted text.
        if self.filter.read().matches(
            &Record::builder()
                .metadata(record.metadata().clone())
                .args(format_args!("{message}"))
                .build(),
        ) {
            let _output = STDERR.lock();
            if !FINISHED.load(Ordering::Acquire) {
                let _ = writeln!(std::io::stderr().lock(), "{entry}");
            }
        }
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

impl Recorder {
    fn scrub(&self, text: &str) -> String {
        let mut output = text.to_owned();
        for secret in self.secrets.read().iter() {
            output = output.replace(secret, "[REDACTED]");
        }
        output
    }
}

fn truncate(text: &mut String, limit: usize) {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}

fn secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "osk",
        "secret",
        "password",
        "credential",
        "token",
        "private_key",
    ]
    .iter()
    .any(|s| key.contains(s))
}

/// Register secret values before validation can include them in an error.
/// Unknown fields are removed from the report, including future machine fields.
pub fn configure(control: &Path, machine: &Value) -> Result<(), String> {
    let mut secrets = Vec::new();
    collect_secrets(machine, false, &mut secrets);
    if let Some(logger) = LOGGER.get() {
        logger.secrets.write().extend(secrets);
    }
    let directory = control
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let context = Context {
        directory,
        machine: redact_machine(machine),
        state: RwLock::new(None),
        exit_sink: RwLock::new(None),
    };
    CONTEXT
        .set(context)
        .map_err(|_| "diagnostics already configured")?;
    super::signal::install()?;
    std::panic::set_hook(Box::new(|info| {
        // Never unwind out of a hook or wait on a VM/device mutex held by this thread.
        let text = scrub(&info.to_string());
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let reason = ProcessExitReason::Panic;
        emit_exit(reason.clone(), vec![text.clone()]);
        let result = write_report(
            reason.clone(),
            vec![text.clone()],
            Some(json!({"message":text, "backtrace":backtrace})),
        );
        finish_report(result);
        super::signal::cleanup();
        std::process::exit(reason.exit_code() as i32);
    }));
    Ok(())
}

fn collect_secrets(value: &Value, secret: bool, output: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                collect_secrets(value, secret || secret_key(key), output);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_secrets(item, secret, output);
            }
        }
        Value::String(text) if secret && !text.is_empty() => output.push(text.clone()),
        Value::Number(number) if secret => output.push(number.to_string()),
        _ => {}
    }
}

// Reviewed schema paths. New fields are private until added here, regardless
// of whether their author remembered to name them "secret".
fn safe_fields(path: &str) -> &'static [&'static str] {
    match path {
        "" => &[
            "exit_on_reset",
            "vfio",
            "machine",
            "cpu",
            "cpu_model",
            "hyperv",
            "display",
            "tablet",
            "usb",
            "tpm",
            "console_pci",
            "ahci_pci",
            "name",
            "arch",
            "accel",
            "vcpus",
            "memory_mib",
            "boot",
            "disks",
            "nics",
            "serial_log",
            "serial_log_port",
            "ports",
            "shares",
            "audio",
            "rtc_base",
            "floppy",
        ],
        "boot" => &[
            "kind",
            "kernel",
            "initrd",
            "cmdline",
            "firmware",
            "code",
            "vars",
            "secure_boot",
            "bootorder",
        ],
        "vfio[]" => &["host", "pci"],
        "audio" => &[
            "pci",
            "device",
            "microphone",
            "node_name",
            "node_description",
        ],
        "disks[]" => &[
            "channel",
            "unit",
            "pci",
            "bus",
            "port",
            "cdrom",
            "id",
            "path",
            "format",
            "read_only",
            "vmstate",
        ],
        "nics[]" => &["model", "rom", "pci", "mac", "mtu", "attachment"],
        "nics[].attachment" => &["kind", "socket"],
        "ports[]" => &["transport", "name", "socket"],
        "shares[]" => &["pci", "tag", "socket", "path", "read_only"],
        "tpm" => &["socket"],
        "display" => &[
            "device",
            "gpu",
            "render_node",
            "pci",
            "rom",
            "socket",
            "vnc_socket",
        ],
        "tablet" | "usb" | "console_pci" | "ahci_pci" | "vfio[].pci" | "audio.pci"
        | "disks[].pci" | "nics[].pci" | "shares[].pci" | "display.pci" => {
            &["bus", "device", "function"]
        }
        _ => &[],
    }
}

fn project_machine(value: &Value, path: &str, secrets: &mut Vec<String>) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let child = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    let value = if secret_key(key) || !safe_fields(path).contains(&key.as_str()) {
                        collect_secrets(value, true, secrets);
                        json!("[REDACTED]")
                    } else {
                        project_machine(value, &child, secrets)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| project_machine(v, &format!("{path}[]"), secrets))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn redact_machine(machine: &Value) -> Value {
    let mut secrets = Vec::new();
    let safe = project_machine(machine, "", &mut secrets);
    if let Some(logger) = LOGGER.get() {
        logger.secrets.write().extend(secrets);
    }
    safe
}

fn redact_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if secret_key(key) {
                    *value = json!("[REDACTED]");
                } else {
                    redact_keys(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_keys(item);
            }
        }
        _ => {}
    }
}

pub fn scrub(text: &str) -> String {
    LOGGER
        .get()
        .map_or_else(|| text.to_owned(), |logger| logger.scrub(text))
}

pub(crate) fn set_state_reader(reader: StateReader, sink: Option<ExitSink>) {
    if let Some(context) = CONTEXT.get() {
        *context.state.write() = Some(reader);
        *context.exit_sink.write() = sink;
    }
}

fn emit_exit(reason: ProcessExitReason, errors: Vec<String>) {
    if let Some(context) = CONTEXT.get()
        && let Some(sink) = context.exit_sink.try_read().and_then(|s| s.clone())
    {
        sink(reason, errors.into_iter().map(|e| scrub(&e)).collect());
    }
}

pub(crate) fn report(
    reason: ProcessExitReason,
    errors: Vec<String>,
    panic: Option<Value>,
) -> Value {
    let context = CONTEXT.get();
    let state = context
        .and_then(|c| c.state.try_read())
        .and_then(|s| s.clone())
        .map(|read| read());
    let logs = LOGGER
        .get()
        .and_then(|l| l.logs.try_lock())
        .map(|logs| logs.iter().cloned().collect::<Vec<_>>());
    let mut result = json!({
        "schema_version":1, "unix_ms": super::flight::unix_ns() / 1_000_000,
        "hv_version":env!("CARGO_PKG_VERSION"), "git_revision":env!("VMLAB_HV_GIT_REV"),
        "pid":std::process::id(), "exit_code":reason.exit_code(), "reason":reason,
        "error_chain":errors, "panic":panic, "machine":context.map(|c| &c.machine),
        "logs":logs, "vm":state,
        "process_status":std::fs::read_to_string("/proc/self/status").ok(),
        "threads":std::fs::read_dir("/proc/self/task").ok().map(|it| it.filter_map(Result::ok).filter_map(|e| std::fs::read_to_string(e.path().join("status")).ok()).collect::<Vec<_>>()),
        "thread_count":std::fs::read_dir("/proc/self/task").ok().map(|it| it.count()),
        "kernel":std::fs::read_to_string("/proc/sys/kernel/osrelease").ok(),
    });
    redact_keys(&mut result);
    scrub_values(&mut result);
    result
}

fn scrub_values(value: &mut Value) {
    match value {
        Value::String(text) => *text = scrub(text),
        Value::Object(map) => {
            for value in map.values_mut() {
                scrub_values(value);
            }
        }
        Value::Array(items) => {
            for value in items {
                scrub_values(value);
            }
        }
        _ => {}
    }
}

pub fn write_report(
    reason: ProcessExitReason,
    errors: Vec<String>,
    panic: Option<Value>,
) -> Result<PathBuf, String> {
    let value = report(reason, errors, panic);
    write_value(&value)
}

pub(crate) fn dump_report() -> Result<PathBuf, String> {
    let mut value = report(ProcessExitReason::Quit, vec![], None);
    value["reason"] = Value::Null;
    value["exit_code"] = Value::Null;
    value["capture"] = "on_demand".into();
    write_value(&value)
}

fn write_value(value: &Value) -> Result<PathBuf, String> {
    let context = CONTEXT.get().ok_or("diagnostics are not configured")?;
    let ms = super::flight::unix_ns() / 1_000_000;
    // create_new preserves an existing report even if two dumps share a clock tick.
    for offset in 0..1000 {
        let path = context
            .directory
            .join(format!("crash-{}.json", ms + offset));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path);
        match file {
            Ok(mut file) => {
                serde_json::to_writer_pretty(&mut file, value).map_err(|e| e.to_string())?;
                file.write_all(b"\n")
                    .and_then(|_| file.sync_all())
                    .map_err(|e| e.to_string())?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("writing diagnostic report {}: {e}", path.display())),
        }
    }
    Err("cannot allocate a unique report filename".into())
}

/// Bypass the stderr filter: the last line must identify the report even at `off`.
pub fn finish_report(result: Result<PathBuf, String>) {
    let message = match result {
        Ok(path) => format!("crash report: {}", path.display()),
        Err(e) => format!("crash report failed: {e}"),
    };
    let entry = json!({"unix_ns":super::flight::unix_ns(), "level":"ERROR", "thread":std::thread::current().name(), "message":scrub(&message)});
    let _output = STDERR.lock();
    FINISHED.store(true, Ordering::Release);
    let _ = writeln!(std::io::stderr().lock(), "{entry}");
}

pub(crate) fn directory() -> Option<&'static Path> {
    CONTEXT.get().map(|c| c.directory.as_path())
}

pub(crate) fn configured() -> bool {
    CONTEXT.get().is_some() && LOGGER.get().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_ring_is_bounded_and_keeps_debug_with_stderr_off() {
        let logger = Recorder {
            filter: RwLock::new(parse_filter("off").unwrap()),
            logs: Mutex::new(VecDeque::new()),
            secrets: RwLock::new(vec![]),
        };
        for _ in 0..2000 {
            logger.log(
                &Record::builder()
                    .level(log::Level::Debug)
                    .args(format_args!("diagnostic detail"))
                    .build(),
            );
        }
        let logs = logger.logs.lock();
        assert_eq!(logs.len(), LOG_CAPACITY);
        assert_eq!(logs[0]["level"], "DEBUG");
    }

    #[test]
    fn future_secret_fields_are_removed_and_values_scrubbed() {
        // Generate a synthetic value at runtime; no key material is a fixture.
        let fake = (0..64)
            .map(|n| char::from(b'A' + n % 26))
            .collect::<String>();
        let machine = json!({"name":"test", "applesmc":{"osk":fake}, "future":{"secret":fake}});
        let mut secrets = Vec::new();
        collect_secrets(&machine, false, &mut secrets);
        let logger = Recorder {
            filter: RwLock::new(parse_filter("off").unwrap()),
            logs: Mutex::new(VecDeque::new()),
            secrets: RwLock::new(secrets),
        };
        let safe = redact_machine(&machine);
        let report = json!({"machine":safe, "error":logger.scrub(&format!("bad value {fake}"))});
        assert!(!serde_json::to_string(&report).unwrap().contains(&fake));
        assert_eq!(report["error"], "bad value [REDACTED]");
    }

    #[test]
    fn invalid_filter_does_not_replace_a_valid_filter() {
        assert!(parse_filter("info,vmlab_hv_vmm=debug").is_ok());
        assert!(parse_filter("vmlab=invalid-level").is_err());
    }

    #[test]
    fn exit_rip_follows_the_recorder_target_level_not_other_debug_logs() {
        for filter in [
            "off",
            "info",
            "vmlab_hv_devices=debug",
            "debug,vmlab_hv_vmm::diagnostics::flight=info",
            "trace,vmlab_hv_vmm::diagnostics::flight=off",
        ] {
            assert!(!recorder_debug(&parse_filter(filter).unwrap()), "{filter}");
        }
        for filter in [
            "debug",
            "trace",
            "vmlab_hv_vmm=debug",
            "info,vmlab_hv_vmm::diagnostics::flight=debug",
            "off,vmlab_hv_vmm::diagnostics::flight=trace",
            "vmlab_hv_vmm::diagnostics::flight=debug/message-regex",
        ] {
            assert!(recorder_debug(&parse_filter(filter).unwrap()), "{filter}");
        }
    }

    #[test]
    fn panic_report_child() {
        let Some(directory) = std::env::var_os("VMLAB_HV_PANIC_TEST_DIR") else {
            return;
        };
        init_logging().unwrap();
        let fake = (0..64)
            .map(|n| char::from(b'A' + n % 26))
            .collect::<String>();
        configure(
            &PathBuf::from(directory).join("control.sock"),
            &json!({
                "name":"panic-test", "applesmc":{"osk":fake},
                "future_field":{"opaque_value":fake},
            }),
        )
        .unwrap();
        log::debug!("retained debug record");
        // Exercise the devices crate's real logging path with its null backend.
        let _audio = vmlab_hv_devices::audio::select_backend("diagnostic-test", "diagnostic test");
        let _locked_log_ring =
            std::env::var_os("VMLAB_HV_PANIC_TEST_LOCK").map(|_| LOGGER.get().unwrap().logs.lock());
        panic!("injected panic containing {fake}");
    }

    #[test]
    fn panic_report_keeps_backtrace_and_device_logs_and_redacts_secrets() {
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::report::tests::panic_report_child",
                "--nocapture",
            ])
            .env("VMLAB_HV_PANIC_TEST_DIR", directory.path())
            .env("VMLAB_HV_LOG", "off")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        let reports: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(reports.len(), 1, "private checkpoints should be cleaned up");
        let text = std::fs::read_to_string(reports[0].path()).unwrap();
        let fake = (0..64)
            .map(|n| char::from(b'A' + n % 26))
            .collect::<String>();
        assert!(!text.contains(&fake));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(&fake));
        let report: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["reason"]["kind"], "panic");
        assert!(
            report["panic"]["backtrace"]
                .as_str()
                .unwrap()
                .contains("panic_report_child")
        );
        let logs = report["logs"].as_array().unwrap();
        assert!(
            logs.iter()
                .any(|entry| entry["message"] == "retained debug record")
        );
        assert!(logs.iter().any(|entry| {
            entry["target"]
                .as_str()
                .unwrap()
                .starts_with("vmlab_hv_devices")
        }));
        let last: Value = serde_json::from_str(
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .last()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(last["level"], "ERROR");
        assert!(
            last["message"]
                .as_str()
                .unwrap()
                .starts_with("crash report: ")
        );
    }

    #[test]
    fn machine_projection_defaults_new_fields_to_private() {
        let mut secrets = Vec::new();
        let safe = project_machine(
            &json!({
                "name":"fixture", "boot":{"kind":"kernel", "kernel":"kernel.bin", "new_field":"sensitive-value"},
                "nics":[{"mac":"52:54:00:00:00:01", "attachment":{"kind":"stream", "socket":"switch.sock", "new_auth":"sensitive-auth"}}],
            }),
            "",
            &mut secrets,
        );
        assert_eq!(safe["boot"]["kernel"], "kernel.bin");
        assert_eq!(safe["boot"]["new_field"], "[REDACTED]");
        assert_eq!(safe["nics"][0]["attachment"]["new_auth"], "[REDACTED]");
        assert_eq!(secrets, vec!["sensitive-value", "sensitive-auth"]);
    }

    #[test]
    fn panic_while_holding_the_log_ring_still_produces_a_report() {
        let directory = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::report::tests::panic_report_child",
                "--nocapture",
            ])
            .env("VMLAB_HV_PANIC_TEST_DIR", directory.path())
            .env("VMLAB_HV_PANIC_TEST_LOCK", "1")
            .env("VMLAB_HV_LOG", "off")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert_eq!(status.code(), Some(3));
                break;
            }
            if std::time::Instant::now() > deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("panic reporter waited on a lock held by the panicking thread");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let path = std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| e.path().extension().is_some_and(|e| e == "json"))
            .unwrap()
            .path();
        let report: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(report["logs"].is_null());
        assert_eq!(report["reason"]["kind"], "panic");
    }
}
