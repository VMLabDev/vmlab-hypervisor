//! The control-socket side: mapping protocol commands onto a machine and
//! serving the socket with the protocol crate's server.

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;

use vmlab_hv_proto::control::{Capability, Command, ErrorCode, Outcome, RunState};
use vmlab_hv_proto::server::Server;

use crate::error::VmmError;
use crate::vm::VmHandle;

/// Maps control [`Command`]s to the machine. Protocol mechanics (handshake,
/// framing, reply ids) live in `vmlab-hv-proto`'s server; this is the VMM's
/// policy.
pub struct ControlHandler {
    vm: VmHandle,
}

impl ControlHandler {
    pub fn new(vm: VmHandle) -> Self {
        ControlHandler { vm }
    }

    pub fn handle(&self, command: Command) -> Outcome {
        match command {
            Command::Status => Outcome::Ok {
                status: Some(self.vm.status()),
            },
            Command::Resume => match self.vm.resume() {
                Ok(state) => Outcome::Ok {
                    status: Some(state),
                },
                Err(message) => Outcome::Error {
                    code: ErrorCode::InvalidState,
                    message,
                },
            },
            Command::Pause => match self.vm.pause() {
                Ok(state) => Outcome::Ok {
                    status: Some(state),
                },
                Err(message) => Outcome::Error {
                    code: ErrorCode::InvalidState,
                    message,
                },
            },
            Command::Powerdown => match self.vm.powerdown() {
                Ok(state) => Outcome::Ok {
                    status: Some(state),
                },
                Err(message) => Outcome::Error {
                    code: ErrorCode::InvalidState,
                    message,
                },
            },
            Command::Quit => {
                self.vm.quit();
                Outcome::Ok {
                    status: Some(RunState::ShuttingDown),
                }
            }
            Command::SnapshotSave { name } => {
                snapshot_outcome(self.vm.status(), crate::snapshot::save(&self.vm, &name))
            }
            Command::SnapshotLoad { name } => {
                snapshot_outcome(self.vm.status(), crate::snapshot::load(&self.vm, &name))
            }
            Command::SnapshotDelete { name } => {
                snapshot_outcome(self.vm.status(), crate::snapshot::delete(&self.vm, &name))
            }
        }
    }
}

/// Map a snapshot operation onto a protocol outcome.
fn snapshot_outcome(
    status: RunState,
    result: Result<(), crate::snapshot::SnapshotError>,
) -> Outcome {
    match result {
        Ok(()) => Outcome::Ok {
            status: Some(status),
        },
        Err(e) => Outcome::Error {
            code: match &e {
                crate::snapshot::SnapshotError::Unsupported(_) => ErrorCode::Unsupported,
                crate::snapshot::SnapshotError::NotFound(_) => ErrorCode::SnapshotNotFound,
                crate::snapshot::SnapshotError::Incompatible(_) => ErrorCode::SnapshotIncompatible,
                crate::snapshot::SnapshotError::Internal(_) => ErrorCode::Internal,
            },
            message: e.to_string(),
        },
    }
}

/// Bind the control socket and start serving it on its own thread. Machine
/// events are broadcast to every connected client through the returned
/// wiring; the caller keeps the machine handle.
pub fn serve(path: &Path, vm: &VmHandle) -> Result<std::thread::JoinHandle<()>, VmmError> {
    if path.exists() {
        return Err(VmmError::Control(format!(
            "control socket path {} already exists",
            path.display()
        )));
    }
    let listener = UnixListener::bind(path)
        .map_err(|e| VmmError::Control(format!("binding {}: {e}", path.display())))?;

    let handler = ControlHandler::new(vm.clone());
    let mut capabilities = vec![Capability::Snapshots];
    if vm.shared().vga.lock().is_some() {
        capabilities.extend([Capability::Display, Capability::Input]);
    }
    let server: Server<_> = Server::new(env!("CARGO_PKG_VERSION"), capabilities, move |command| {
        handler.handle(command)
    });
    let broadcaster = server.clone();
    vm.set_event_sink(Arc::new(move |event| broadcaster.broadcast(event)));

    std::thread::Builder::new()
        .name("control-serve".to_string())
        .spawn(move || {
            if let Err(e) = server.serve(listener) {
                eprintln!("control: accept loop ended: {e}");
            }
        })
        .map_err(|e| VmmError::Control(format!("spawning control thread: {e}")))
}
