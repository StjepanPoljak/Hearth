use anyhow::{ anyhow, bail, Result };
use libc::{ _IOW, _IO };
use std::collections::{ BTreeMap, HashMap };
use std::fs::{ File, OpenOptions };
use std::os::unix::io::AsRawFd;
use std::sync::{ Arc, Mutex };
use std::sync::atomic::{ AtomicBool, Ordering };
use crate::kvm_bindings::*;
use crate::{ install_kick_handler, kick_signal, mmap_check, read_file, read_guest_ptr, syscall_check, write_guest_ptr };
use crate::{ Display, GuestMemory, Keyboard, MemRegion, VmConfig, VCPU, KVMIO, PAGE_SIZE };

pub(crate) type MMIO = kvm_run__bindgen_ty_1__bindgen_ty_6;
pub(crate) type IO = kvm_run__bindgen_ty_1__bindgen_ty_4;
/* By base address: each device's size and the device. */
type MMIODevices = BTreeMap::<u64, (u64, Arc<Mutex<dyn MMIODevice>>)>;
type IODevices = HashMap::<u64, Arc<Mutex<dyn IODevice>>>;

pub trait MMIODevice: Send {
    /* `offset` is the access's address relative to the device's base. */
    fn handle(&mut self, offset: u64, mmio: &mut MMIO, mem: &GuestMemory) -> Result<()>;
    /* Picks up work that arrived from outside the guest, see VmControl::kick(). */
    fn poll(&mut self, _mem: &GuestMemory) -> Result<()> { Ok(()) }
}

pub trait IODevice: Send {
    fn handle(&mut self, io: &IO, base: *mut u8) -> Result<()>;
}

/* Why the VM stopped, when nothing went wrong. */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmExit {
    /* A vcpu ran HLT with interrupts off (x86). */
    Halted,
    /* KVM_EXIT_SHUTDOWN: a triple fault, which is also how x86 guests
     * without another way reset. */
    TripleFault,
    /* PSCI SYSTEM_OFF (aarch64). */
    PoweredOff,
    /* The guest asked for a reset: PSCI SYSTEM_RESET, or the i8042's
     * reset line. Hearth has no reset, so it stops. */
    Reset,
    /* Ctrl-A x on the terminal. */
    Quit,
    /* The terminal the serial console was on went away. */
    TerminalClosed
}

impl VmExit {
    /* What to tell the user; None when they stopped it themselves. */
    pub(crate) fn message(&self) -> Option<&'static str> {
        match self {
            VmExit::Halted => Some("Guest halted."),
            VmExit::TripleFault => Some("Guest triple-faulted (KVM_EXIT_SHUTDOWN): a reset or a crash."),
            VmExit::PoweredOff => Some("Guest powered off."),
            VmExit::Reset => Some("Guest asked for a reset, which Hearth doesn't do; stopped."),
            VmExit::Quit => None,
            VmExit::TerminalClosed => Some("Terminal closed; stopped.")
        }
    }
}

/* Per-VM state shared by its vcpu threads and device threads. */
pub struct VmControl {
    should_stop: AtomicBool,
    /* The first reason given to stop(). */
    exit: Mutex<Option<VmExit>>,
    devices_pending: AtomicBool,
    /* Threads currently in VCPU::run(), the targets of kick_signal(). */
    vcpu_threads: Mutex<Vec<libc::pthread_t>>
}

impl VmControl {
    fn new() -> Self {
        Self {
            should_stop: AtomicBool::new(false),
            exit: Mutex::new(None),
            devices_pending: AtomicBool::new(false),
            vcpu_threads: Mutex::new(vec![])
        }
    }

    /* Makes every vcpu thread leave its run loop; the VM's exit is the
     * first reason given. */
    pub fn stop(&self, exit: VmExit) {
        self.exit.lock().unwrap().get_or_insert(exit);
        self.halt_vcpus();
    }

    /* Stops the vcpus without a reason, when an error will say why. */
    fn halt_vcpus(&self) {
        self.should_stop.store(true, Ordering::SeqCst);
        for tid in self.vcpu_threads.lock().unwrap().iter() {
            unsafe { libc::pthread_kill(*tid, kick_signal()); }
        }
    }

    /* Called from device threads: makes a vcpu thread poll the MMIO devices. */
    pub fn kick(&self) {
        self.devices_pending.store(true, Ordering::SeqCst);
        if let Some(tid) = self.vcpu_threads.lock().unwrap().first() {
            unsafe { libc::pthread_kill(*tid, kick_signal()); }
        }
    }

    pub(crate) fn should_stop(&self) -> bool {
        self.should_stop.load(Ordering::SeqCst)
    }

    pub(crate) fn take_devices_pending(&self) -> bool {
        self.devices_pending.swap(false, Ordering::SeqCst)
    }

    /* Kicks only reach registered threads, and a thread is unregistered
     * under the same lock, so pthread_kill never sees a stale thread id. */
    pub(crate) fn register_vcpu_thread(&self) {
        self.vcpu_threads.lock().unwrap().push(unsafe { libc::pthread_self() });
    }

    pub(crate) fn unregister_vcpu_thread(&self) {
        let tid = unsafe { libc::pthread_self() };
        self.vcpu_threads.lock().unwrap().retain(|t| unsafe { libc::pthread_equal(*t, tid) } == 0);
    }
}

const KVM_CREATE_VM: libc::Ioctl = _IO(KVMIO, 0x01);
const KVM_CHECK_EXTENSION: libc::Ioctl = _IO(KVMIO, 0x03);
const KVM_CREATE_VCPU: libc::Ioctl = _IO(KVMIO, 0x41);
const KVM_GET_VCPU_MMAP_SIZE: libc::Ioctl = _IO(KVMIO, 0x04);

const KVM_SET_USER_MEMORY_REGION: libc::Ioctl = _IOW::<kvm_userspace_memory_region>(KVMIO, 0x46);
const KVM_GET_DIRTY_LOG: libc::Ioctl = _IOW::<kvm_dirty_log>(KVMIO, 0x42);
const KVM_REGISTER_COALESCED_MMIO: libc::Ioctl = _IOW::<kvm_coalesced_mmio_zone>(KVMIO, 0x67);

pub struct KvmDev {
    pub file: File,
    pub kvm_run_size: usize
}

impl KvmDev {
    pub(crate) fn new() -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .expect("Failed to open /dev/kvm");

        // 140904 ioctl(3</dev/kvm<char 10:232>>, 0xae04 /* KVM_GET_VCPU_MMAP_SIZE */, 0) = 12288
        let kvm_run_size = syscall_check("KVM_GET_VCPU_MMAP_SIZE", unsafe { libc::ioctl(file.as_raw_fd(), KVM_GET_VCPU_MMAP_SIZE, 0usize) })?;
        
        Ok(Self { file, kvm_run_size: kvm_run_size as usize })
    }

    pub(crate) fn create_vm(&self) -> Result<VM> {
        // 140900 ioctl(3</dev/kvm<char 10:232>>, 0xae01 /* KVM_CREATE_VM */, 0) = 9<anon_inode:kvm-vm>
        let vm_fd = syscall_check("KVM_CREATE_VM", unsafe { libc::ioctl(self.fd(), KVM_CREATE_VM, 0usize) })?;

        Ok(VM {
            fd: vm_fd,
            guest_memory: GuestMemory::new(),
            slots: vec![],
            next_slot: 0,
            mmio_devices: MMIODevices::new(),
            io_devices: IODevices::new(),
            display: None,
            keyboard: None,
            control: Arc::new(VmControl::new()) })
    }

    pub(crate) fn fd(&self) -> libc::c_int {
        self.file.as_raw_fd()
    }

    pub(crate) fn get_kvm_run_size(&self) -> usize {
        self.kvm_run_size
    }
}

pub struct VM {
    pub fd: libc::c_int,
    pub guest_memory: GuestMemory,
    /* The KVM slots mapping guest_memory: one per region, until
     * log_dirty_pages splits it. */
    slots: Vec<Slot>,
    next_slot: u32,
    pub mmio_devices: MMIODevices,
    pub io_devices: IODevices,
    /* Set by the devices that provide them, for frontends such as RFB. */
    pub display: Option<Arc<dyn Display>>,
    pub keyboard: Option<Arc<Mutex<dyn Keyboard>>>,
    pub control: Arc<VmControl>
}

struct Slot {
    id: u32,
    guest_phys_addr: u64,
    size: u64,
    userspace_addr: u64,
    logged: bool
}

/* Which pages of a memory slot the guest has written since the last
 * fetch. KVM write-protects the slot's pages and notes each one on its
 * first write (or, on Intel, the CPU logs it with PML). */
pub struct DirtyLog {
    vm_fd: libc::c_int,
    /* Of the first page; bit n is the page n pages after it. */
    pub gpa: u64,
    slot: u32,
    pages: usize
}

impl DirtyLog {
    /* A bit per page, set if the page was written; fetching clears the
     * log, so a write made after this returns is in the next one. */
    pub fn fetch(&self) -> Result<Vec<u64>> {
        /* KVM copies whole longs. */
        let mut bitmap = vec![0u64; self.pages.div_ceil(64)];
        let log = kvm_dirty_log {
            slot: self.slot,
            padding1: 0,
            __bindgen_anon_1: kvm_dirty_log__bindgen_ty_1 { dirty_bitmap: bitmap.as_mut_ptr().cast() }
        };
        syscall_check("KVM_GET_DIRTY_LOG", unsafe { libc::ioctl(self.vm_fd, KVM_GET_DIRTY_LOG, &log) })?;
        Ok(bitmap)
    }
}

impl Drop for DirtyLog {
    fn drop(&mut self) {
        unsafe { libc::close(self.vm_fd); }
    }
}

/* Writes KVM queued instead of exiting to us, see VM::coalesce_pio().
 * KVM appends at `last` and the reader frees entries by advancing
 * `first`; when the ring is full, a write exits as usual. The ring is
 * the VM's, so it has one reader, which must apply what is queued
 * before handling an exit for the same ports, or writes reorder. */
pub struct CoalescedRing {
    pub(crate) ring: *mut kvm_coalesced_mmio_ring
}

/* Only the reader touches `first`, and it is behind the device's lock. */
unsafe impl Send for CoalescedRing {}

impl CoalescedRing {
    const ENTRIES: u32 = ((PAGE_SIZE as usize - std::mem::size_of::<kvm_coalesced_mmio_ring>())
                          / std::mem::size_of::<kvm_coalesced_mmio>()) as u32;

    /* Calls write(port, data) for each queued write, oldest first. */
    pub fn drain(&self, mut write: impl FnMut(u64, &[u8])) {
        let first = unsafe { std::ptr::addr_of_mut!((*self.ring).first) };
        let last = unsafe { std::ptr::addr_of!((*self.ring).last) };
        let entries = unsafe { (*self.ring).__bindgen_anon_1.coalesced_mmio.as_ptr() };
        let mut i = read_guest_ptr::<u32>(first.cast());
        /* Entries up to `last` are complete once it is read. */
        let end = read_guest_ptr::<u32>(last.cast());
        std::sync::atomic::fence(Ordering::Acquire);
        while i != end {
            let entry = unsafe { std::ptr::read_volatile(entries.add(i as usize)) };
            write(entry.phys_addr, &entry.data[..(entry.len as usize).min(entry.data.len())]);
            i = (i + 1) % Self::ENTRIES;
        }
        /* KVM may reuse the entries once `first` passes them. */
        std::sync::atomic::fence(Ordering::Release);
        write_guest_ptr::<u32>(first.cast(), end);
    }
}

impl Drop for CoalescedRing {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ring.cast(), PAGE_SIZE as usize); }
    }
}

impl VM {
    pub(crate) fn create_vcpu(&self, id: u32, kvm_run_size: usize) -> Result<VCPU> {
        // 140904 ioctl(9<anon_inode:kvm-vm>, 0xae41 /* KVM_CREATE_VCPU */, 0) = 10<anon_inode:kvm-vcpu:0>
        let vcpu_fd = syscall_check("KVM_CREATE_VCPU", unsafe { libc::ioctl(self.fd, KVM_CREATE_VCPU, id as usize) })?;

        VCPU::new(id, vcpu_fd, kvm_run_size)
    }

    pub(crate) fn add_mem_region(&mut self, mem_size: usize, guest_phys_addr: u64) -> Result<usize> {
        // 140900 mmap(NULL, 1075838976, 0 /* PROT_NONE */, 0x22 /* MAP_PRIVATE|MAP_ANONYMOUS */, -1, 0) = 0x7768b3e00000
        // 140900 mmap(0x7768b3e00000, 1073741824, 0x3 /* PROT_READ|PROT_WRITE */, 0x32 /* MAP_PRIVATE|MAP_FIXED|MAP_ANONYMOUS */, -1, 0) = 0x7768b3e00000
        let mem_ptr = mmap_check("mmap of guest memory", unsafe {
            libc::mmap(std::ptr::null_mut(),
                       mem_size as usize,
                       libc::PROT_READ|libc::PROT_WRITE,
                       libc::MAP_PRIVATE|libc::MAP_ANONYMOUS,
                       -1,
                       0)
        })?;

        let id = self.next_slot;
        self.next_slot += 1;
        self.set_user_memory_region(id, 0, guest_phys_addr, mem_size as u64, mem_ptr as u64)?;
        self.slots.push(Slot { id, guest_phys_addr, size: mem_size as u64, userspace_addr: mem_ptr as u64, logged: false });
        self.guest_memory.push(Arc::new(MemRegion { mem_ptr, mem_size, guest_phys_addr, allocators: vec![] }));

        Ok(self.guest_memory.len() - 1)
    }

    /* A memory_size of 0 deletes the slot. */
    fn set_user_memory_region(&self, slot: u32, flags: u32, guest_phys_addr: u64,
                              memory_size: u64, userspace_addr: u64) -> Result<()> {
        // 140900 ioctl(9<anon_inode:kvm-vm>, 0x4020ae46 /* KVM_SET_USER_MEMORY_REGION */, {slot=0, flags=0, guest_phys_addr=0, memory_size=1073741824, userspace_addr=0x7768b3e00000}) = 0
        let region = kvm_userspace_memory_region { slot, flags, guest_phys_addr, memory_size, userspace_addr };
        syscall_check("KVM_SET_USER_MEMORY_REGION", unsafe { libc::ioctl(self.fd, KVM_SET_USER_MEMORY_REGION, &region) })?;
        Ok(())
    }

    /* Starts logging writes to the pages covering gpa..gpa + size. Logging
     * is per slot, and a slot cannot be resized, so the slot holding the
     * range is deleted and added again as up to three: before, logged,
     * after. The mapping stays one piece. The range must not overlap one
     * logged before; call before the vcpus run. */
    pub fn log_dirty_pages(&mut self, gpa: u64, size: usize) -> Result<DirtyLog> {
        let start = gpa & !(PAGE_SIZE - 1);
        let end = (gpa + size as u64).next_multiple_of(PAGE_SIZE);
        let idx = self.slots.iter()
            .position(|s| !s.logged && s.guest_phys_addr <= start && end <= s.guest_phys_addr + s.size)
            .ok_or(anyhow!("No unlogged memory slot covers {:#x}..{:#x}.", start, end))?;

        /* Its own fd, as frontends may fetch it after the VM is dropped.
         * Taken first, so a failure here leaves the slots untouched. */
        let vm_fd = syscall_check("dup of the VM fd", unsafe { libc::dup(self.fd) })?;

        let old = &self.slots[idx];
        let (old_id, old_gpa, old_size, old_host) = (old.id, old.guest_phys_addr, old.size, old.userspace_addr);
        let saved_next_slot = self.next_slot;
        let mut free_id = Some(old_id);
        let pieces: Vec<Slot> = [(old_gpa, start, false), (start, end, true), (end, old_gpa + old_size, false)]
            .into_iter()
            .filter(|(from, to, _)| from != to)
            .map(|(from, to, log)| {
                let id = free_id.take().unwrap_or_else(|| {
                    self.next_slot += 1;
                    self.next_slot - 1
                });
                Slot { id, guest_phys_addr: from, size: to - from,
                       userspace_addr: old_host + (from - old_gpa), logged: log }
            })
            .collect();

        if let Err(e) = self.replace_slot(old_id, old_gpa, old_host, &pieces) {
            /* Put KVM back as it was: drop the pieces added, re-add the
             * whole slot. Best effort, the first error is the one returned. */
            for piece in &pieces {
                let _ = self.set_user_memory_region(piece.id, 0, piece.guest_phys_addr, 0, piece.userspace_addr);
            }
            let _ = self.set_user_memory_region(old_id, 0, old_gpa, old_size, old_host);
            self.next_slot = saved_next_slot;
            unsafe { libc::close(vm_fd) };
            return Err(e);
        }

        let logged = pieces.iter().find(|s| s.logged).unwrap().id;
        self.slots.remove(idx);
        self.slots.extend(pieces);
        Ok(DirtyLog { vm_fd, gpa: start, slot: logged, pages: ((end - start) / PAGE_SIZE) as usize })
    }

    /* Deletes slot old_id and adds pieces in its place. On error, KVM may
     * hold some of the pieces; the caller undoes it. */
    fn replace_slot(&self, old_id: u32, old_gpa: u64, old_host: u64, pieces: &[Slot]) -> Result<()> {
        self.set_user_memory_region(old_id, 0, old_gpa, 0, old_host)?;
        for piece in pieces {
            let flags = if piece.logged { KVM_MEM_LOG_DIRTY_PAGES } else { 0 };
            self.set_user_memory_region(piece.id, flags, piece.guest_phys_addr, piece.size, piece.userspace_addr)?;
        }
        Ok(())
    }

    /* Makes KVM queue the guest's writes to ports..ports + len in the
     * returned ring rather than exit for each; reads still exit. None
     * if KVM can't. Once per VM: the ring is shared by every zone. It
     * is mapped through a vcpu, and on its own, so a frontend can keep
     * reading it after the vcpus are gone. */
    pub fn coalesce_pio(&self, vcpu: &VCPU, port: u16, len: u32) -> Result<Option<CoalescedRing>> {
        let supported = unsafe { libc::ioctl(self.fd, KVM_CHECK_EXTENSION, KVM_CAP_COALESCED_PIO as usize) };
        if supported <= 0 {
            return Ok(None);
        }
        let zone = kvm_coalesced_mmio_zone {
            addr: port as u64,
            size: len,
            __bindgen_anon_1: kvm_coalesced_mmio_zone__bindgen_ty_1 { pio: 1 }
        };
        syscall_check("KVM_REGISTER_COALESCED_MMIO", unsafe { libc::ioctl(self.fd, KVM_REGISTER_COALESCED_MMIO, &zone) })?;
        let ring = mmap_check("mmap of the coalesced MMIO ring", unsafe {
            libc::mmap(std::ptr::null_mut(),
                       PAGE_SIZE as usize,
                       libc::PROT_READ|libc::PROT_WRITE,
                       libc::MAP_SHARED,
                       vcpu.fd,
                       (KVM_COALESCED_MMIO_PAGE_OFFSET as u64 * PAGE_SIZE) as libc::off_t)
        })?;
        Ok(Some(CoalescedRing { ring: ring.cast() }))
    }

    pub(crate) fn load_data_to_memory(&self, mem_region_idx: usize, data: Vec<u8>, offset: u64) -> Result<usize> {
        let mem_region = self.guest_memory
            .get(mem_region_idx)
            .ok_or(anyhow!("Index exceeds memory region vector."))?;

        if (offset as usize) >= mem_region.mem_size {
            bail!("Offset exceeds memory region.");
        }

        if (offset as usize) + data.len() > mem_region.mem_size {
            bail!("Data ({:#x} {:#x}) would exceed memory region ({}).", offset, data.len(), mem_region.mem_size);
        }

        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(),
                                          (mem_region.mem_ptr as *mut u8).add(offset as usize),
                                          data.len());
        };

        Ok(data.len())
    }

    pub(crate) fn load_file_to_memory(&self, mem_region_idx: usize, path: &str, offset: u64) -> Result<usize> {
        let res = read_file(path)?;

        println!("Loading {:?} at {:#x}...\r", path, offset + self.guest_memory[mem_region_idx].guest_phys_addr);

        self.load_data_to_memory(mem_region_idx, res, offset)
    }

    /* The region to add allocators to or fill through them: only while
     * nothing else holds it, i.e. while the VM is being set up. Only the
     * x86 loader uses allocators so far. */
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    pub(crate) fn mem_region_mut(&mut self, mem_region_idx: usize) -> Result<&mut MemRegion> {
        let region = self.guest_memory
            .get_mut(mem_region_idx)
            .ok_or(anyhow!("Index exceeds memory region vector."))?;
        Arc::get_mut(region).ok_or(anyhow!("Memory region is already shared."))
    }

    pub(crate) fn load_linux(&mut self, vcpu: &mut VCPU, config: &VmConfig) -> Result<()> {
        self.arch_load_linux(vcpu, config)
    }

    pub(crate) fn init_mmio_devices(&mut self, config: &VmConfig) -> Result<()> {
        self.arch_init_mmio_devices(config)
    }

    pub(crate) fn init_io_devices(&mut self, vcpu: &VCPU, config: &VmConfig) -> Result<()> {
        self.arch_init_io_devices(vcpu, config)
    }

    /* Places a device at addr..addr + size, which must not overlap another. */
    pub(crate) fn add_mmio_device(&mut self, addr: u64, size: u64, device: Arc<Mutex<dyn MMIODevice>>) {
        let overlaps = self.mmio_devices.iter()
            .any(|(&base, &(len, _))| addr < base + len && base < addr + size);
        assert!(!overlaps, "MMIO device at {:#x}+{:#x} overlaps another", addr, size);
        self.mmio_devices.insert(addr, (size, device));
    }

    /* An access to no device reads as 0 and its writes are dropped, as on
     * a bus with nothing there; otherwise the read would return whatever
     * the last access left in the buffer. */
    pub(crate) fn handle_mmio(&self, mmio: &mut MMIO) -> Result<()> {
        let addr = mmio.phys_addr;
        let device = self.mmio_devices.range(..=addr).next_back()
            .filter(|(&base, &(size, _))| addr + mmio.len as u64 <= base + size);
        match device {
            Some((&base, (_, device))) => device.lock().unwrap().handle(addr - base, mmio, &self.guest_memory)?,
            None => if mmio.is_write == 0 { mmio.data.fill(0) }
        }
        Ok(())
    }

    pub(crate) fn handle_io(&self, io: &IO, base: *mut u8) -> Result<()> {
        self.arch_handle_io(io, base)
    }

    pub(crate) fn poll_mmio_devices(&self) -> Result<()> {
        for (_, device) in self.mmio_devices.values() {
            device.lock().unwrap().poll(&self.guest_memory)?;
        }
        Ok(())
    }

    /* Runs each vcpu on its own thread until the VM stops. The first vcpu
     * to leave its run loop stops the others; the first error is returned,
     * or else why the VM stopped. */
    pub(crate) fn run(self, vcpus: Vec<VCPU>) -> Result<VmExit> {
        unsafe { install_kick_handler() };

        let vm = Arc::new(self);
        let mut threads = vec![];
        let mut res = Ok(());
        for mut vcpu in vcpus {
            let vm_thread = vm.clone();
            let thread = std::thread::Builder::new()
                .name(format!("vcpu{}", vcpu.id))
                .spawn(move || {
                    let res = vcpu.run(&vm_thread);
                    match res {
                        Ok(Some(exit)) => vm_thread.control.stop(exit),
                        Ok(None) => (),
                        Err(_) => vm_thread.control.halt_vcpus()
                    }
                    res
                });
            match thread {
                Ok(thread) => threads.push(thread),
                Err(e) => { vm.control.halt_vcpus(); res = Err(anyhow::Error::new(e).context("cannot start a vcpu thread")); break; }
            }
        }

        for thread in threads {
            let thread_res = thread.join()
                .unwrap_or_else(|_| Err(anyhow!("vCPU thread panicked.")));
            if res.is_ok() {
                res = thread_res.map(|_| ());
            }
        }
        res?;
        /* Without an error, every vcpu left because stop() was called. */
        let exit = *vm.control.exit.lock().unwrap();
        Ok(exit.expect("vcpus stopped without a reason"))
    }
}

impl Drop for VM {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd); }
    }
}
