/* std's print macros panic when the output is gone, as in `hearth | head`
 * once head exits: Rust ignores SIGPIPE, so the write fails with EPIPE.
 * Hearth must keep running the guest, and must not die of SIGPIPE either,
 * as a VNC or serial socket client may go away too. So these shadow std's
 * for the whole crate and drop what can't be written. */
macro_rules! println {
    ($($arg:tt)*) => {{ use std::io::Write; let _ = writeln!(std::io::stdout(), $($arg)*); }}
}
#[allow(unused_macros)]
macro_rules! print {
    ($($arg:tt)*) => {{ use std::io::Write; let _ = write!(std::io::stdout(), $($arg)*); }}
}
macro_rules! eprintln {
    ($($arg:tt)*) => {{ use std::io::Write; let _ = writeln!(std::io::stderr(), $($arg)*); }}
}
#[allow(unused_macros)]
macro_rules! eprint {
    ($($arg:tt)*) => {{ use std::io::Write; let _ = write!(std::io::stderr(), $($arg)*); }}
}

#[allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code)]
mod kvm_bindings {
    include!(concat!(env!("OUT_DIR"), "/kvm-bindings.rs"));
}
use anyhow::Result;
use kvm_bindings::*;

include!("frontend/mod.rs");
include!("util/mod.rs");
include!("arch/mod.rs");
include!("virtio/mod.rs");

mod config;
use config::*;
mod args;
use args::*;
mod memory;
use memory::*;
mod vm;
use vm::*;
mod vcpu;
use vcpu::*;

use clap::Parser;

const PAGE_SIZE: u64 = 4096;

/* Names the calling thread, and with it every thread it starts unnamed;
 * the main thread's name is the process's. The kernel keeps 15 bytes. */
fn set_process_name(name: &str) -> Result<()> {
    let mut comm = [0u8; 16];
    let len = name.floor_char_boundary(15);
    comm[..len].copy_from_slice(&name.as_bytes()[..len]);
    syscall_check("prctl(PR_SET_NAME)", unsafe { libc::prctl(libc::PR_SET_NAME, comm.as_ptr()) })?;
    Ok(())
}

fn print_dirty_pages(log: &DirtyLog) -> Result<()> {
    let bitmap = log.fetch()?;
    let pages: Vec<u64> = (0..bitmap.len() * 64)
        .filter(|n| bitmap[n / 64] & (1 << (n % 64)) != 0)
        .map(|n| log.gpa + n as u64 * PAGE_SIZE)
        .collect();
    if pages.is_empty() {
        println!("Dirty log: no page written.\r");
    }
    for page in pages {
        println!("Dirty log: {:#x} written.\r", page);
    }
    Ok(())
}

fn main() {
    let args = Args::parse();
    /* The serial frontend's thread may never get to restore the terminal
     * when the guest stops the VM, or when setting up fails after it put
     * the terminal in raw mode, so main restores it. */
    let saved_tty = save_tty();
    let res = args.vm_config().and_then(|config| run(&args, &config));
    if let Some(tty) = saved_tty {
        restore_tty(&tty);
    }
    match res {
        Ok(exit) => if let Some(message) = exit.message() {
            println!("{}", message);
        },
        Err(e) => {
            eprintln!("hearth: {:#}", e);
            std::process::exit(1);
        }
    }
}

fn run(args: &Args, config: &VmConfig) -> Result<VmExit> {
    if let Some(name) = &config.name {
        set_process_name(name)?;
    }
    let kvm_dev = KvmDev::new()?;
    let mut vm = kvm_dev.create_vm()?;

    arch::arch_pre_vcpu_init(&kvm_dev, &mut vm)?;

    let mut vcpus = (0..config.cpus)
        .map(|id| vm.create_vcpu(id, kvm_dev.get_kvm_run_size()))
        .collect::<Result<Vec<_>>>()?;

    match &config.name {
        Some(name) => println!("\nHearth: {}\r", name),
        None => println!("\nHearth\r")
    }

    arch::arch_init(&kvm_dev, &mut vm, &mut vcpus)?;

    /* The boot processor; the others wait for the guest to start them. */
    let vcpu = &mut vcpus[0];
    vcpu.set_ip(config.load_addr as usize)?;

    /* Guest memory is complete before any device (and device thread) exists. */
    if let Some(_) = config.linux {
        vm.load_linux(vcpu, config)?;
    } else if let Some(binary) = &config.binary {
        let mem_region_idx = vm.add_ram(config.memory.unwrap_or(256 << 10))?;
        vm.load_file_to_memory(mem_region_idx, binary, config.load_addr)?;
    }

    if !args.breakpoints.is_empty() {
        let elf_data = match &config.linux {
            Some(path) => Some(read_file(path)?),
            None => None
        };
        let breakpoints = args.breakpoints.iter()
            .map(|spec| resolve_breakpoint(spec, elf_data.as_deref()))
            .collect::<Result<Vec<_>>>()?;
        vcpu.set_breakpoints(breakpoints)?;
        for bp in &vcpu.breakpoints {
            println!("Breakpoint {} at {:#x}.\r", bp.label, bp.addr);
        }
    }

    vm.init_mmio_devices(config)?;
    vm.init_io_devices(&vcpus[0], config)?;
    let dirty_log = match args.dirty_log {
        Some((gpa, size)) => Some(vm.log_dirty_pages(gpa, size)?),
        None => None
    };
    if let Some(addr) = &config.vnc {
        start_rfb(addr.as_str(), vm.display.clone(), vm.keyboard.clone())?;
    }

    let res = vm.run(vcpus);
    if let Some(log) = dirty_log {
        /* The VM's own error matters more; only report this one. */
        if let Err(e) = print_dirty_pages(&log) {
            eprintln!("Dirty log: cannot fetch: {:#}\r", e);
        }
    }
    res
}
