//! Host facts returned as JSON by `vmlab-hv --probe`, without running a guest.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFacts {
    pub kvm: bool,
    /// The unscaled KVM vCPU frequency. Absent when KVM or a stable TSC is unavailable.
    pub tsc_khz: Option<u32>,
    /// CPUID.80000007H:EDX[8] on the host CPU.
    pub invariant_tsc: bool,
    /// KVM_GET_TSC_KHZ succeeded with a nonzero frequency.
    pub stable_tsc: bool,
}
