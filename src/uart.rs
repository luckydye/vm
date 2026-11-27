//! The serial console: a PL011 on arm64, a 16550 on x86-64.

#[cfg(target_arch = "aarch64")]
mod pl011;
#[cfg(target_arch = "aarch64")]
pub use pl011::Uart;
#[cfg(target_arch = "x86_64")]
mod ns16550;
#[cfg(target_arch = "x86_64")]
pub use ns16550::Uart;
