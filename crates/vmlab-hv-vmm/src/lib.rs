//! The KVM virtual machine: guest memory, vCPU threads, the I/O buses, the
//! machine model, the control server and snapshot orchestration.

pub mod acpi;
pub mod builder;
pub mod bus;
pub mod control;
pub mod e820;
pub mod error;
pub mod factory;
pub mod kvm;
pub mod layout;
pub mod loader;
pub mod memory;
pub mod pci;
pub mod pm;
pub mod vcpu;
pub mod vm;

#[cfg(test)]
mod kvm_tests;

pub use builder::{build, validate};
pub use error::{DeviceUnavailable, VmmError};
pub use factory::{DeviceFactory, NoDevices};
pub use vm::{ExitReason, Vm, VmHandle};
