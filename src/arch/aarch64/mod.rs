mod vcpu;
mod init;
mod mmio;
mod io;

pub use init::{ arch_init, arch_pre_vcpu_init };
pub use io::{ arch_update_irq };

/* Most vcpus a VM can have: one, so far. */
pub const MAX_CPUS: u32 = 1;
