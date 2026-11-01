//! `vmlab-hv --machine <machine.json> --control <control.sock> [--paused]`
//!
//! One process per machine. Exit status: 0 on guest-initiated or requested
//! clean shutdown (S5, guest reset, `quit`). Other outcomes and reports are
//! documented in docs/diagnostics.md. The control socket is bound before vCPUs start.

use std::path::PathBuf;
use std::process::ExitCode;

use vmlab_hv_proto::MachineDescription;
use vmlab_hv_proto::control::ProcessExitReason;
use vmlab_hv_vmm::{VmHandle, builder, control, diagnostics, vm::ExitReason};

use crate::devices::DeviceSet;

const USAGE: &str = "usage: vmlab-hv --machine <machine.json> --control <control.sock> [--paused]";

struct Args {
    machine: PathBuf,
    control: PathBuf,
    paused: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut machine = None;
    let mut control = None;
    let mut paused = false;
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--machine" => machine = Some(next_value(&mut argv, &arg)?),
            "--control" => control = Some(next_value(&mut argv, &arg)?),
            "--paused" => paused = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                log::info!("process exit: help; exit_code=0");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Args {
        machine: machine.ok_or("missing --machine")?,
        control: control.ok_or("missing --control")?,
        paused,
    })
}

fn next_value(argv: &mut impl Iterator<Item = String>, flag: &str) -> Result<PathBuf, String> {
    argv.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("{flag} needs a value"))
}

#[cfg(test)]
mod a20_acceptance;
#[cfg(test)]
mod acceptance;
mod devices;
#[cfg(test)]
mod display_acceptance;
#[cfg(test)]
mod macos_acceptance;
#[cfg(test)]
mod macos_hwref;
#[cfg(test)]
mod pc_acceptance;
#[cfg(test)]
mod pc_hwref;

fn main() -> ExitCode {
    if let Err(message) = diagnostics::report::init_logging() {
        // Logging cannot report its own initialization failure through the facade.
        eprintln!(
            "vmlab-hv: logging initialization: {message}; exit_reason=startup_error exit_code=2"
        );
        return ExitCode::from(ProcessExitReason::StartupError.exit_code());
    }
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            log::error!("{message}\n{USAGE}; exit_reason=startup_error exit_code=2");
            return ExitCode::from(ProcessExitReason::StartupError.exit_code());
        }
    };

    let desc = match read_machine(&args.machine, &args.control) {
        Ok(desc) => desc,
        Err(message) => {
            log::error!("{message}; exit_reason=startup_error exit_code=2");
            diagnostics::signal::cleanup();
            return ExitCode::from(ProcessExitReason::StartupError.exit_code());
        }
    };

    let (reason, errors) = match run(desc, args) {
        Ok(reason) => (reason.process_reason(), reason.error_chain()),
        Err(message) => (
            ProcessExitReason::StartupError,
            vec![diagnostics::report::scrub(&message)],
        ),
    };
    let code = reason.exit_code();
    if code == 0 {
        log::info!("process exit: {reason:?}; exit_code={code}");
    } else {
        log::error!("process exit: {reason:?}; exit_code={code}; error_chain={errors:?}");
    }
    let report = if matches!(
        reason,
        ProcessExitReason::RuntimeError | ProcessExitReason::GuestPanic
    ) || std::env::var_os("VMLAB_HV_DIAGNOSTICS").is_some()
    {
        Some(diagnostics::report::write_report(reason, errors, None))
    } else {
        None
    };
    // No worker or teardown log may follow the report path on stderr.
    diagnostics::signal::cleanup();
    if let Some(report) = report {
        diagnostics::report::finish_report(report);
    }
    ExitCode::from(code)
}

fn read_machine(path: &PathBuf, control: &std::path::Path) -> Result<MachineDescription, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    // Syntax errors report only location, never the input line or a secret value.
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        format!(
            "parsing {}: invalid JSON at line {} column {}",
            path.display(),
            e.line(),
            e.column()
        )
    })?;
    diagnostics::report::configure(control, &value)?;
    serde_json::from_value(value).map_err(|e| {
        format!(
            "parsing {}: {}",
            path.display(),
            diagnostics::report::scrub(&e.to_string())
        )
    })
}

fn run(desc: MachineDescription, args: Args) -> Result<ExitReason, String> {
    // sigwait requires termination signals blocked before workers inherit the mask.
    block_termination_signals()?;
    log::info!(
        "vmlab-hv {}: building machine {:?}, {} vCPUs, {} MiB",
        env!("CARGO_PKG_VERSION"),
        desc.name,
        desc.vcpus,
        desc.memory_mib
    );

    // Refuse an occupied control path before doing any work, so a stale
    // socket or a second launch fails fast and leaves the path alone.
    if args.control.exists() {
        return Err(format!(
            "control socket: {} already exists",
            args.control.display()
        ));
    }

    let mut vm = builder::build(&desc, &DeviceSet, true)
        .map_err(|e| format!("building machine `{}`: {e}", desc.name))?;
    vm.attach_diagnostics();
    let handle = vm.handle();

    // Bind every served socket before the vCPUs run.
    if let Some(display) = &desc.display {
        let directory = args.control.parent().unwrap_or(std::path::Path::new("."));
        let native = display
            .socket
            .clone()
            .unwrap_or_else(|| directory.join("display.sock"));
        let rfb = display
            .vnc_socket
            .clone()
            .unwrap_or_else(|| directory.join("vnc.sock"));
        handle
            .serve_display(&native, &rfb)
            .map_err(|e| e.to_string())?;
    }
    let control =
        control::serve(&args.control, &handle).map_err(|e| format!("control socket: {e}"))?;

    diagnostics::signal::checkpoint()?;

    let signal_waiter = spawn_signal_waiter(handle.clone())?;

    // A low-frequency checkpoint is prepared outside all signal handlers.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let checkpoint = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("diagnostic-checkpoint".into())
            .spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    if let Err(error) = diagnostics::signal::checkpoint() {
                        log::warn!("fatal checkpoint: {error}");
                    }
                    std::thread::park_timeout(std::time::Duration::from_secs(1));
                }
            })
            .map_err(|e| format!("spawning diagnostic checkpoint: {e}"))?
    };

    if !args.paused {
        vm.start();
    }

    handle.wait_exit();
    // Stop checkpoint readers before taking the final joined register sample.
    stop.store(true, std::sync::atomic::Ordering::Release);
    checkpoint.thread().unpark();
    checkpoint
        .join()
        .map_err(|_| "diagnostic checkpoint panicked".to_string())?;
    let reason = vm.wait();
    // Wake an idle sigwait without inventing another exit reason. The state
    // is already ShuttingDown, so the waiter returns without logging a signal.
    if !signal_waiter.is_finished() {
        use std::os::unix::thread::JoinHandleExt;
        // SAFETY: this unjoined thread's pthread id is valid and SIGTERM is blocked.
        unsafe {
            libc::pthread_kill(signal_waiter.as_pthread_t(), libc::SIGTERM);
        }
    }
    signal_waiter
        .join()
        .map_err(|_| "signal waiter panicked".to_string())?;
    control
        .join()
        .map_err(|_| "control server panicked".to_string())?;
    Ok(reason)
}

fn block_termination_signals() -> Result<(), String> {
    // SAFETY: pthread_sigmask changes only the calling thread's mask.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGQUIT);
        libc::sigaddset(&mut set, libc::SIGINT);
        let status = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        if status != 0 {
            return Err(format!(
                "cannot block termination signals: {}",
                std::io::Error::from_raw_os_error(status)
            ));
        }
    }
    Ok(())
}

fn spawn_signal_waiter(handle: VmHandle) -> Result<std::thread::JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("signal-wait".into())
        .spawn(move || {
            // SAFETY: sigwait on the set blocked process-wide at startup.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGTERM);
                libc::sigaddset(&mut set, libc::SIGQUIT);
                libc::sigaddset(&mut set, libc::SIGINT);
                let mut sig: libc::c_int = 0;
                let status = libc::sigwait(&set, &mut sig);
                if handle.status() == vmlab_hv_proto::control::RunState::ShuttingDown {
                    return;
                }
                if status == 0 {
                    log::info!("caught signal {sig}, stopping the machine");
                    handle.host_signal(sig);
                } else {
                    handle.runtime_error(format!(
                        "sigwait: {}",
                        std::io::Error::from_raw_os_error(status)
                    ));
                }
            }
        })
        .map_err(|e| format!("spawning signal waiter: {e}"))
}
