//! The control-socket side: mapping protocol commands onto a machine and
//! serving the socket with the protocol crate's server.

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;

use vmlab_hv_proto::control::{Command, ErrorCode, Outcome, RunState};
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
            Command::SnapshotSave { .. }
            | Command::SnapshotLoad { .. }
            | Command::SnapshotDelete { .. } => Outcome::Error {
                code: ErrorCode::Unsupported,
                message: "snapshots are not implemented in this build".to_string(),
            },
        }
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
    // Phase 1 command surface: nothing to advertise until snapshots land
    // (they answer `Unsupported` in this build).
    let server: Server<_> = Server::new(
        env!("CARGO_PKG_VERSION"),
        Vec::new(), // Capability::Snapshots, once implemented
        move |command| handler.handle(command),
    );
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

