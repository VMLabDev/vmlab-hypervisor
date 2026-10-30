//! Test children own a process group, a deadline, and an optional shared VM slot.

use std::fs::{File, OpenOptions};
use std::io;
use std::ops::{Deref, DerefMut};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A slot shared by all S1 and acceptance test processes for this user.
pub struct VmPermit(File);

impl VmPermit {
    pub fn acquire() -> io::Result<Self> {
        let limit = std::env::var("VMLAB_HV_TEST_VM_LIMIT")
            .unwrap_or_else(|_| "2".into())
            .parse::<usize>()
            .map_err(io::Error::other)?;
        if limit == 0 {
            return Err(io::Error::other("VMLAB_HV_TEST_VM_LIMIT must be positive"));
        }
        // SAFETY: geteuid has no preconditions and only reads the effective UID.
        let uid = unsafe { libc::geteuid() };
        let directory = std::env::temp_dir().join(format!("vmlab-hv-vm-slots-{uid}"));
        std::fs::create_dir_all(&directory)?;
        Self::acquire_in(&directory, limit, Duration::from_secs(600))
    }

    fn acquire_in(directory: &Path, limit: usize, timeout: Duration) -> io::Result<Self> {
        let deadline = Instant::now() + timeout;
        loop {
            for slot in 0..limit {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(directory.join(format!("{slot}.lock")))?;
                match file.try_lock() {
                    Ok(()) => return Ok(Self(file)),
                    Err(std::fs::TryLockError::WouldBlock) => {}
                    Err(std::fs::TryLockError::Error(error)) => return Err(error),
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "waiting for a test VM slot",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for VmPermit {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Kill and reap the child and its descendants on panic, timeout, or normal drop.
pub struct ManagedChild {
    child: Child,
    cancel: Option<mpsc::Sender<()>>,
    watchdog: Option<std::thread::JoinHandle<()>>,
    _permit: Option<VmPermit>,
}

impl ManagedChild {
    pub fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.cancel_watchdog();
        }
        Ok(status)
    }

    pub fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        let status = self.child.wait()?;
        self.cancel_watchdog();
        Ok(status)
    }

    fn cancel_watchdog(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }

    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let permit = VmPermit::acquire()?;
        Self::start(command, Some(permit), Self::timeout()?)
    }

    pub fn helper(command: &mut Command) -> io::Result<Self> {
        Self::start(command, None, Self::timeout()?)
    }

    fn timeout() -> io::Result<Duration> {
        let seconds = std::env::var("VMLAB_HV_TEST_PROCESS_TIMEOUT_SECS")
            .unwrap_or_else(|_| "600".into())
            .parse::<u64>()
            .map_err(io::Error::other)?;
        if seconds == 0 {
            return Err(io::Error::other("test process timeout must be positive"));
        }
        Ok(Duration::from_secs(seconds))
    }

    fn start(
        command: &mut Command,
        permit: Option<VmPermit>,
        timeout: Duration,
    ) -> io::Result<Self> {
        // SAFETY: the pre-exec closure calls only async-signal-safe syscalls.
        // A killed test runner must not leave a VM running without its guard.
        unsafe {
            let parent = libc::getpid();
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::_exit(127);
                }
                Ok(())
            });
        }
        let child = command.process_group(0).spawn()?;
        let pid = child.id();
        let (cancel, receiver) = mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            if receiver.recv_timeout(timeout) == Err(mpsc::RecvTimeoutError::Timeout) {
                eprintln!("test child {pid} exceeded {timeout:?}; killing its process group");
                kill_group(pid);
            }
        });
        Ok(Self {
            child,
            cancel: Some(cancel),
            watchdog: Some(watchdog),
            _permit: permit,
        })
    }
}

fn kill_group(pid: u32) {
    // SAFETY: every managed child creates its own process group with PGID=PID.
    // The negative PID targets only that owned group, never the test runner.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

impl Deref for ManagedChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}

impl DerefMut for ManagedChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        self.cancel_watchdog();
        kill_group(self.child.id());
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_exclusive_until_released() {
        let directory = tempfile::tempdir().unwrap();
        let acquire = || VmPermit::acquire_in(directory.path(), 2, Duration::from_millis(30));
        let first = acquire().unwrap();
        let second = acquire().unwrap();
        assert_eq!(acquire().err().unwrap().kind(), io::ErrorKind::TimedOut);
        drop(first);
        let third = acquire().unwrap();
        drop((second, third));
    }

    #[test]
    fn slots_are_shared_across_test_processes() {
        const PROBE: &str = "VMLAB_HV_SLOT_PROBE";
        if let Some(directory) = std::env::var_os(PROBE) {
            let error = VmPermit::acquire_in(Path::new(&directory), 2, Duration::from_millis(30))
                .err()
                .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let first = VmPermit::acquire_in(directory.path(), 2, Duration::ZERO).unwrap();
        let second = VmPermit::acquire_in(directory.path(), 2, Duration::ZERO).unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process::tests::slots_are_shared_across_test_processes",
                "--nocapture",
            ])
            .env(PROBE, directory.path())
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        drop((first, second));
    }

    #[test]
    fn panic_kills_and_reaps_a_child() {
        let mut pid = 0;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let child = ManagedChild::helper(Command::new("sleep").arg("600")).unwrap();
            pid = child.id();
            panic!("injected startup failure");
        }));
        assert!(result.is_err());
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn watchdog_kills_a_stuck_child() {
        let mut child = ManagedChild::start(
            Command::new("sleep").arg("600"),
            None,
            Duration::from_millis(30),
        )
        .unwrap();
        assert!(!child.wait().unwrap().success());
    }

    #[test]
    fn abrupt_runner_exit_does_not_leave_a_child_running() {
        const PROBE: &str = "VMLAB_HV_PARENT_DEATH_PROBE";
        if let Some(path) = std::env::var_os(PROBE) {
            let child = ManagedChild::helper(Command::new("sleep").arg("600")).unwrap();
            std::fs::write(path, child.id().to_string()).unwrap();
            // Bypass Drop to model a test runner killed by an outer timeout.
            std::process::exit(0);
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pid");
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process::tests::abrupt_runner_exit_does_not_leave_a_child_running",
            ])
            .env(PROBE, &path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let pid = std::fs::read_to_string(path).unwrap();
        let status = format!("/proc/{pid}/status");
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Ok(status) = std::fs::read_to_string(&status) {
            if status
                .lines()
                .any(|line| line.starts_with("State:") && line.contains('Z'))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "child survived its parent: {status}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
