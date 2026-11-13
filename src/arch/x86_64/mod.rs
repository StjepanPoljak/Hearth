mod acpi;
mod power;
mod vcpu;
mod init;
mod io;
mod mmio;
mod i8042;
mod vga;

pub use init::{arch_init, arch_pre_vcpu_init};
pub use acpi::MAX_CPUS;
pub use io::{arch_update_irq};
