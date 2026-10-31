//! Control protocol messages.

use serde::{Deserialize, Serialize};

/// What a client sends: the handshake first, then requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello { proto_version: u32 },
    Request { id: u64, command: Command },
}

/// What the server sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Hello {
        proto_version: u32,
        hv_version: String,
        capabilities: Vec<Capability>,
    },
    Reply {
        id: u64,
        #[serde(flatten)]
        outcome: Outcome,
    },
    Event {
        #[serde(flatten)]
        event: Event,
    },
    /// A protocol-level failure that is not a reply to any request: a
    /// rejected handshake. Sent without an id (there is no request id to
    /// echo), then the connection is closed.
    Error { code: ErrorCode, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Status,
    Resume,
    Pause,
    /// ACPI power button press; the guest decides what to do.
    Powerdown,
    /// Stop the machine immediately and exit the process.
    Quit,
    /// Hot-plug a device. The fields carry what vmlab passes QEMU's
    /// `usb-host` today: `driver` selects the device model, `bus` names the
    /// controller root hub (`usbx.0`), and `hostbus`/`hostaddr`/`hostport`
    /// pin the host device (address plus physical port path).
    DeviceAdd {
        id: String,
        driver: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bus: Option<String>,
        hostbus: u32,
        hostaddr: u32,
        hostport: String,
    },
    /// Unplug the device with id `id`; the server confirms with a
    /// `device_deleted` event once the device is back on the host.
    DeviceDel { id: String },
    SnapshotSave {
        name: String,
    },
    SnapshotLoad {
        name: String,
    },
    SnapshotDelete {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Ok {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<RunState>,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Built, vCPUs not yet started (`--paused`) or paused by command.
    Paused,
    Running,
    /// Guest entered S3.
    Suspended,
    ShuttingDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A request arrived before the handshake.
    NoHandshake,
    VersionMismatch,
    /// The command is not valid in the current run state.
    InvalidState,
    /// The machine lacks the capability the command needs.
    Unsupported,
    /// No hot-plugged device carries the id the command named.
    DeviceNotFound,
    SnapshotNotFound,
    /// The snapshot's device state has an incompatible major version.
    SnapshotIncompatible,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Snapshots,
    Display,
    Input,
    Hotplug,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Shutdown { reason: ShutdownReason },
    Reset,
    Stopped,
    Resumed,
    Suspend,
    Wakeup,
    /// A hot-plugged device left the machine: after `device_del`, or when
    /// the host device itself was unplugged. QEMU's DEVICE_DELETED.
    DeviceDeleted { id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    GuestShutdown,
    GuestReset,
    HostRequest,
    GuestPanic,
}
