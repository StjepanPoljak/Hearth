use anyhow::{ bail, Result };
use std::sync::Arc;
use crate::BumpAllocator;

/* Shared, so a device reading guest RAM from a frontend thread keeps
 * its region mapped after the VM is gone. */
pub(crate) type GuestMemory = Vec<Arc<MemRegion>>;

pub struct MemRegion {
    pub mem_ptr: *mut libc::c_void,
    pub mem_size: usize,
    pub guest_phys_addr: u64,
    /* Parts of the region the VMM fills before the guest runs; see
     * add_bump_allocator. */
    pub allocators: Vec<BumpAllocator>
}

impl MemRegion {
    /* Adds an allocator handing out gpa_start..gpa_end, which must lie in
     * this region and clear of its other allocators. Returns its index in
     * allocators. */
    pub fn add_bump_allocator(&mut self, gpa_start: u64, gpa_end: u64) -> Result<usize> {
        let region_end = self.guest_phys_addr + self.mem_size as u64;
        if gpa_start > gpa_end || gpa_start < self.guest_phys_addr || gpa_end > region_end {
            bail!("Allocator {:#x}..{:#x} is not inside region {:#x}..{:#x}.",
                  gpa_start, gpa_end, self.guest_phys_addr, region_end);
        }
        if let Some(other) = self.allocators.iter().find(|a| gpa_start < a.gpa_end() && a.gpa_start() < gpa_end) {
            bail!("Allocator {:#x}..{:#x} overlaps {:#x}..{:#x}.",
                  gpa_start, gpa_end, other.gpa_start(), other.gpa_end());
        }
        let host_start = unsafe {
            (self.mem_ptr as *mut u8).add((gpa_start - self.guest_phys_addr) as usize)
        };
        self.allocators.push(BumpAllocator::new(gpa_start, gpa_end, host_start));
        Ok(self.allocators.len() - 1)
    }
}

/* The mapping lives as long as the last Arc to the region, and the
 * guest writes it concurrently anyway; accesses go through volatile reads and writes. */
unsafe impl Send for MemRegion {}
unsafe impl Sync for MemRegion {}

impl Drop for MemRegion {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.mem_ptr, self.mem_size); }
    }
}

pub(crate) fn gpa_to_host(mem: &GuestMemory, gpa: u64, len: usize) -> Option<*mut u8> {
    mem.iter().find_map(|r| {
        let off = usize::try_from(gpa.checked_sub(r.guest_phys_addr)?).ok()?;
        (off.checked_add(len)? <= r.mem_size).then(|| unsafe { (r.mem_ptr as *mut u8).add(off) })
    })
}

pub(crate) fn read_guest_ptr<T>(ptr: *const u8) -> T {
    unsafe {
        std::ptr::read_volatile(ptr.cast::<T>())
    }
}

pub(crate) fn write_guest_ptr<T>(ptr: *mut u8, data: T) {
    unsafe {
        std::ptr::write_volatile(ptr.cast::<T>(), data)
    }
}
