//! vCPU threads: the KVM run loop and the machinery to interrupt it.

use std::sync::Arc;
use std::thread::JoinHandle;

use kvm_ioctls::{VcpuExit, VcpuFd};
use parking_lot::Mutex;

use crate::vm::VmShared;

/// The signal used to kick a vCPU out of KVM_RUN. A no-op handler without
/// `SA_RESTART` makes the ioctl return `EINTR`; the run loop then re-checks
/// the machine state. The registry in [`VmShared`] holds each thread's id;
/// kicking goes through it, so thread handles need no tid of their own.
const KICK_SIGNAL: libc::c_int = libc::SIGUSR1;

extern "C" fn kick_handler(_sig: libc::c_int) {}

/// Install the kick signal handler once, before the first vCPU thread runs.
pub fn install_kick_handler() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: sigaction(2) on SIGUSR1 with a handler that does nothing
        // and an empty mask, so nothing else is blocked. Flags exclude
        // SA_RESTART on purpose: blocked ioctls must fail with EINTR.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = kick_handler as *const () as usize;
            action.sa_flags = 0;
            if libc::sigaction(KICK_SIGNAL, &action, std::ptr::null_mut()) != 0 {
                panic!(
                    "sigaction(SIGUSR1) failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    });
}

/// One running vCPU: its thread. The kick handle is the thread's id, held in
/// the shared registry from thread start to thread end; the KVM fd itself is
/// shared with the VMM so snapshot orchestration can read and write vCPU
/// state while the owning thread is parked.
pub struct VcpuThread {
    handle: Option<JoinHandle<()>>,
}

impl VcpuThread {
    /// Spawn the thread that owns `vcpu` and runs it against `shared`.
    /// The fd sits in a mutex shared with the snapshot path: this thread
    /// holds it across KVM_RUN only, so a snapshot can take it once the
    /// thread parks.
    pub fn spawn(id: u8, vcpu: Arc<Mutex<VcpuFd>>, shared: Arc<VmShared>) -> std::io::Result<Self> {
        install_kick_handler();
        let handle = std::thread::Builder::new()
            .name(format!("vcpu-{id}"))
            .spawn(move || {
                // SAFETY: pthread_self(3) reads the calling thread's own id.
                let tid = unsafe { libc::pthread_self() };
                shared.register_vcpu(tid);
                run_vcpu(id, vcpu, &shared);
                shared.unregister_vcpu(tid);
            })?;
        Ok(VcpuThread {
            handle: Some(handle),
        })
    }

    pub fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The KVM_RUN loop of one vCPU. The fd is locked across the ioctl only, so
/// the snapshot path can hold it while this thread is parked.
fn run_vcpu(id: u8, vcpu: Arc<Mutex<VcpuFd>>, shared: &Arc<VmShared>) {
    loop {
        if !shared.should_run() {
            if shared.exited() {
                return;
            }
            shared.park();
            continue;
        }
        // The exit borrows the fd's kvm_run area, so the guard lives through
        // the dispatch; nothing else takes this mutex while the thread runs
        // (the snapshot path waits for the thread to park first).
        let mut guard = vcpu.lock();
        guard.set_kvm_immediate_exit(0);
        let run_ptr = guard.get_kvm_run() as *const kvm_bindings::kvm_run;
        let permit = shared.enter_kvm();
        let result = guard.run();
        drop(permit);
        let width = if matches!(&result, Ok(VcpuExit::IoIn(..) | VcpuExit::IoOut(..))) {
            // SAFETY: KVM_EXIT_IO selects this union member. run_ptr names
            // the stable vCPU mmap; size is metadata disjoint from the I/O
            // payload slice borrowed by VcpuExit.
            unsafe { (*run_ptr).__bindgen_anon_1.io.size as usize }
        } else { 1 };
        if !matches!(width, 1 | 2 | 4) {
            shared.request_exit(crate::vm::ExitReason::Error(format!("invalid KVM I/O width {width}")));
            return;
        }
        match result {
            Ok(VcpuExit::IoIn(port, data)) => {
                for value in data.chunks_mut(width) { shared.io_read(port, value); }
            }
            Ok(VcpuExit::IoOut(port, data)) => {
                for value in data.chunks(width) { shared.io_write(port, value); }
                if port == 0xb2 && shared.take_smi() {
                    // Complete the deferred OUT before SMM saves RIP. Without
                    // this completion, RSM can return to the same APMC write.
                    guard.set_kvm_immediate_exit(1);
                    let permit = shared.enter_kvm();
                    let completion = guard.run();
                    drop(permit);
                    if !matches!(completion, Err(ref e) if e.errno() == libc::EINTR) {
                        shared.request_exit(crate::vm::ExitReason::Error(format!(
                            "completing APMC output before SMI: {completion:?}"
                        )));
                        return;
                    }
                    guard.set_kvm_immediate_exit(0);
                    if let Err(e) = guard.smi() {
                        shared.request_exit(crate::vm::ExitReason::Error(format!("KVM_SMI: {e}")));
                        return;
                    }
                }
            }
            Ok(VcpuExit::MmioRead(addr, data)) => shared.mmio_read(addr, data),
            Ok(VcpuExit::MmioWrite(addr, data)) => shared.mmio_write(addr, data),
            Ok(VcpuExit::Hlt) => {
                // With the in-kernel irqchip the vCPU wakes on the next
                // interrupt; re-enter and let the state check handle pauses.
            }
            Ok(VcpuExit::IrqWindowOpen) => {}
            Ok(VcpuExit::Shutdown) => {
                // Triple fault: the guest reset. `reboot=t` and `panic=-1`
                // both land here.
                shared.request_exit(crate::vm::ExitReason::Shutdown(
                    vmlab_hv_proto::control::ShutdownReason::GuestReset,
                ));
                return;
            }
            Ok(exit) => {
                shared.request_exit(crate::vm::ExitReason::Error(format!(
                    "vcpu {id}: unhandled KVM exit {exit:?}"
                )));
                return;
            }
            Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => {
                // EINTR: kicked, re-check the machine state. EAGAIN: the
                // vCPU is not runnable yet — an application processor parked
                // before the kernel's INIT-SIPI-SIPI wakes it — retry.
            }
            Err(e) => {
                shared.request_exit(crate::vm::ExitReason::Error(format!(
                    "vcpu {id}: KVM_RUN: {e}"
                )));
                return;
            }
        }
    }
}
