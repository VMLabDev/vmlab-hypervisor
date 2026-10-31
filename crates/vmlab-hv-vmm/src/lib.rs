//! The KVM virtual machine: guest memory, vCPU threads, the I/O buses, the
//! machine model, the control server and snapshot orchestration.

pub mod acpi;
pub mod audio;
pub mod builder;
pub mod bus;
pub mod chipset;
mod clock;
pub mod control;
mod display;
pub mod e820;
pub mod error;
pub mod factory;
mod gpu;
pub mod hyperv;
pub mod kvm;
pub mod layout;
pub mod loader;
pub mod memory;
pub mod pci;
pub mod pm;
pub mod q35;
mod reset;
pub mod smm;
pub mod snapshot;
pub mod vcpu;
mod vfio;
pub mod vm;

#[cfg(test)]
mod kvm_tests;
#[cfg(test)]
mod snapshot_tests;

pub use builder::{build, validate};
pub use error::{DeviceUnavailable, VmmError};
pub use factory::{DeviceFactory, NoDevices};
pub use vm::{ExitReason, Vm, VmHandle};
