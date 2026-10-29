//! The guest's network: vmnet through vmnet-helper on macOS, passt on Linux.
//! Both give the guest NAT with DHCP and DNS, and need no privileges.

#[cfg(target_os = "linux")]
mod passt;
#[cfg(target_os = "linux")]
pub use passt::*;
#[cfg(target_os = "macos")]
mod vmnet;
#[cfg(target_os = "macos")]
pub use vmnet::*;
