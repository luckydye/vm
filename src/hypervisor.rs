//! The host's hypervisor: Hypervisor.framework on macOS, KVM on Linux. Both
//! offer the same `Vm`, `IrqLine` and, on arm64, `gic_sizes`.

#[cfg(target_os = "macos")]
mod hvf;
#[cfg(target_os = "macos")]
pub use hvf::*;
#[cfg(target_os = "linux")]
mod kvm;
#[cfg(target_os = "linux")]
pub use kvm::*;
