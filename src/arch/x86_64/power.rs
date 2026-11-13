use anyhow::Result;
use std::sync::Arc;
use crate::{ IODevice, IO, VmControl, VmExit, KVM_EXIT_IO_OUT };

/* ACPI's sleep control and status registers, which a hardware-reduced
 * platform has in place of PM1 blocks; the FADT points at them. Linux
 * powers off by writing S5's SLP_TYP, which the DSDT's \_S5_ gives,
 * with SLP_EN, to the control register. Any other sleep state is
 * ignored: the DSDT offers none. */
pub const SLEEP_CONTROL_PORT: u16 = 0x600;
pub const SLEEP_STATUS_PORT: u16 = 0x601;
/* SLP_TYP for S5; the value is Hearth's to choose. */
pub const S5_SLEEP_TYPE: u8 = 5;

const SLP_EN: u8 = 1 << 5;
const SLP_TYP_SHIFT: u8 = 2;
const SLP_TYP_MASK: u8 = 0x7;

pub struct AcpiSleep {
    control: Arc<VmControl>
}

impl AcpiSleep {
    pub fn new(control: Arc<VmControl>) -> Self {
        AcpiSleep { control }
    }
}

impl IODevice for AcpiSleep {
    /* The status register reads 0: WAK_STS never sets, as nothing wakes
     * the VM. Its writes, which clear WAK_STS, change nothing. */
    fn handle(&mut self, io: &IO, base: *mut u8) -> Result<()> {
        let data = unsafe {
            std::slice::from_raw_parts_mut(base.add(io.data_offset as usize), io.size as usize * io.count as usize)
        };
        if io.direction as u32 == KVM_EXIT_IO_OUT {
            let value = data[0];
            if io.port == SLEEP_CONTROL_PORT && value & SLP_EN != 0
                && (value >> SLP_TYP_SHIFT) & SLP_TYP_MASK == S5_SLEEP_TYPE {
                self.control.stop(VmExit::PoweredOff);
            }
        } else {
            data.fill(0);
        }
        Ok(())
    }
}
