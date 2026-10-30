//! `vmlab-hv --machine <machine.json> --control <control.sock> [--paused]`
//!
//! One process per machine. Exit status: 0 on guest-initiated or requested
//! clean shutdown (S5, guest reset, `quit`), 2 on a launch problem (usage,
//! unreadable machine description, missing kernel, absent device model), 1
//! on a runtime failure. The control socket is bound before the vCPUs start.

use std::path::PathBuf;
use std::process::ExitCode;

use vmlab_hv_proto::MachineDescription;
use vmlab_hv_vmm::{VmHandle, builder, control, vm::ExitReason};

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
mod acceptance;
mod devices;
#[cfg(test)]
mod display_acceptance;

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("vmlab-hv: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let desc = match read_machine(&args.machine) {
        Ok(desc) => desc,
        Err(message) => {
            eprintln!("vmlab-hv: {message}");
            return ExitCode::from(2);
        }
    };

    match run(desc, args) {
        Ok(ExitReason::Shutdown(_)) => ExitCode::SUCCESS,
        Ok(ExitReason::Error(message)) => {
            eprintln!("vmlab-hv: {message}");
            ExitCode::FAILURE
        }
        Err(message) => {
            eprintln!("vmlab-hv: {message}");
            ExitCode::from(2)
        }
    }
}

fn read_machine(path: &PathBuf) -> Result<MachineDescription, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))
}

fn run(desc: MachineDescription, args: Args) -> Result<ExitReason, String> {
    // SIGTERM/SIGQUIT stop the machine as a host request instead of killing
    // the process mid-flight. sigwait needs them blocked in every thread, so
    // block before the vCPU threads exist.
    block_termination_signals();

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
    let handle = vm.handle();

    // Bind every served socket before the vCPUs run.
    control::serve(&args.control, &handle).map_err(|e| format!("control socket: {e}"))?;
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

    spawn_signal_waiter(handle.clone());

    if !args.paused {
        vm.start();
    }

    // Joins the vCPU threads once the exit reason is set; the control
    // thread serves until the process ends.
    Ok(vm.wait())
}

fn block_termination_signals() {
    // SAFETY: pthread_sigmask on the calling thread; failure only means the
    // default signal behaviour stays, which is safe to run without.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGQUIT);
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            eprintln!(
                "vmlab-hv: cannot block signals: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

fn spawn_signal_waiter(handle: VmHandle) {
    std::thread::Builder::new()
        .name("signal-wait".into())
        .spawn(move || {
            // SAFETY: sigwait on the set blocked process-wide at startup.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGTERM);
                libc::sigaddset(&mut set, libc::SIGQUIT);
                let mut sig: libc::c_int = 0;
                if libc::sigwait(&set, &mut sig) == 0 {
                    eprintln!("vmlab-hv: caught signal {sig}, stopping the machine");
                    handle.quit();
                }
            }
        })
        .expect("spawn signal waiter");
}
