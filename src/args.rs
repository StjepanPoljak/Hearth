use anyhow::{ anyhow, Result };
use clap::Parser;
use crate::{ parse_disk, parse_mem_size, parse_number, parse_serial, DiskConfig, SerialSpec, VmConfig, PAGE_SIZE };

#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Args {
    /// Read the VM's settings from this TOML file: each flag --foo X is
    /// foo = X there, and --disk's settings go in a [disk] table. Flags
    /// given here override the file's settings
    #[arg(long, value_name = "FILE")]
    config: Option<String>,

    /// Binary to run
    #[arg(short, long)]
    binary: Option<String>,

    /// Kernel to boot: a vmlinux or other PVH ELF on x86, an Image on aarch64
    #[arg(long, alias = "kernel")]
    linux: Option<String>,

    /// Memory size in bytes, or with a K, M or G suffix, e.g. 512M; a
    /// multiple of the page size [default: 256K for --binary, 1G for --linux]
    #[arg(short, long, value_parser = parse_mem_size)]
    memory: Option<usize>,

    /// Number of vcpus; more than 1 on x86 only, where Linux finds them
    /// in the ACPI tables Hearth makes [default: 1]
    #[arg(long)]
    cpus: Option<u32>,

    /// Load address [default: 0x1000]
    #[arg(short, long)]
    load_addr: Option<u64>,

    /// Device tree blob for --linux on aarch64; on riscv64 Hearth makes
    /// one for the VM, and this replaces it
    #[arg(long)]
    dtb: Option<String>,

    /// Initramfs, or on x86 any file to pass as a PVH module
    #[arg(long)]
    initramfs: Option<String>,

    /// Disk image, with optional virtio-blk settings after it:
    /// PATH[,queue_size=N][,seg_max=N][,overlay]. queue_size is the request queue
    /// size, offered as QueueNumMax [default: 128]; split rings must be a
    /// power of 2, and Linux refuses other sizes. seg_max offers
    /// VIRTIO_BLK_F_SEG_MAX with this many data segments per request
    /// [default: not offered, the guest uses 1]. Without indirect
    /// descriptors a request takes seg_max + 2 ring entries, so keep
    /// seg_max <= queue_size - 2 or the guest can build requests that
    /// never fit. overlay leaves the image as it is: the guest's writes go
    /// to a temporary file next to it, dropped when the VM stops, and any
    /// number of VMs with overlays can share the image. Without it, Hearth locks
    /// the image for itself
    #[arg(long, value_parser = parse_disk, value_name = "PATH[,KEY=VALUE...]")]
    disk: Option<DiskConfig>,

    /// Tap interface name
    #[arg(long)]
    tap: Option<String>,

    /// Give the guest a virtio-balloon with free page reporting: the RAM
    /// it frees goes back to the host, rather than staying Hearth's for
    /// good. The guest kernel needs CONFIG_VIRTIO_BALLOON (x86 only)
    #[arg(long)]
    balloon: bool,

    /// Kernel command line arguments (on x86 extra ones; on riscv64 the
    /// device tree's bootargs, unless --dtb is given; on aarch64 the DTB
    /// has them)
    #[arg(long, allow_hyphen_values = true)]
    append: Option<String>,

    /// Serve the display and keyboard to VNC clients on this address,
    /// e.g. 127.0.0.1:5900. No authentication: keep it local or tunnel it.
    #[arg(long)]
    vnc: Option<String>,

    /// Where the serial console goes: stdio (this terminal; Ctrl-A x
    /// quits), pty (prints the device to open with screen or minicom),
    /// file:PATH (output only) or unix:PATH (a socket, one client at a time)
    /// [default: stdio]
    #[arg(long, value_parser = parse_serial)]
    serial: Option<SerialSpec>,

    /// Name of this VM: the process takes it (as ps and top show it, at
    /// most 15 bytes), so several VMs can be told apart
    #[arg(long)]
    name: Option<String>,

    /// Log the guest's writes to GPA..GPA+SIZE with KVM's dirty log and
    /// print the pages written when the VM stops, e.g. 0x200000 or
    /// 0x200000:64K [default SIZE: one page]
    #[arg(long, value_parser = parse_dirty_log)]
    pub(crate) dirty_log: Option<(u64, usize)>,

    /// Report each time the guest is about to execute this address, with
    /// its registers, and go on: a symbol of the --linux ELF, optionally
    /// +OFFSET, or an address, e.g. start_kernel or 0xffffffff81000000.
    /// Repeat for more, at most 4 (x86 only, hardware breakpoints)
    #[arg(long = "break", value_name = "SYMBOL[+OFFSET]|ADDRESS")]
    pub(crate) breakpoints: Vec<String>
}

impl Args {
    /* The --config file's settings, overridden by the flags given. */
    pub(crate) fn vm_config(&self) -> Result<VmConfig> {
        let mut config = match &self.config {
            Some(path) => VmConfig::load(path)?,
            None => VmConfig::default()
        };
        macro_rules! set {
            ($($field:ident),*) => {
                $(if let Some(value) = &self.$field {
                    config.$field = Some(value.clone());
                })*
            }
        }
        set!(name, binary, linux, memory, dtb, initramfs, append, disk, tap, vnc);
        if self.balloon {
            config.balloon = true;
        }
        if let Some(cpus) = self.cpus {
            config.cpus = cpus;
        }
        if let Some(load_addr) = self.load_addr {
            config.load_addr = load_addr;
        }
        if let Some(serial) = &self.serial {
            config.serial = serial.clone();
        }
        config.validate().map_err(|e| anyhow!(e))?;
        Ok(config)
    }
}

/* GPA[:SIZE], GPA in hex with 0x or decimal, SIZE as for --memory. */
fn parse_dirty_log(arg: &str) -> Result<(u64, usize), String> {
    let (gpa, size) = match arg.split_once(':') {
        Some((gpa, size)) => (gpa, parse_mem_size(size)?),
        None => (arg, PAGE_SIZE as usize)
    };
    let gpa = parse_number(gpa).map_err(|e| format!("invalid GPA: {}", e))?;
    Ok((gpa, size))
}
