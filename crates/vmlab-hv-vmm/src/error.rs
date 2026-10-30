//! Errors returned by the VMM.

use thiserror::Error;

/// Anything that can stop the VMM from building or running a machine.
#[derive(Debug, Error)]
pub enum VmmError {
    #[error("cannot open /dev/kvm: {0}")]
    KvmOpen(String),
    #[error("KVM does not support {0}")]
    KvmCap(&'static str),
    #[error("kvm ioctl {0}: {1}")]
    KvmIoctl(&'static str, String),
    #[error("guest memory: {0}")]
    Memory(String),
    #[error("machine description: {0}")]
    InvalidMachine(String),
    #[error("kernel: {0}")]
    Kernel(String),
    #[error("acpi: {0}")]
    Acpi(String),
    #[error("device model `{device}` is not available: {reason}")]
    DeviceUnavailable {
        device: &'static str,
        reason: String,
    },
    #[error("control socket: {0}")]
    Control(String),
    #[error("{0}")]
    Other(String),
}

impl VmmError {
    /// Wrap a `kvm-ioctls` error with the ioctl that failed.
    pub fn ioctl(op: &'static str, e: impl std::fmt::Display) -> Self {
        VmmError::KvmIoctl(op, e.to_string())
    }
}

/// A device the factory could not produce, before it is turned into a
/// [`VmmError::DeviceUnavailable`] naming the machine part that needed it.
#[derive(Debug, Error)]
#[error("{device}: {reason}")]
pub struct DeviceUnavailable {
    pub device: &'static str,
    pub reason: String,
}

impl DeviceUnavailable {
    pub fn new(device: &'static str, reason: impl Into<String>) -> Self {
        DeviceUnavailable {
            device,
            reason: reason.into(),
        }
    }
}
