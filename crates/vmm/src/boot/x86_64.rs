//! The Linux x86 boot protocol's 64-bit entry (Documentation/arch/x86/boot.rst): places a bzImage,
//! command line, zero page, MP table, GDT and identity page tables in guest memory.
use crate::GuestMemory;
use std::io;
use thanatos_hv::{X86_GDT, X86_PML4};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};

pub const ZERO_PAGE: u64 = 0x7000;
const CMDLINE: u64 = 0x2_0000;
const KERNEL: u64 = 0x10_0000;
/// The MP table sits at the start of the EBDA, where Linux scans for it.
const EBDA: u64 = 0x9_fc00;
const IOAPIC: u32 = 0xfec0_0000;
const LOCAL_APIC: u32 = 0xfee0_0000;

/// Loads the guest and returns the kernel's 64-bit entry point.
pub fn load(memory: &GuestMemory, kernel: &[u8], cmdline: &str, vcpus: u8) -> io::Result<u64> {
    let header = Header::parse(kernel)?;
    if memory.last_addr().0 < header.pref_address + header.init_size {
        return Err(invalid("guest memory too small for the kernel"));
    }
    if cmdline.len() >= header.cmdline_size {
        return Err(invalid("kernel command line too long"));
    }
    put(memory, KERNEL, &kernel[header.payload_offset..])?;
    put(memory, CMDLINE, cmdline.as_bytes())?;
    put(memory, CMDLINE + cmdline.len() as u64, &[0])?;
    put(memory, ZERO_PAGE, &zero_page(memory, kernel))?;
    put(memory, EBDA, &mp_table(vcpus))?;
    page_tables(memory)?;
    // Null, null, 64-bit code at selector 0x10, data at 0x18: the selectors the kernel expects.
    let gdt = [0u64, 0, 0x00af_9b00_0000_ffff, 0x00cf_9300_0000_ffff];
    put(memory, X86_GDT, &gdt.map(u64::to_le_bytes).concat())?;
    Ok(KERNEL + 0x200)
}

struct Header {
    payload_offset: usize,
    pref_address: u64,
    init_size: u64,
    cmdline_size: usize,
}

impl Header {
    fn parse(kernel: &[u8]) -> io::Result<Self> {
        if kernel.len() < 0x268 || &kernel[0x202..0x206] != b"HdrS" {
            return Err(invalid("guest kernel is not a bzImage"));
        }
        // 2.12 added xloadflags, whose bit 0 advertises the 64-bit entry point.
        if u16_at(kernel, 0x206) < 0x020c || u16_at(kernel, 0x236) & 1 == 0 {
            return Err(invalid("guest kernel has no 64-bit boot entry"));
        }
        let setup_sects = match kernel[0x1f1] {
            0 => 4,
            sectors => sectors as usize,
        };
        let payload_offset = (setup_sects + 1) * 512;
        if payload_offset >= kernel.len() {
            return Err(invalid("guest kernel is truncated"));
        }
        Ok(Self {
            payload_offset,
            pref_address: u64::from(u32_at(kernel, 0x258)) | u64::from(u32_at(kernel, 0x25c)) << 32,
            init_size: u32_at(kernel, 0x260).into(),
            cmdline_size: u32_at(kernel, 0x238) as usize,
        })
    }
}

fn zero_page(memory: &GuestMemory, kernel: &[u8]) -> [u8; 4096] {
    let mut page = [0u8; 4096];
    // The setup header runs from 0x1f1 to the end its own length byte at 0x201 declares.
    let end = 0x202 + kernel[0x201] as usize;
    page[0x1f1..end].copy_from_slice(&kernel[0x1f1..end]);
    page[0x210] = 0xff; // type_of_loader: undefined
    page[0x228..0x22c].copy_from_slice(&(CMDLINE as u32).to_le_bytes());
    let mut ram = vec![(0, EBDA)];
    for region in memory.iter() {
        let (start, size) = (region.start_addr().0, region.len());
        // The first region's low megabyte holds the boot tables and the legacy hole.
        ram.push(if start == 0 {
            (KERNEL, size - KERNEL)
        } else {
            (start, size)
        });
    }
    page[0x1e8] = ram.len() as u8;
    for (index, (address, size)) in ram.into_iter().enumerate() {
        let entry = 0x2d0 + index * 20;
        page[entry..entry + 8].copy_from_slice(&address.to_le_bytes());
        page[entry + 8..entry + 16].copy_from_slice(&size.to_le_bytes());
        page[entry + 16..entry + 20].copy_from_slice(&1u32.to_le_bytes()); // E820_TYPE_RAM
    }
    page
}

/// An Intel MultiProcessor Specification 1.4 floating pointer and configuration table: one
/// processor per vCPU with APIC IDs from 0, the ISA bus, and the in-kernel IOAPIC with each ISA
/// interrupt on the pin of the same number, as KVM's default routing delivers them.
fn mp_table(vcpus: u8) -> Vec<u8> {
    let ioapic = vcpus;
    let mut entries = Vec::new();
    for id in 0..vcpus {
        let flags = if id == 0 { 3 } else { 1 }; // enabled, and the bootstrap processor
        entries.extend([0, id, 0x14, flags]);
        entries.extend(0x600u32.to_le_bytes()); // family 6
        entries.extend(0x201u32.to_le_bytes()); // FPU, APIC
        entries.extend([0; 8]);
    }
    entries.extend(*b"\x01\x00ISA   ");
    entries.extend([2, ioapic, 0x11, 1]);
    entries.extend(IOAPIC.to_le_bytes());
    for irq in 0..16 {
        entries.extend([3, 0, 0, 0, 0, irq, ioapic, irq]);
    }
    entries.extend([4, 3, 0, 0, 0, 0, 0, 0]); // ExtINT to the bootstrap processor's LINT0
    entries.extend([4, 1, 0, 0, 0, 0, 0xff, 1]); // NMI to every LINT1
    let count = u16::from(vcpus) + 20;
    let mut table = [
        b"PCMP".as_slice(),
        &(44 + entries.len() as u16).to_le_bytes(),
        &[4, 0],
    ]
    .concat();
    table.extend(*b"THANATOS VMM        ");
    table.extend([0; 6]); // no OEM table
    table.extend(count.to_le_bytes());
    table.extend(LOCAL_APIC.to_le_bytes());
    table.extend([0; 4]); // no extended entries
    table.extend(entries);
    table[7] = checksum(&table);
    let mut pointer = [b"_MP_".as_slice(), &(EBDA as u32 + 16).to_le_bytes()].concat();
    pointer.extend([1, 4, 0, 0, 0, 0, 0, 0]); // 16 bytes, spec 1.4, a configuration table
    pointer[10] = checksum(&pointer);
    [pointer, table].concat()
}

/// The byte that makes `bytes` sum to zero.
fn checksum(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0, |sum: u8, byte| sum.wrapping_sub(*byte))
}

/// Identity-maps the first 1 GiB with 2 MiB pages; the kernel builds its own tables early.
fn page_tables(memory: &GuestMemory) -> io::Result<()> {
    let (pdpt, pd) = (X86_PML4 + 0x1000, X86_PML4 + 0x2000);
    put(memory, X86_PML4, &(pdpt | 0x3).to_le_bytes())?;
    put(memory, pdpt, &(pd | 0x3).to_le_bytes())?;
    let entries: Vec<u8> = (0..512u64)
        .flat_map(|index| ((index << 21) | 0x83).to_le_bytes())
        .collect();
    put(memory, pd, &entries)
}

fn put(memory: &GuestMemory, address: u64, bytes: &[u8]) -> io::Result<()> {
    memory
        .write_slice(bytes, GuestAddress(address))
        .map_err(|_| invalid("guest image does not fit in guest memory"))
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from(u16_at(bytes, offset)) | u32::from(u16_at(bytes, offset + 2)) << 16
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestMemoryMmap;

    fn memory(size: usize) -> GuestMemory {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), size)]).unwrap()
    }

    fn bzimage() -> Vec<u8> {
        let mut kernel = vec![0u8; 5 * 512 + 16];
        kernel[0x1f1] = 4;
        kernel[0x201] = 0x66; // header ends at 0x268
        kernel[0x202..0x206].copy_from_slice(b"HdrS");
        kernel[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes());
        kernel[0x236] = 1;
        kernel[0x238..0x23c].copy_from_slice(&2048u32.to_le_bytes());
        kernel[0x258..0x260].copy_from_slice(&0x100_0000u64.to_le_bytes());
        kernel[0x260..0x264].copy_from_slice(&0x40_0000u32.to_le_bytes());
        kernel[5 * 512] = 0xe9;
        kernel
    }

    fn at<const N: usize>(memory: &GuestMemory, address: u64) -> [u8; N] {
        let mut bytes = [0; N];
        memory
            .read_slice(&mut bytes, GuestAddress(address))
            .unwrap();
        bytes
    }

    #[test]
    fn the_zero_page_and_mp_table_match_their_golden_blob() {
        let memory = memory(64 << 20);
        load(&memory, &bzimage(), "console=ttyS0", 2).unwrap();
        let mut blob = at::<4096>(&memory, ZERO_PAGE).to_vec();
        blob.extend_from_slice(&mp_table(2));
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/boot/golden/x86_64.bin");
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(path, &blob).unwrap();
        }
        assert_eq!(blob, std::fs::read(path).unwrap());
    }

    #[test]
    fn places_the_kernel_and_boot_parameters() {
        let memory = memory(64 << 20);
        let entry = load(&memory, &bzimage(), "console=ttyS0", 2).unwrap();
        assert_eq!(entry, 0x10_0200);
        assert_eq!(at::<1>(&memory, 0x10_0000), [0xe9]);
        assert_eq!(at::<1>(&memory, ZERO_PAGE + 0x210), [0xff]);
        assert_eq!(&at::<4>(&memory, ZERO_PAGE + 0x202), b"HdrS");
        assert_eq!(at::<1>(&memory, ZERO_PAGE + 0x1e8), [2]);
        assert_eq!(&at::<14>(&memory, CMDLINE), b"console=ttyS0\0");
        assert_eq!(u64::from_le_bytes(at(&memory, X86_PML4)), 0xa003);
        assert_eq!(u64::from_le_bytes(at(&memory, 0xb000 + 8)), 0x20_0083);
        assert_eq!(&at::<4>(&memory, EBDA), b"_MP_");
    }

    #[test]
    fn the_mp_table_lists_every_vcpu_and_checks_out() {
        let table = mp_table(3);
        let sum = |bytes: &[u8]| bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        assert_eq!(sum(&table[..16]), 0);
        let config = &table[16..];
        assert_eq!(&config[..4], b"PCMP");
        assert_eq!(
            u16::from_le_bytes([config[4], config[5]]) as usize,
            config.len()
        );
        assert_eq!(sum(config), 0);
        assert_eq!(u16::from_le_bytes([config[34], config[35]]), 23);
    }

    #[test]
    fn rejects_images_that_cannot_boot() {
        let big = memory(64 << 20);
        let mut old = bzimage();
        old[0x236] = 0;
        assert!(load(&big, &old, "", 1).is_err());
        assert!(load(&big, b"not a kernel", "", 1).is_err());
        assert!(load(&memory(19 << 20), &bzimage(), "", 1).is_err());
    }
}
