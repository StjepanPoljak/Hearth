use anyhow::{ bail, Context, Result };
use libc::_IO;
use std::cell::Cell;
use std::io;
use crate::kvm_bindings::*;
use crate::{ mmap_check, Breakpoint, VmExit, KVMIO, VM };

const KVM_RUN: libc::Ioctl = _IO(KVMIO, 0x80);

thread_local! {
    /* The kvm_run of the vcpu running on this thread, for kick_handler(). */
    static KVM_RUN_PTR: Cell<*mut kvm_run> = const { Cell::new(std::ptr::null_mut()) };
}

pub struct VCPU {
    pub id: u32,
    pub fd: libc::c_int,
    pub kvm_run_mem: *mut libc::c_void,
    kvm_run_size: usize,
    /* Set by set_breakpoints, in debug register order on x86. */
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    pub(crate) breakpoints: Vec<Breakpoint>
}

/* kvm_run is only touched by the thread running the vcpu. */
unsafe impl Send for VCPU {}

impl VCPU {
    /* Takes ownership of fd, a vcpu KVM_CREATE_VCPU made, and maps its kvm_run. */
    pub(crate) fn new(id: u32, fd: libc::c_int, kvm_run_size: usize) -> Result<Self> {
        let mut vcpu = VCPU { id, fd, kvm_run_mem: std::ptr::null_mut(), kvm_run_size, breakpoints: vec![] };
        vcpu.map_kvm_run()?;
        Ok(vcpu)
    }

    fn map_kvm_run(&mut self) -> Result<()> {
        // 140904 mmap(NULL, 12288, 0x3 /* PROT_READ|PROT_WRITE */, 0x1 /* MAP_SHARED */, 10<anon_inode:kvm-vcpu:0>, 0) = 0x7769050b0000
        self.kvm_run_mem = mmap_check("mmap of kvm_run", unsafe {
            libc::mmap(std::ptr::null_mut(),
                       self.kvm_run_size,
                       libc::PROT_READ|libc::PROT_WRITE,
                       libc::MAP_SHARED,
                       self.fd,
                       0)
        })?;
        Ok(())
    }

    /* Runs the vcpu on the calling thread until it exits or the VM stops. */
    /* None when the vcpu left because the VM was stopped from elsewhere. */
    pub(crate) fn run(&mut self, vm: &VM) -> Result<Option<VmExit>> {
        KVM_RUN_PTR.set(self.kvm_run_mem as *mut kvm_run);
        vm.control.register_vcpu_thread();
        let res = self.run_loop(vm);
        vm.control.unregister_vcpu_thread();
        KVM_RUN_PTR.set(std::ptr::null_mut());
        res
    }

    fn run_loop(&mut self, vm: &VM) -> Result<Option<VmExit>> {
        let run = self.kvm_run_mem as *mut kvm_run;

        loop {
            if vm.control.should_stop() {
                return Ok(None);
            }
            if vm.control.take_devices_pending() {
                vm.poll_mmio_devices()?;
            }

            let ret = unsafe { libc::ioctl(self.fd, KVM_RUN, 0usize) };
            if ret < 0 {
                /* Taken first: print_regs() makes ioctls of its own. */
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    unsafe { std::ptr::write_volatile(&mut (*run).immediate_exit, 0); }
                    continue;
                }
                /* An AP waiting for its SIPI got an INIT or a SIPI: KVM
                 * returns to have it run again, in its new state. */
                if e.raw_os_error() == Some(libc::EAGAIN) {
                    continue;
                }
                self.print_regs()?;
                return Err(e).context("KVM_RUN");
            }

            let exit_reason = unsafe { (*run).exit_reason };

            match exit_reason {
                KVM_EXIT_IO => {
                    let io = unsafe { (*run).__bindgen_anon_1.io };
                    vm.handle_io(&io, self.kvm_run_mem as *mut u8)?;
                }
                KVM_EXIT_SHUTDOWN => {
                    self.print_regs()?;
                    return Ok(Some(VmExit::TripleFault));
                }
                KVM_EXIT_MMIO => {
                    let mmio = unsafe { &mut (*run).__bindgen_anon_1.mmio };
                    vm.handle_mmio(mmio)?
                }
                KVM_EXIT_DEBUG => {
                    self.handle_debug_exit()?;
                }
                KVM_EXIT_HLT => return Ok(Some(VmExit::Halted)),
                KVM_EXIT_INTERNAL_ERROR => {
                    let internal = unsafe { (*run).__bindgen_anon_1.internal };
                    println!("suberror: {:#x}", internal.suberror);
                    for i in 0..internal.ndata {
                        println!("{:4}: {:#x}", i, internal.data[i as usize]);
                    }
                    bail!("KVM internal error.");
                }
                KVM_EXIT_SYSTEM_EVENT => {
                    let event = unsafe { (*run).__bindgen_anon_1.system_event };
                    match event.type_ {
                        KVM_SYSTEM_EVENT_SHUTDOWN => return Ok(Some(VmExit::PoweredOff)),
                        KVM_SYSTEM_EVENT_RESET => return Ok(Some(VmExit::Reset)),
                        KVM_SYSTEM_EVENT_CRASH => bail!("Guest crashed (KVM_SYSTEM_EVENT_CRASH)."),
                        other => bail!("Unexpected KVM system event {}.", other)
                    }
                }
                KVM_EXIT_FAIL_ENTRY => {
                    let fail_entry = unsafe { (*run).__bindgen_anon_1.fail_entry };
                    let reason = fail_entry.hardware_entry_failure_reason;
                    let cpu = fail_entry.cpu;
                    self.print_regs()?;
                    bail!(
                        "KVM could not enter the guest: hardware entry failure reason {:#x} on CPU {}.", reason, cpu);
                }
                _ => {
                    bail!("Unexpected KVM exit reason {}.", exit_reason);
                }
            }
        }
    }
}

impl Drop for VCPU {
    fn drop(&mut self) {
        if !self.kvm_run_mem.is_null() {
            unsafe { libc::munmap(self.kvm_run_mem, self.kvm_run_size); }
        }
        unsafe { libc::close(self.fd); }
    }
}

/* immediate_exit makes KVM_RUN return EINTR even if the kick lands before the vcpu enters the guest. */
extern "C" fn kick_handler(_sig: libc::c_int) {
    let run = KVM_RUN_PTR.get();
    if !run.is_null() {
        unsafe { std::ptr::write_volatile(&mut (*run).immediate_exit, 1); }
    }
}

/* Real-time signal, so it can't clash with SIGUSR1/2 users. */
pub(crate) fn kick_signal() -> libc::c_int {
    libc::SIGRTMIN()
}

/* Must run before any vcpu thread registers: the default action would kill us. */
pub(crate) unsafe fn install_kick_handler() {
    let mut sa: libc::sigaction = std::mem::zeroed();
    sa.sa_sigaction = kick_handler as *const() as usize;
    libc::sigemptyset(&mut sa.sa_mask);
    sa.sa_flags = 0;
    libc::sigaction(kick_signal(), &sa, std::ptr::null_mut());
}
