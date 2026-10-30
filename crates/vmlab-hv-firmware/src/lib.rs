//! q35 firmware inputs and the fw_cfg port-I/O device.

pub mod acpi;
pub mod fw_cfg;
pub mod loader;
pub mod platform;
pub mod smbios;

pub use acpi::{PciDevice, Platform};
pub use fw_cfg::FwCfg;
pub use platform::FirmwareOptions;
