//! The guest machine: memory layout, device addresses and interrupt lines, and
//! how the kernel is loaded.

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::*;
#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

/// Where the boot CPU starts and what it finds in its first argument register.
pub struct Boot {
    pub entry: u64,
    pub arg: u64,
}

pub fn checked_end(start: u64, len: usize, name: &str) -> u64 {
    start
        .checked_add(len as u64)
        .unwrap_or_else(|| panic!("{name} address range overflows"))
}
