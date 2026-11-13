/* ACPI tables for Linux on x86. The guest finds the RSDP through the PVH
 * start info; the RSDP points at the XSDT, which lists the FADT and the
 * MADT.
 *
 * The MADT lists a local APIC per vcpu, and KVM's IOAPIC: Linux brings
 * up the vcpus from it.
 *
 * The FADT declares a hardware-reduced platform: no PM blocks and no SCI
 * to emulate, and Linux sets up neither the legacy PIC nor the PIT, but
 * takes every interrupt through the IOAPIC, as the devices' ACPI
 * descriptions give them. So the DSDT the FADT points at describes each
 * device with an interrupt: COM1, the i8042's keyboard port, and the
 * virtio-mmio slots, which Linux's virtio_mmio driver binds by their
 * _HID, LNRO0005. The FADT's sleep control and status registers, with
 * the DSDT's \_S5_, let the guest power off.
 *
 * Every table is little-endian, whatever the host, and each one's bytes
 * sum to 0 mod 256. */

use anyhow::Result;
use crate::BumpAllocator;

/* What KVM's in-kernel irqchip puts where. */
const LOCAL_APIC_ADDR: u32 = 0xfee0_0000;
const IOAPIC_ADDR: u32 = 0xfec0_0000;
const IOAPIC_ID: u8 = 0;

const OEM_ID: &[u8; 6] = b"HEARTH";
const OEM_TABLE_ID: &[u8; 8] = b"HEARTH  ";
const CREATOR_ID: &[u8; 4] = b"HRTH";

/* FADT Flags: no fixed hardware; everything is described by the tables. */
const FADT_HW_REDUCED_ACPI: u32 = 1 << 20;
/* FADT IA-PC boot architecture flags. */
const IAPC_8042: u16 = 1 << 1;
const IAPC_MSI_NOT_SUPPORTED: u16 = 1 << 3;
const IAPC_CMOS_RTC_NOT_PRESENT: u16 = 1 << 5;

/* MADT */
const MADT_PCAT_COMPAT: u32 = 1; // there are 8259s; KVM's irqchip has them
const MADT_LOCAL_APIC: u8 = 0;
const MADT_IOAPIC: u8 = 1;
const MADT_LOCAL_APIC_NMI: u8 = 4;
const MADT_LOCAL_APIC_ENABLED: u32 = 1;

/* Little-endian writes for building tables. */
trait Put {
    fn put8(&mut self, v: u8) -> &mut Self;
    fn put16(&mut self, v: u16) -> &mut Self;
    fn put32(&mut self, v: u32) -> &mut Self;
    fn put64(&mut self, v: u64) -> &mut Self;
    fn put(&mut self, v: &[u8]) -> &mut Self;
}

impl Put for Vec<u8> {
    fn put8(&mut self, v: u8) -> &mut Self { self.push(v); self }
    fn put16(&mut self, v: u16) -> &mut Self { self.put(&v.to_le_bytes()) }
    fn put32(&mut self, v: u32) -> &mut Self { self.put(&v.to_le_bytes()) }
    fn put64(&mut self, v: u64) -> &mut Self { self.put(&v.to_le_bytes()) }
    fn put(&mut self, v: &[u8]) -> &mut Self { self.extend_from_slice(v); self }
}

/* The byte that makes data sum to 0, if it goes in a byte that is 0. */
fn checksum(data: &[u8]) -> u8 {
    0u8.wrapping_sub(data.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)))
}

/* A system description table: the 36-byte header, then body. */
fn sdt(signature: &[u8; 4], revision: u8, body: &[u8]) -> Vec<u8> {
    const HEADER_LEN: usize = 36;
    const CHECKSUM_OFFSET: usize = 9;
    let mut table = Vec::with_capacity(HEADER_LEN + body.len());
    table.put(signature)
        .put32((HEADER_LEN + body.len()) as u32)
        .put8(revision)
        .put8(0) // checksum, below
        .put(OEM_ID)
        .put(OEM_TABLE_ID)
        .put32(1) // OEM revision
        .put(CREATOR_ID)
        .put32(1) // creator revision
        .put(body);
    table[CHECKSUM_OFFSET] = checksum(&table);
    table
}

/* Revision 2 (ACPI 2.0 and later), pointing at the XSDT only. */
fn rsdp(xsdt_addr: u64) -> Vec<u8> {
    const LEN: u32 = 36;
    let mut rsdp = Vec::with_capacity(LEN as usize);
    rsdp.put(b"RSD PTR ")
        .put8(0) // checksum of the first 20 bytes
        .put(OEM_ID)
        .put8(2) // revision
        .put32(0) // RSDT address: none, the XSDT replaces it
        .put32(LEN)
        .put64(xsdt_addr)
        .put8(0) // extended checksum, of all of it
        .put(&[0; 3]);
    rsdp[8] = checksum(&rsdp[..20]);
    rsdp[32] = checksum(&rsdp);
    rsdp
}

fn xsdt(tables: &[u64]) -> Vec<u8> {
    let mut body = vec![];
    for addr in tables {
        body.put64(*addr);
    }
    sdt(b"XSDT", 1, &body)
}

/* A Generic Address Structure for a byte-wide I/O port. */
fn gas_io_byte(port: u16) -> Vec<u8> {
    const SYSTEM_IO: u8 = 1;
    const BYTE_ACCESS: u8 = 1;
    let mut gas = vec![];
    gas.put8(SYSTEM_IO).put8(8).put8(0).put8(BYTE_ACCESS).put64(port as u64);
    gas
}

/* FADT revision 6.5. Hardware-reduced: of the fixed hardware fields, all
 * stay 0; only the flags, the boot architecture, the DSDT and the sleep
 * registers are set. The UART at 0x3f8 is in the DSDT, so
 * LEGACY_DEVICES stays clear. */
fn fadt(dsdt_addr: u64, platform: &Platform) -> Vec<u8> {
    const BODY_LEN: usize = 276 - 36;
    /* Offsets in the whole table, header included. */
    const IAPC_BOOT_ARCH: usize = 109;
    const FLAGS: usize = 112;
    const MINOR_VERSION: usize = 131;
    const X_DSDT: usize = 140;
    const SLEEP_CONTROL_REG: usize = 244;
    const SLEEP_STATUS_REG: usize = 256;
    let mut body = vec![0u8; BODY_LEN];
    let at = |offset: usize| offset - 36;
    body[at(IAPC_BOOT_ARCH)..][..2].copy_from_slice(
        &(IAPC_8042 | IAPC_MSI_NOT_SUPPORTED | IAPC_CMOS_RTC_NOT_PRESENT).to_le_bytes());
    body[at(FLAGS)..][..4].copy_from_slice(&FADT_HW_REDUCED_ACPI.to_le_bytes());
    body[at(MINOR_VERSION)] = 5;
    body[at(X_DSDT)..][..8].copy_from_slice(&dsdt_addr.to_le_bytes());
    body[at(SLEEP_CONTROL_REG)..][..12].copy_from_slice(&gas_io_byte(platform.sleep_control_port));
    body[at(SLEEP_STATUS_REG)..][..12].copy_from_slice(&gas_io_byte(platform.sleep_status_port));
    sdt(b"FACP", 6, &body)
}

/* Revision 2 of the DSDT has 64-bit AML integers. */
fn dsdt(aml: &[u8]) -> Vec<u8> {
    sdt(b"DSDT", 2, aml)
}

/* A local APIC per vcpu, ACPI processor UID and APIC ID both the vcpu's
 * id, as KVM gives each vcpu its id as its APIC ID; then the IOAPIC,
 * its pins GSIs 0..23, which ISA IRQs map to one to one; then LINT1 of
 * every local APIC as NMI, as on PCs. */
fn madt(cpus: u32) -> Vec<u8> {
    let mut body = vec![];
    body.put32(LOCAL_APIC_ADDR).put32(MADT_PCAT_COMPAT);
    for id in 0..cpus {
        body.put8(MADT_LOCAL_APIC).put8(8)
            .put8(id as u8).put8(id as u8)
            .put32(MADT_LOCAL_APIC_ENABLED);
    }
    body.put8(MADT_IOAPIC).put8(12)
        .put8(IOAPIC_ID).put8(0)
        .put32(IOAPIC_ADDR)
        .put32(0); // GSI base
    body.put8(MADT_LOCAL_APIC_NMI).put8(6)
        .put8(0xff) // all processors
        .put16(0)   // flags: as the bus conforms
        .put8(1);   // LINT1
    sdt(b"APIC", 5, &body)
}

/* The bits of AML the DSDT needs: named devices with an ID and their
 * resources, in \_SB. ACPI 6.5, section 20 has the encoding. */
mod aml {
    use super::Put;

    const ZERO_OP: u8 = 0x00;
    const ONE_OP: u8 = 0x01;
    const NAME_OP: u8 = 0x08;
    const BYTE_PREFIX: u8 = 0x0a;
    const WORD_PREFIX: u8 = 0x0b;
    const DWORD_PREFIX: u8 = 0x0c;
    const STRING_PREFIX: u8 = 0x0d;
    const QWORD_PREFIX: u8 = 0x0e;
    const SCOPE_OP: u8 = 0x10;
    const BUFFER_OP: u8 = 0x11;
    const PACKAGE_OP: u8 = 0x12;
    const EXT_OP_PREFIX: u8 = 0x5b;
    const DEVICE_OP: u8 = 0x82;
    const ROOT_CHAR: u8 = b'\\';

    /* A PkgLength followed by what it covers; the length counts its own
     * bytes. One byte holds up to 63; longer ones take 2 to 4 bytes,
     * the first holding the byte count in bits 7:6 and the low nibble. */
    fn package(contents: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        let len = contents.len();
        if len + 1 <= 0x3f {
            out.put8((len + 1) as u8);
        } else {
            let n = (2..=4).find(|n| len + n < 1 << (4 + 8 * (n - 1)))
                .expect("AML package longer than 2^28 bytes");
            let total = len + n;
            out.put8((((n - 1) << 6) | (total & 0xf)) as u8);
            for i in 0..n - 1 {
                out.put8((total >> (4 + 8 * i)) as u8);
            }
        }
        out.put(contents);
        out
    }

    /* A NameSeg: four characters, A-Z, 0-9 or _, padded with _. */
    fn name_seg(name: &str) -> [u8; 4] {
        assert!(name.len() <= 4 && name.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_'),
                "bad AML name {:?}", name);
        let mut seg = [b'_'; 4];
        seg[..name.len()].copy_from_slice(name.as_bytes());
        seg
    }

    pub fn integer(n: u64) -> Vec<u8> {
        let mut out = vec![];
        match n {
            0 => out.put8(ZERO_OP),
            1 => out.put8(ONE_OP),
            2..=0xff => out.put8(BYTE_PREFIX).put8(n as u8),
            0x100..=0xffff => out.put8(WORD_PREFIX).put16(n as u16),
            0x1_0000..=0xffff_ffff => out.put8(DWORD_PREFIX).put32(n as u32),
            _ => out.put8(QWORD_PREFIX).put64(n)
        };
        out
    }

    pub fn string(s: &str) -> Vec<u8> {
        let mut out = vec![STRING_PREFIX];
        out.put(s.as_bytes()).put8(0);
        out
    }

    /* EisaId("PNP0501"): three letters of five bits each, then four
     * hex digits, as a DWORD whose bytes are in that order. */
    pub fn eisa_id(id: &str) -> Vec<u8> {
        let b = id.as_bytes();
        assert!(b.len() == 7 && b[..3].iter().all(u8::is_ascii_uppercase), "bad EISA ID {:?}", id);
        let letters = b[..3].iter().fold(0u16, |v, c| (v << 5) | (c - b'@') as u16);
        let digits = u16::from_str_radix(&id[3..], 16).expect("bad EISA ID digits");
        let mut out = vec![DWORD_PREFIX];
        out.put(&letters.to_be_bytes()).put(&digits.to_be_bytes());
        out
    }

    /* Name(NAME, value) */
    pub fn name(name: &str, value: &[u8]) -> Vec<u8> {
        let mut out = vec![NAME_OP];
        out.put(&name_seg(name)).put(value);
        out
    }

    /* Scope(\NAME) { body } */
    pub fn root_scope(name: &str, body: &[u8]) -> Vec<u8> {
        let mut contents = vec![ROOT_CHAR];
        contents.put(&name_seg(name)).put(body);
        let mut out = vec![SCOPE_OP];
        out.put(&package(&contents));
        out
    }

    /* Device(NAME) { body } */
    pub fn device(name: &str, body: &[u8]) -> Vec<u8> {
        let mut contents = name_seg(name).to_vec();
        contents.put(body);
        let mut out = vec![EXT_OP_PREFIX, DEVICE_OP];
        out.put(&package(&contents));
        out
    }

    /* Package() { elements }, each element already encoded. */
    pub fn package_of(elements: &[Vec<u8>]) -> Vec<u8> {
        let mut contents = vec![u8::try_from(elements.len()).expect("AML package of over 255 elements")];
        contents.put(&elements.concat());
        let mut out = vec![PACKAGE_OP];
        out.put(&package(&contents));
        out
    }

    /* ResourceTemplate() { descriptors }: a buffer of resource
     * descriptors, closed by an end tag whose checksum 0 means none. */
    pub fn resource_template(descriptors: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes: Vec<u8> = descriptors.concat();
        bytes.put8(0x79).put8(0);
        let mut contents = integer(bytes.len() as u64);
        contents.put(&bytes);
        let mut out = vec![BUFFER_OP];
        out.put(&package(&contents));
        out
    }

    /* IO(Decode16, port, port, 1, len) */
    pub fn io(port: u16, len: u8) -> Vec<u8> {
        let mut out = vec![0x47, 1];
        out.put16(port).put16(port).put8(1).put8(len);
        out
    }

    /* IRQNoFlags() { irq }: an ISA IRQ, edge-triggered, active high. */
    pub fn irq_no_flags(irq: u32) -> Vec<u8> {
        assert!(irq < 16);
        let mut out = vec![0x22];
        out.put16(1 << irq);
        out
    }

    /* Memory32Fixed(ReadWrite, base, len) */
    pub fn memory32_fixed(base: u32, len: u32) -> Vec<u8> {
        let mut out = vec![0x86];
        out.put16(9).put8(1).put32(base).put32(len);
        out
    }

    /* Interrupt(ResourceConsumer, Edge, ActiveHigh, Exclusive) { gsi } */
    pub fn interrupt_edge(gsi: u32) -> Vec<u8> {
        const CONSUMER: u8 = 1 << 0;
        const EDGE: u8 = 1 << 1;
        let mut out = vec![0x89];
        out.put16(6).put8(CONSUMER | EDGE).put8(1).put32(gsi);
        out
    }
}

/* A virtio-mmio slot: its registers and its interrupt. */
pub struct VirtioSlot {
    pub addr: u64,
    pub size: u32,
    pub gsi: u32
}

/* The rest of the machine: the ISA devices on their usual ports and IRQs,
 * and the ACPI sleep registers. */
pub struct Platform {
    pub uart_port: u16,
    pub uart_irq: u32,
    pub i8042_data: u16,
    pub i8042_command: u16,
    pub i8042_irq: u32,
    pub sleep_control_port: u16,
    pub sleep_status_port: u16,
    /* The SLP_TYP that means S5, soft off. */
    pub s5_sleep_type: u8
}

/* The DSDT's AML: \_S5_, then \_SB with COM1, the i8042's keyboard port
 * and a device per virtio-mmio slot. Each _UID tells devices with the
 * same _HID apart. */
fn dsdt_aml(platform: &Platform, virtio: &[VirtioSlot]) -> Vec<u8> {
    use aml::*;
    /* SLP_TYP for the sleep control register; the other three fields
     * are for PM1b and reserved. */
    let s5 = name("_S5", &package_of(&[integer(platform.s5_sleep_type as u64), integer(0), integer(0), integer(0)]));
    let mut devices = vec![];
    devices.put(&device("COM1", &[
        name("_HID", &eisa_id("PNP0501")),
        name("_UID", &integer(1)),
        name("_CRS", &resource_template(&[
            io(platform.uart_port, 8),
            irq_no_flags(platform.uart_irq)
        ]))
    ].concat()));
    devices.put(&device("PS2K", &[
        name("_HID", &eisa_id("PNP0303")),
        name("_CRS", &resource_template(&[
            io(platform.i8042_data, 1),
            io(platform.i8042_command, 1),
            irq_no_flags(platform.i8042_irq)
        ]))
    ].concat()));
    for (i, slot) in virtio.iter().enumerate() {
        let addr = u32::try_from(slot.addr).expect("virtio-mmio slot above 4G");
        devices.put(&device(&format!("VR{:02X}", i), &[
            name("_HID", &string("LNRO0005")),
            name("_UID", &integer(i as u64)),
            name("_CRS", &resource_template(&[
                memory32_fixed(addr, slot.size),
                interrupt_edge(slot.gsi)
            ]))
        ].concat()));
    }
    [s5, root_scope("_SB", &devices)].concat()
}

/* Most vcpus the MADT can describe: local APIC entries hold 8-bit IDs,
 * and 0xff means all processors. */
pub const MAX_CPUS: u32 = 255;

/* Writes the tables through alloc and returns the RSDP's address. */
pub fn write_acpi_tables(alloc: &mut BumpAllocator, cpus: u32, platform: &Platform,
                         virtio: &[VirtioSlot]) -> Result<u64> {
    assert!((1..=MAX_CPUS).contains(&cpus));
    /* Each table goes in before the ones that point at it. */
    let dsdt_addr = alloc.push_aligned(&dsdt(&dsdt_aml(platform, virtio)), Some(8))?;
    let fadt_addr = alloc.push_aligned(&fadt(dsdt_addr, platform), Some(8))?;
    let madt_addr = alloc.push_aligned(&madt(cpus), Some(8))?;
    let xsdt_addr = alloc.push_aligned(&xsdt(&[fadt_addr, madt_addr]), Some(8))?;
    /* Linux looks for it on 16-byte boundaries when it scans for it. */
    alloc.push_aligned(&rsdp(xsdt_addr), Some(16))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sums_to_zero(data: &[u8]) -> bool {
        data.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)) == 0
    }

    fn le32(data: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
    }

    fn le64(data: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
    }

    const PLATFORM: Platform = Platform {
        uart_port: 0x3f8, uart_irq: 4, i8042_data: 0x60, i8042_command: 0x64, i8042_irq: 1,
        sleep_control_port: 0x600, sleep_status_port: 0x601, s5_sleep_type: 5
    };

    fn slots() -> Vec<VirtioSlot> {
        vec![VirtioSlot { addr: 0xd000_0000, size: 0x200, gsi: 5 },
             VirtioSlot { addr: 0xd001_0000, size: 0x200, gsi: 6 }]
    }

    #[test]
    fn rsdp_checksums_and_xsdt() {
        let r = rsdp(0x1234_5678_9abc);
        assert_eq!(r.len(), 36);
        assert_eq!(&r[..8], b"RSD PTR ");
        assert!(sums_to_zero(&r[..20]));
        assert!(sums_to_zero(&r));
        assert_eq!(r[15], 2);
        assert_eq!(le32(&r, 20), 36);
        assert_eq!(le64(&r, 24), 0x1234_5678_9abc);
    }

    #[test]
    fn tables_have_length_and_checksum() {
        for table in [dsdt(&[]), dsdt(&dsdt_aml(&PLATFORM, &slots())), fadt(0xe0000, &PLATFORM),
                      madt(1), madt(4), xsdt(&[1, 2])] {
            assert_eq!(le32(&table, 4) as usize, table.len());
            assert!(sums_to_zero(&table), "{:?}", std::str::from_utf8(&table[..4]));
        }
    }

    #[test]
    fn fadt_fields() {
        let f = fadt(0xe0040, &PLATFORM);
        assert_eq!(&f[..4], b"FACP");
        assert_eq!(f.len(), 276);
        assert_eq!(f[8], 6);
        assert_eq!(f[131], 5);
        assert_eq!(le32(&f, 112), FADT_HW_REDUCED_ACPI);
        assert_eq!(le64(&f, 140), 0xe0040);
        /* The 32-bit DSDT field stays 0: X_DSDT is the one used. */
        assert_eq!(le32(&f, 40), 0);
        /* Sleep control and status: I/O space, 8 bits, byte access. */
        assert_eq!(&f[244..256], &[1, 8, 0, 1, 0x00, 0x06, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&f[256..268], &[1, 8, 0, 1, 0x01, 0x06, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn madt_entries() {
        let m = madt(3);
        assert_eq!(&m[..4], b"APIC");
        assert_eq!(le32(&m, 36), LOCAL_APIC_ADDR);
        /* 44: the first local APIC; 3 of 8 bytes, IOAPIC 12, NMI 6. */
        assert_eq!(m.len(), 44 + 3 * 8 + 12 + 6);
        for id in 0..3 {
            let e = &m[44 + 8 * id..];
            assert_eq!(&e[..4], &[MADT_LOCAL_APIC, 8, id as u8, id as u8]);
            assert_eq!(le32(e, 4), MADT_LOCAL_APIC_ENABLED);
        }
        let ioapic = &m[44 + 24..];
        assert_eq!(&ioapic[..2], &[MADT_IOAPIC, 12]);
        assert_eq!(le32(ioapic, 4), IOAPIC_ADDR);
    }

    #[test]
    fn aml_package_lengths() {
        use aml::*;
        /* Scope(\_SB) {}: a one-byte PkgLength counting itself. */
        assert_eq!(root_scope("_SB", &[]), [0x10, 6, b'\\', b'_', b'S', b'B', b'_']);
        /* 63 bytes in all, PkgLength included, still take one byte. */
        let s = root_scope("_SB", &[0; 57]);
        assert_eq!(&s[..2], &[0x10, 63]);
        /* One more takes two: 0x40 | the low nibble, then the rest. */
        let s = root_scope("_SB", &[0; 58]);
        assert_eq!(&s[..3], &[0x10, 0x40 | (65 & 0xf), 65 >> 4]);
        assert_eq!(s.len(), 1 + 65);
        /* 4096 + 3 bytes in all need three. */
        let s = root_scope("_SB", &[0; 4096 - 2]);
        let total = 4096 - 2 + 5 + 3;
        assert_eq!(&s[..4], &[0x10, 0x80 | (total & 0xf) as u8, (total >> 4) as u8, (total >> 12) as u8]);
    }

    #[test]
    fn aml_values() {
        use aml::*;
        assert_eq!(integer(0), [0x00]);
        assert_eq!(integer(1), [0x01]);
        assert_eq!(integer(5), [0x0a, 5]);
        assert_eq!(integer(0x1234), [0x0b, 0x34, 0x12]);
        assert_eq!(integer(0xd000_0000), [0x0c, 0, 0, 0, 0xd0]);
        assert_eq!(string("LNRO0005"), b"\x0dLNRO0005\0");
        /* The well-known values: EisaId("PNP0501") is 0x0105d041. */
        assert_eq!(eisa_id("PNP0501"), [0x0c, 0x41, 0xd0, 0x05, 0x01]);
        assert_eq!(eisa_id("PNP0303"), [0x0c, 0x41, 0xd0, 0x03, 0x03]);
    }

    #[test]
    fn aml_resources() {
        use aml::*;
        assert_eq!(io(0x3f8, 8), [0x47, 1, 0xf8, 0x03, 0xf8, 0x03, 1, 8]);
        assert_eq!(irq_no_flags(4), [0x22, 0x10, 0x00]);
        assert_eq!(memory32_fixed(0xd000_0000, 0x1000), [0x86, 9, 0, 1, 0, 0, 0, 0xd0, 0, 0x10, 0, 0]);
        assert_eq!(interrupt_edge(5), [0x89, 6, 0, 0x03, 1, 5, 0, 0, 0]);
        /* Buffer(size) { descriptors, end tag }. */
        assert_eq!(resource_template(&[irq_no_flags(1)]), [0x11, 8, 0x0a, 5, 0x22, 2, 0, 0x79, 0]);
    }

    #[test]
    fn dsdt_names_each_device() {
        let a = dsdt_aml(&PLATFORM, &slots());
        let has = |needle: &[u8]| a.windows(needle.len()).any(|w| w == needle);
        /* Name(_S5_, Package() { 5, 0, 0, 0 }), first, at the root. */
        assert_eq!(&a[..12], &[0x08, b'_', b'S', b'5', b'_', 0x12, 7, 4, 0x0a, 5, 0, 0]);
        for name in [&b"COM1"[..], b"PS2K", b"VR00", b"VR01", b"LNRO0005"] {
            assert!(has(name), "{:?} missing", std::str::from_utf8(name));
        }
        assert!(has(&aml::memory32_fixed(0xd001_0000, 0x200)));
        assert!(has(&aml::interrupt_edge(6)));
    }

    #[test]
    fn written_tables_chain_from_the_rsdp() {
        let mut mem = vec![0u8; 0x1000];
        let base = 0xe0000;
        let mut alloc = BumpAllocator::new(base, base + mem.len() as u64, mem.as_mut_ptr());
        let rsdp_addr = write_acpi_tables(&mut alloc, 2, &PLATFORM, &slots()).unwrap();
        assert_eq!(rsdp_addr % 16, 0);
        let at = |gpa: u64| &mem[(gpa - base) as usize..];
        let xsdt = at(le64(at(rsdp_addr), 24));
        assert_eq!(&xsdt[..4], b"XSDT");
        let fadt = at(le64(xsdt, 36));
        let madt = at(le64(xsdt, 44));
        assert_eq!(&fadt[..4], b"FACP");
        assert_eq!(&madt[..4], b"APIC");
        assert_eq!(madt[44 + 8 + 3], 1); /* the second vcpu's APIC ID */
        assert_eq!(&at(le64(fadt, 140))[..4], b"DSDT");
    }
}
