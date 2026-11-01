//! Fatal handlers use only fixed buffers, atomics, and Linux signal-safe syscalls.
//! The report body is prepared outside the handler; the handler appends the fault.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use parking_lot::Mutex;
use vmlab_hv_proto::control::ProcessExitReason;

static SIGNAL: OnceLock<SignalFiles> = OnceLock::new();
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static CLAIMED: AtomicBool = AtomicBool::new(false);
static UPDATE: Mutex<()> = Mutex::new(());

struct Slot {
    file: File,
    path: CString,
    fallback_line: Vec<u8>,
}
struct SignalFiles {
    slots: [Slot; 2],
    destination: CString,
    stderr_line: Vec<u8>,
    paths: [PathBuf; 2],
}

/// Rust installs a per-thread alternate signal stack for stack-overflow handling.
/// Preserve it when present; provide one for foreign or main threads without one.
pub(crate) fn ensure_alt_stack() -> Result<(), String> {
    // SAFETY: querying the current thread's stack writes only to `old`.
    let mut old: libc::stack_t = unsafe { std::mem::zeroed() };
    // SAFETY: old is valid output storage, and a null first argument only queries.
    if unsafe { libc::sigaltstack(std::ptr::null(), &mut old) } != 0 {
        return Err(format!(
            "sigaltstack query: {}",
            std::io::Error::last_os_error()
        ));
    }
    if old.ss_flags & libc::SS_DISABLE == 0 {
        return Ok(());
    }
    let stack = vec![0u8; 64 * 1024].into_boxed_slice();
    let stack = Box::leak(stack);
    let new = libc::stack_t {
        ss_sp: stack.as_mut_ptr().cast(),
        ss_flags: 0,
        ss_size: stack.len(),
    };
    // SAFETY: the leaked allocation outlives this thread's signal stack.
    if unsafe { libc::sigaltstack(&new, std::ptr::null_mut()) } != 0 {
        return Err(format!(
            "sigaltstack install: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

pub(crate) fn install() -> Result<(), String> {
    ensure_alt_stack()?;
    let directory = super::report::directory().ok_or("missing report directory")?;
    let ms = super::flight::unix_ns() / 1_000_000;
    let destination = directory.join(format!("crash-{ms}.json"));
    let paths =
        [0, 1].map(|n| directory.join(format!(".crash-{ms}-{}-{n}.pending", std::process::id())));
    let open = |path: &PathBuf| -> Result<Slot, String> {
        Ok(Slot {
            file:OpenOptions::new().append(true).create_new(true).mode(0o600).open(path).map_err(|e| format!("prepare fatal report: {e}"))?,
            path:CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?,
            fallback_line:(serde_json::json!({"level":"ERROR", "message":super::report::scrub(&format!("crash report publication failed; prepared report: {}", path.display()))}).to_string() + "\n").into_bytes(),
        })
    };
    let first = open(&paths[0])?;
    let second = match open(&paths[1]) {
        Ok(slot) => slot,
        Err(error) => {
            let _ = std::fs::remove_file(&paths[0]);
            return Err(error);
        }
    };
    let slots = [first, second];
    let line = serde_json::json!({"level":"ERROR", "message":super::report::scrub(&format!("crash report: {}", destination.display()))}).to_string() + "\n";
    SIGNAL
        .set(SignalFiles {
            slots,
            paths,
            destination: CString::new(destination.as_os_str().as_bytes())
                .map_err(|e| e.to_string())?,
            stderr_line: line.into_bytes(),
        })
        .map_err(|_| "fatal handler already installed")?;
    checkpoint()?;
    // SAFETY: handlers use only signal-safe operations and immutable static data.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = fatal as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESETHAND;
        libc::sigfillset(&mut action.sa_mask);
        for signal in [
            libc::SIGSEGV,
            libc::SIGBUS,
            libc::SIGILL,
            libc::SIGFPE,
            libc::SIGABRT,
        ] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(format!(
                    "sigaction({signal}): {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
    }
    Ok(())
}

/// Refresh the inactive checkpoint, then publish it. No report/device locks are
/// acquired by the signal handler. A checkpoint records its capture time.
pub fn checkpoint() -> Result<(), String> {
    let Some(files) = SIGNAL.get() else {
        return Ok(());
    };
    let Some(_update) = UPDATE.try_lock() else {
        return Ok(());
    };
    if CLAIMED.load(Ordering::SeqCst) {
        return Ok(());
    }
    let next = 1 - ACTIVE.load(Ordering::Acquire);
    let mut report =
        super::report::report(ProcessExitReason::FatalSignal { signal: 0 }, vec![], None);
    report["reason"] = serde_json::json!({"kind":"fatal_signal"});
    report["exit_code"] = serde_json::Value::Null;
    report["capture"] = "checkpoint; exact signal and code are in fault".into();
    let mut bytes = serde_json::to_vec(&report).map_err(|e| e.to_string())?;
    // Serialize with a real parser; remove only the known closing object token.
    assert_eq!(bytes.pop(), Some(b'}'));
    bytes.push(b',');
    let file = &files.slots[next].file;
    file.set_len(0).map_err(|e| e.to_string())?;
    (&*file).write_all(&bytes).map_err(|e| e.to_string())?;
    ACTIVE.store(next, Ordering::Release);
    Ok(())
}

pub fn cleanup() {
    if let Some(files) = SIGNAL.get() {
        for path in &files.paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

struct Buffer {
    bytes: [u8; 512],
    len: usize,
}
impl Buffer {
    fn text(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }
    fn number(&mut self, mut value: u64) {
        let mut digits = [0u8; 20];
        let mut n = 0;
        loop {
            digits[n] = b'0' + (value % 10) as u8;
            n += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        for i in (0..n).rev() {
            self.text(&digits[i..i + 1]);
        }
    }
}

extern "C" fn fatal(signal: i32, info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    if !CLAIMED.swap(true, Ordering::SeqCst)
        && let Some(files) = SIGNAL.get()
    {
        let slot = &files.slots[ACTIVE.load(Ordering::Acquire)];
        let mut buffer = Buffer {
            bytes: [0; 512],
            len: 0,
        };
        buffer.text(b"\"fault\":{\"signal\":");
        buffer.number(signal as u64);
        buffer.text(b",\"exit_code\":");
        buffer.number((128 + signal) as u64);
        buffer.text(b",\"address\":");
        // SAFETY: SA_SIGINFO supplies siginfo; si_addr is meaningful for faults.
        let address = unsafe {
            if info.is_null() || (*info).si_code <= 0 || signal == libc::SIGABRT {
                0
            } else {
                (*info).si_addr() as usize
            }
        };
        buffer.number(address as u64);
        buffer.text(b",\"tid\":");
        // SAFETY: gettid has no pointer arguments or allocation.
        buffer.number(unsafe { libc::syscall(libc::SYS_gettid) } as u64);
        buffer.text(b",\"unix_ns\":");
        // SAFETY: clock_gettime is async-signal-safe and ts is valid storage.
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        // SAFETY: valid clock id and output pointer.
        unsafe {
            libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts);
        }
        buffer.number(
            (ts.tv_sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(ts.tv_nsec as u64),
        );
        buffer.text(b"}}\n");
        // SAFETY: pre-opened fd and preallocated buffers. link, unlink and write are
        // async-signal-safe. The signal handler never formats, allocates or locks.
        unsafe {
            write_all(slot.file.as_raw_fd(), &buffer.bytes[..buffer.len]);
            // link never overwrites an existing report. Both paths are on
            // the same filesystem; unlink removes only our private pending name.
            if libc::link(slot.path.as_ptr(), files.destination.as_ptr()) == 0 {
                libc::unlink(slot.path.as_ptr());
                write_all(libc::STDERR_FILENO, &files.stderr_line);
            } else {
                write_all(libc::STDERR_FILENO, &slot.fallback_line);
            }
        }
    }
    // SA_RESETHAND has restored SIG_DFL. Queue the signal to this same thread;
    // it is delivered on return from the handler after its mask is restored.
    // SAFETY: raise is async-signal-safe and the default action produces a core.
    unsafe {
        libc::raise(signal);
    }
}

unsafe fn write_all(fd: i32, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        // SAFETY: bytes is a live preallocated slice and write is signal-safe.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n <= 0 {
            break;
        }
        bytes = &bytes[n as usize..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_signal_child() {
        let Some(directory) = std::env::var_os("VMLAB_HV_FATAL_TEST_DIR") else {
            return;
        };
        super::super::report::init_logging().unwrap();
        super::super::report::configure(
            &PathBuf::from(directory).join("control.sock"),
            &serde_json::json!({"name":"fatal-test"}),
        )
        .unwrap();
        if let Ok(signal) = std::env::var("VMLAB_HV_FATAL_TEST_SIGNAL") {
            let signal = signal.parse::<i32>().unwrap();
            assert!([libc::SIGBUS, libc::SIGILL, libc::SIGFPE, libc::SIGABRT].contains(&signal));
            // SAFETY: raise delivers a known fatal signal to this isolated child.
            unsafe {
                libc::setrlimit(
                    libc::RLIMIT_CORE,
                    &libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    },
                );
                libc::raise(signal);
            }
            unreachable!();
        }
        // Prevent this test from filling the host's core-dump storage.
        // SAFETY: setrlimit affects only this child. The inaccessible mapping
        // is used by inline assembly to trigger a real hardware page fault.
        unsafe {
            libc::setrlimit(
                libc::RLIMIT_CORE,
                &libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                },
            );
            let page = libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            assert_ne!(page, libc::MAP_FAILED);
            std::arch::asm!("mov byte ptr [{page}], 0", page = in(reg) page);
        }
        unreachable!();
    }

    #[test]
    fn fatal_signal_leaves_json_and_last_stderr_path() {
        use std::os::unix::process::ExitStatusExt;
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::signal::tests::fatal_signal_child",
                "--nocapture",
            ])
            .env("VMLAB_HV_FATAL_TEST_DIR", directory.path())
            .output()
            .unwrap();
        assert_eq!(output.status.signal(), Some(libc::SIGSEGV));
        let reports: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|e| e == "json"))
            .collect();
        assert_eq!(reports.len(), 1);
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(reports[0].path()).unwrap()).unwrap();
        assert_eq!(report["fault"]["signal"], libc::SIGSEGV);
        assert!(report["fault"]["tid"].as_u64().unwrap() > 0);
        assert!(report["fault"]["address"].as_u64().unwrap() > 0);
        let stderr = String::from_utf8(output.stderr).unwrap();
        let last: serde_json::Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
        assert!(
            last["message"]
                .as_str()
                .unwrap()
                .ends_with(reports[0].path().to_str().unwrap())
        );
    }

    #[test]
    fn other_fatal_signals_preserve_the_default_wait_status() {
        use std::os::unix::process::ExitStatusExt;
        for signal in [libc::SIGBUS, libc::SIGILL, libc::SIGFPE, libc::SIGABRT] {
            let directory = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "diagnostics::signal::tests::fatal_signal_child",
                    "--nocapture",
                ])
                .env("VMLAB_HV_FATAL_TEST_DIR", directory.path())
                .env("VMLAB_HV_FATAL_TEST_SIGNAL", signal.to_string())
                .output()
                .unwrap();
            assert_eq!(output.status.signal(), Some(signal));
            let path = std::fs::read_dir(directory.path())
                .unwrap()
                .filter_map(Result::ok)
                .find(|e| e.path().extension().is_some_and(|e| e == "json"))
                .unwrap()
                .path();
            let report: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(report["fault"]["signal"], signal);
            assert_eq!(report["fault"]["exit_code"], 128 + signal);
        }
    }
}
