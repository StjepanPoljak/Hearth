/* ACPI tables for Linux on x86: just enough for SMP. The guest finds the
 * RSDP through the PVH start info; the RSDP points at the XSDT, which
 * lists only the MADT: a local APIC per vcpu, and KVM's IOAPIC. With no
 * FADT, Linux takes the processors from the MADT and leaves the ACPI
 * interpreter off ("Unable to enable ACPI"); interrupts and devices stay
 * as without ACPI, on the legacy PIC's and IOAPIC's ISA IRQs.
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

/* Most vcpus the MADT can describe: local APIC entries hold 8-bit IDs,
 * and 0xff means all processors. */
pub const MAX_CPUS: u32 = 255;

/* Writes the tables through alloc and returns the RSDP's address. */
pub fn write_acpi_tables(alloc: &mut BumpAllocator, cpus: u32) -> Result<u64> {
    assert!((1..=MAX_CPUS).contains(&cpus));
    /* Each table goes in before the one that points at it. */
    let madt_addr = alloc.push_aligned(&madt(cpus), Some(8))?;
    let xsdt_addr = alloc.push_aligned(&xsdt(&[madt_addr]), Some(8))?;
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
        for table in [madt(1), madt(4), xsdt(&[1, 2])] {
            assert_eq!(le32(&table, 4) as usize, table.len());
            assert!(sums_to_zero(&table), "{:?}", std::str::from_utf8(&table[..4]));
        }
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
    fn written_tables_chain_from_the_rsdp() {
        let mut mem = vec![0u8; 0x1000];
        let base = 0xe0000;
        let mut alloc = BumpAllocator::new(base, base + mem.len() as u64, mem.as_mut_ptr());
        let rsdp_addr = write_acpi_tables(&mut alloc, 2).unwrap();
        assert_eq!(rsdp_addr % 16, 0);
        let at = |gpa: u64| &mem[(gpa - base) as usize..];
        let xsdt = at(le64(at(rsdp_addr), 24));
        assert_eq!(&xsdt[..4], b"XSDT");
        assert_eq!(le32(xsdt, 4), 36 + 8);
        let madt = at(le64(xsdt, 36));
        assert_eq!(&madt[..4], b"APIC");
        assert_eq!(madt[44 + 8 + 3], 1); /* the second vcpu's APIC ID */
    }
}
