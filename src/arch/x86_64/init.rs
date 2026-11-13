use anyhow::{ anyhow, Context, Result };
use crate::syscall_check;
use crate::{ kvm_cpuid2, kvm_cpuid_entry2, kvm_pit_config };
use crate::{ VCPU, VM, KvmDev, VmConfig, KVMIO, PAGE_SIZE, VIRTIO_MMIO_SIZE };
use crate::{ read_le64, read_le32, load_elf, read_file };
use libc::{_IOW, _IO, _IOWR};
use zerocopy::{ Immutable, IntoBytes };
use zerocopy::byteorder::{ LE, U32, U64 };

use object::read::elf::{ElfFile64, SectionHeader};
use object::{Endianness, Object};
use super::acpi::{ write_acpi_tables, Platform, VirtioSlot };
use super::power::{ SLEEP_CONTROL_PORT, SLEEP_STATUS_PORT, S5_SLEEP_TYPE };
use super::i8042::{ I8042_DATA, I8042_COMMAND, I8042_IRQ };
use super::io::{ UART_PORT, UART_IRQ };
use super::mmio::{ VIRTIO_BLK_MMIO_ADDR, VIRTIO_BLK_IRQ, VIRTIO_NET_MMIO_ADDR, VIRTIO_NET_IRQ };

const KVM_SET_TSS_ADDR: libc::Ioctl = _IO(KVMIO, 0x47);
const KVM_CREATE_IRQCHIP: libc::Ioctl = _IO(KVMIO, 0x60);
const KVM_CREATE_PIT2: libc::Ioctl = _IOW::<kvm_pit_config>(KVMIO, 0x77);
const KVM_GET_SUPPORTED_CPUID: libc::Ioctl = _IOWR::<kvm_cpuid2>(KVMIO, 0x05);
const KVM_SET_CPUID2: libc::Ioctl = _IOW::<kvm_cpuid2>(KVMIO, 0x90);

/* PVH boot structures. Their fields are little-endian whatever the host,
 * and align(8) keeps the alignment the u64 fields would have had. */
#[derive(IntoBytes, Immutable)]
#[repr(C, align(8))]
struct HvmStartInfo {
    magic: U32<LE>,
    version: U32<LE>,
    flags: U32<LE>,
    nr_modules: U32<LE>,
    modlist_paddr: U64<LE>,
    cmdline_paddr: U64<LE>,
    rsdp_paddr: U64<LE>,
    memmap_paddr: U64<LE>,
    memmap_entries: U32<LE>,
    reserved: U32<LE>
}

#[derive(IntoBytes, Immutable)]
#[repr(C, align(8))]
struct HvmModlistEntry {
    paddr: U64<LE>,
    size: U64<LE>,
    cmdline_paddr: U64<LE>,
    reserved: U64<LE>
}

#[derive(IntoBytes, Immutable)]
#[repr(C, align(8))]
struct HvmMemmapEntry {
    addr: U64<LE>,
    size: U64<LE>,
    type_: U32<LE>,
    reserved: U32<LE>
}

const E820_RAM: u32 = 1;
const E820_RESERVED: u32 = 2;

impl HvmStartInfo {
    fn new(nr_modules: u32, modlist_paddr: u64, cmdline_paddr: u64,
           rsdp_paddr: u64, memmap_paddr: u64, memmap_entries: u32) -> Self {
        /* "xEn3" with the top bit of 'E' set. */
        let mut magic = [ 'x', 'E', 'n', '3' ].map(|c| c as u8); magic[1] |= 0x80;
        HvmStartInfo {
            magic: U32::from_bytes(magic),
            version: 1.into(),
            flags: 0.into(),
            nr_modules: nr_modules.into(),
            modlist_paddr: modlist_paddr.into(),
            cmdline_paddr: cmdline_paddr.into(),
            rsdp_paddr: rsdp_paddr.into(),
            memmap_paddr: memmap_paddr.into(),
            memmap_entries: memmap_entries.into(),
            reserved: 0.into()
        }
    }
}

impl HvmModlistEntry {
    fn new(paddr: u64, size: u64, cmdline_paddr: u64) -> Self {
        HvmModlistEntry { paddr: paddr.into(), size: size.into(), cmdline_paddr: cmdline_paddr.into(), reserved: 0.into() }
    }
}

impl HvmMemmapEntry {
    fn new(addr: u64, size: u64, type_: u32) -> Self {
        HvmMemmapEntry { addr: addr.into(), size: size.into(), type_: type_.into(), reserved: 0.into() }
    }
}

/* Three pages in the MMIO hole, clear of RAM and devices, where KVM keeps
 * the TSS it runs real mode with when the host has no unrestricted guest
 * (no EPT, say). The address is QEMU's. */
const TSS_ADDR: u64 = 0xfffb_d000;

/* The hole below 4G that RAM leaves free, for the devices (virtio at
 * 0xd0000000) and KVM's TSS. */
const MMIO_START: u64 = 0xc000_0000;
const HIGH_RAM_START: u64 = 0x1_0000_0000;

impl VM {
    /* Guest RAM from 0; what doesn't fit below MMIO_START goes at 4G, so
     * the hole stays free. Returns the low region, which holds address 0. */
    pub fn add_ram(&mut self, size: usize) -> Result<usize> {
        let low_idx = self.add_mem_region(size.min(MMIO_START as usize), 0x0)?;
        if size as u64 > MMIO_START {
            self.add_mem_region(size - MMIO_START as usize, HIGH_RAM_START)?;
        }
        Ok(low_idx)
    }
}

/* Where the PVH start info goes, followed by what it points at. */
const HVM_START_INFO_ADDR: u64 = 0x10000;

pub fn arch_pre_vcpu_init(_kvm_dev: &KvmDev, vm: &mut VM) -> Result<()> {
    syscall_check("KVM_SET_TSS_ADDR", unsafe { libc::ioctl(vm.fd, KVM_SET_TSS_ADDR, TSS_ADDR) })?;

    syscall_check("KVM_CREATE_IRQCHIP", unsafe { libc::ioctl(vm.fd, KVM_CREATE_IRQCHIP, 0x0) })?;

    let pit_config = kvm_pit_config { flags: 0, pad: [0; 15] };

    syscall_check("KVM_CREATE_PIT2", unsafe { libc::ioctl(vm.fd, KVM_CREATE_PIT2, &pit_config) })?;

    Ok(())
}

/* Debug aid, not called: prints the function and index of each CPUID
 * entry, e.g. from arch_init after supported_cpuid. */
#[allow(dead_code)]
fn print_cpuid(entries: &[kvm_cpuid_entry2]) {
    for entry in entries {
        println!("{:#x} {:#x}", entry.function, entry.index);
    }
}

/* The entry's index matters: one entry per subleaf. */
const KVM_CPUID_FLAG_SIGNIFCANT_INDEX: u32 = 1;
const MAX_CPUID_ENTRIES: usize = 256;

fn supported_cpuid(kvm_dev: &KvmDev) -> Result<Vec<kvm_cpuid_entry2>> {
    let mut buf = vec![0u32; (size_of::<kvm_cpuid2>() + MAX_CPUID_ENTRIES * size_of::<kvm_cpuid_entry2>()) / 4];
    let cpuid2 = buf.as_mut_ptr() as *mut kvm_cpuid2;
    unsafe { (*cpuid2).nent = MAX_CPUID_ENTRIES as u32; }
    syscall_check("KVM_GET_SUPPORTED_CPUID", unsafe { libc::ioctl(kvm_dev.fd(), KVM_GET_SUPPORTED_CPUID, cpuid2) })?;
    let nent = unsafe { (*cpuid2).nent as usize };
    Ok(unsafe { (*cpuid2).__bindgen_anon_1.entries.as_slice(nent) }.to_vec())
}

fn set_cpuid(vcpu: &VCPU, entries: &[kvm_cpuid_entry2]) -> Result<()> {
    let mut buf = vec![0u32; (size_of::<kvm_cpuid2>() + entries.len() * size_of::<kvm_cpuid_entry2>()) / 4];
    let cpuid2 = buf.as_mut_ptr() as *mut kvm_cpuid2;
    unsafe {
        (*cpuid2).nent = entries.len() as u32;
        (*cpuid2).__bindgen_anon_1.entries.as_mut_slice(entries.len()).copy_from_slice(entries);
    }
    syscall_check("KVM_SET_CPUID2", unsafe { libc::ioctl(vcpu.fd, KVM_SET_CPUID2, cpuid2) })?;
    Ok(())
}

/* KVM's CPUID as the host has it, made to describe this VM: `cpus`
 * cores of one thread each in one package, this vcpu's APIC ID being
 * its id, as KVM makes its local APIC's. Linux checks the APIC IDs the
 * CPUID gives against the MADT's. */
fn vcpu_cpuid(supported: &[kvm_cpuid_entry2], apic_id: u32, cpus: u32) -> Vec<kvm_cpuid_entry2> {
    /* Bits of the x2APIC ID that number the cores in a package. */
    let core_bits = cpus.next_power_of_two().trailing_zeros();
    let mut entries: Vec<kvm_cpuid_entry2> = supported.iter()
        .filter(|e| e.function != 0xb && e.function != 0x1f)
        .copied()
        .collect();
    for e in entries.iter_mut() {
        match e.function {
            1 => {
                /* EBX: initial APIC ID in 31:24, addressable logical
                 * processors in 23:16. EDX bit 28, HTT: that count is valid. */
                e.ebx = (e.ebx & 0xffff) | (apic_id << 24) | ((1 << core_bits) << 16);
                e.edx |= 1 << 28;
            }
            /* Type 0: no more caches. */
            4 if e.eax & 0x1f != 0 => {
                /* EAX 31:26: core IDs in a package, less one; 25:14:
                 * threads sharing this cache, less one. Each core has
                 * its own L1 and L2, and they share the L3. */
                let level = (e.eax >> 5) & 0x7;
                let sharing = if level >= 3 { (1 << core_bits) - 1 } else { 0 };
                e.eax = (e.eax & 0x3fff) | (sharing << 14) | (((1 << core_bits) - 1) << 26);
            }
            _ => ()
        }
    }
    /* Extended topology: an SMT level of one thread, then a core level
     * of `cpus`; the subleaf after the last has level type 0. EDX is the
     * x2APIC ID in every subleaf. 0x1f is 0xb's successor; Linux takes
     * it when the maximum leaf reaches it. */
    let max_leaf = supported.iter().find(|e| e.function == 0).map_or(0, |e| e.eax);
    for function in [0xb, 0x1f].into_iter().filter(|&f| f <= max_leaf) {
        let levels = [
            (0, 1, 1),                   /* EAX: shift to the next level, EBX: logical processors, ECX 15:8: level type */
            (core_bits, cpus, 2),
            (0, 0, 0)
        ];
        for (index, (shift, count, kind)) in levels.into_iter().enumerate() {
            entries.push(kvm_cpuid_entry2 {
                function,
                index: index as u32,
                flags: KVM_CPUID_FLAG_SIGNIFCANT_INDEX,
                eax: shift,
                ebx: count,
                ecx: (kind << 8) | index as u32,
                edx: apic_id,
                padding: [0; 3]
            });
        }
    }
    entries
}

/* vcpus[0] is the boot processor, which starts at the VMM's entry point;
 * KVM keeps the others waiting for the INIT and SIPI the guest sends. */
pub fn arch_init(kvm_dev: &KvmDev, _vm: &mut VM, vcpus: &mut [VCPU]) -> Result<()> {
    let supported = supported_cpuid(kvm_dev)?;
    let cpus = vcpus.len() as u32;
    for vcpu in vcpus.iter() {
        set_cpuid(vcpu, &vcpu_cpuid(&supported, vcpu.id, cpus))?;
    }

    let bsp = &mut vcpus[0];
    let mut sregs2 = bsp.get_sregs2()?;

    sregs2.cs.base = 0;
    sregs2.cs.selector = 0;

    bsp.set_sregs2(sregs2)
}

impl VM {
    fn try_pvh_entrypoint(&mut self, vmlinux: &ElfFile64<Endianness>) -> Result<u64> {
        let endian = vmlinux.endian();
        let mut notes = vmlinux.section_by_name(".notes").ok_or(anyhow!("No .notes found."))?
            .elf_section_header()
            .notes(endian, vmlinux.data()).unwrap()
            .unwrap();

        while let Some(note) = notes.next().unwrap() {
            if note.n_type(endian) == object::elf::NoteType(18) {
                return match note.desc().len() {
                    4 => Ok(read_le32(note.desc(), 0) as u64),
                    8 => Ok(read_le64(note.desc(), 0)),
                    _ => Err(anyhow!("unexpected PHYS32_ENTRY descsz {}", note.desc().len()))
                };
            }
        }

        Err(anyhow!("No valid PVH entry found."))
    }

    /* vcpu is the boot processor; the ACPI tables describe config.cpus. */
    fn linux_pvh_boot(&mut self, vcpu: &mut VCPU, config: &VmConfig) -> Result<()> {
        let vmlinux_path = config.linux.as_ref().unwrap();
        let vmlinux_data = read_file(vmlinux_path)?;
        let vmlinux = ElfFile64::<Endianness>::parse(&*vmlinux_data)
            .with_context(|| format!("{} is not an ELF file", vmlinux_path))?;

        let pvh_entrypoint = self.try_pvh_entrypoint(&vmlinux)?;

        const EBDA_START: u64 = 0x9fc00;
        /* The BIOS area, reserved in the E820 map: the ACPI tables. */
        const ACPI_START: u64 = 0xe_0000;
        const HIMEM_START: u64 = 0x10_0000;

        let ram_size = config.memory.unwrap_or(1 << 30) as u64;
        let linux_mem_idx = self.add_ram(ram_size as usize)?;

        let last_addr = load_elf(self, &vmlinux, &vmlinux_data, linux_mem_idx)?;

        /* The devices are in the ACPI tables, not on the command line.
         * reboot=k: on hardware-reduced ACPI, Linux reboots through EFI
         * and then the BIOS, and Hearth has neither; it has the i8042,
         * whose reset line stops it. --append can still say otherwise. */
        let mut cmdline = String::from("console=ttyS0,115200 root=/dev/vda reboot=k");
        if let Some(append) = &config.append {
            cmdline = format!("{} {}", cmdline, append);
        }
        /* Boot info goes in low RAM below the EBDA; modules go after the
         * kernel, up to the end of RAM below 4G. */
        let region = self.mem_region_mut(linux_mem_idx)?;
        let boot_alloc = region.add_bump_allocator(HVM_START_INFO_ADDR, EBDA_START)?;
        let module_alloc = region.add_bump_allocator(last_addr.next_multiple_of(PAGE_SIZE), ram_size.min(MMIO_START))?;
        let acpi_alloc = region.add_bump_allocator(ACPI_START, HIMEM_START)?;
        let platform = Platform {
            uart_port: UART_PORT, uart_irq: UART_IRQ,
            i8042_data: I8042_DATA, i8042_command: I8042_COMMAND, i8042_irq: I8042_IRQ,
            sleep_control_port: SLEEP_CONTROL_PORT, sleep_status_port: SLEEP_STATUS_PORT,
            s5_sleep_type: S5_SLEEP_TYPE
        };
        let virtio = [
            VirtioSlot { addr: VIRTIO_BLK_MMIO_ADDR, size: VIRTIO_MMIO_SIZE as u32, gsi: VIRTIO_BLK_IRQ },
            VirtioSlot { addr: VIRTIO_NET_MMIO_ADDR, size: VIRTIO_MMIO_SIZE as u32, gsi: VIRTIO_NET_IRQ }
        ];
        let rsdp_paddr = write_acpi_tables(&mut region.allocators[acpi_alloc], config.cpus, &platform, &virtio)?;

        let mut modlist_entries = vec![];
        if let Some(initramfs_path) = &config.initramfs {
            /* The module's command line carries its file name, for guests
             * that look modules up by name; Linux ignores it. */
            let module_name = std::path::Path::new(initramfs_path)
                .file_name().map_or(String::new(), |name| name.to_string_lossy().into_owned());
            let initramfs = read_file(initramfs_path)?;

            let modules = &mut region.allocators[module_alloc];
            let cmdline_paddr = modules.push(format!("{}\0", module_name).as_bytes())?;
            let paddr = modules.push_aligned(&initramfs, Some(PAGE_SIZE))?;
            println!("Loaded {:?} at {:#x}.\r", initramfs_path, paddr);
            modlist_entries.push(HvmModlistEntry::new(paddr, initramfs.len() as u64, cmdline_paddr));
        }

        let mut memmap_entries = vec![
            HvmMemmapEntry::new(0x0, EBDA_START, E820_RAM),
            HvmMemmapEntry::new(EBDA_START, HIMEM_START - EBDA_START, E820_RESERVED),
            HvmMemmapEntry::new(HIMEM_START, ram_size.min(MMIO_START) - HIMEM_START, E820_RAM)
        ];
        if ram_size > MMIO_START {
            memmap_entries.push(HvmMemmapEntry::new(HIGH_RAM_START, ram_size - MMIO_START, E820_RAM));
        }

        /* The start info points at the rest, so it is filled in last. */
        let boot = &mut region.allocators[boot_alloc];
        let cmdline_paddr = boot.push(format!("{}\0", cmdline).as_bytes())?;
        let memmap_paddr = boot.push_aligned(&memmap_entries, Some(PAGE_SIZE))?;
        let modlist_paddr = boot.push_aligned(&modlist_entries, Some(PAGE_SIZE))?;

        let hvm_start_info = HvmStartInfo::new(
            modlist_entries.len() as u32, modlist_paddr, cmdline_paddr,
            rsdp_paddr, memmap_paddr, memmap_entries.len() as u32);
        let start_info_paddr = boot.push_aligned(&[hvm_start_info], Some(PAGE_SIZE))?;

        let mut regs = vcpu.get_regs()?;
        regs.rip = pvh_entrypoint;
        regs.rbx = start_info_paddr;
        regs.rflags = 0x2; // bits 17, 9 and 8 cleared, all other unspecified
        vcpu.set_regs(regs)?;
        regs.print();

        vcpu.protected_mode_setup()?;

        Ok(())
    }

    pub fn arch_load_linux(&mut self, vcpu: &mut VCPU, config: &VmConfig) -> Result<()> {
        self.linux_pvh_boot(vcpu, config)
    }
}
