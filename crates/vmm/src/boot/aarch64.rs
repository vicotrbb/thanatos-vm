//! The arm64 boot protocol (Documentation/arch/arm64/booting.rst): an `Image` and a device tree.
use crate::GuestMemory;
use std::io;
use thanatos_hv::{GIC_DISTRIBUTOR, GIC_REDISTRIBUTOR};
use vm_fdt::FdtWriter;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};

pub const RAM_BASE: u64 = 0x8000_0000;
pub const SERIAL_BASE: u64 = 0x0900_0000;
pub const SERIAL_SPI: u32 = 1;
pub const RTC_BASE: u64 = 0x0901_0000;
const GIC_PHANDLE: u32 = 1;
const CLOCK_PHANDLE: u32 = 2;
const REDISTRIBUTOR_SIZE: u64 = 0x2_0000;
const DTB_MAX: u64 = 2 << 20;
const IRQ_LEVEL_HIGH: u32 = 4;

/// Loads the guest and returns the entry point and the device tree's address.
pub fn load(
    memory: &GuestMemory,
    kernel: &[u8],
    cmdline: &str,
    vcpus: u32,
    devices: &[(u64, u64, u32)],
) -> io::Result<(u64, u64)> {
    if kernel.len() < 64 || &kernel[0x38..0x3c] != b"ARM\x64" {
        return Err(invalid("guest kernel is not an arm64 Image"));
    }
    // The protocol places the Image text_offset bytes above a 2 MiB boundary.
    let text_offset = u64::from_le_bytes(kernel[8..16].try_into().unwrap());
    let entry = RAM_BASE + text_offset;
    let end = memory.last_addr().0 + 1;
    let dtb_address = (end - DTB_MAX) & !(DTB_MAX - 1);
    if entry + kernel.len() as u64 > dtb_address {
        return Err(invalid("guest memory too small for the kernel"));
    }
    let size: u64 = memory.iter().map(|region| region.len()).sum();
    let dtb = device_tree(size, cmdline, vcpus, devices).map_err(io::Error::other)?;
    let put = |address, bytes: &[u8]| {
        memory
            .write_slice(bytes, GuestAddress(address))
            .map_err(|_| invalid("guest image does not fit in guest memory"))
    };
    put(entry, kernel)?;
    put(dtb_address, &dtb)?;
    Ok((entry, dtb_address))
}

fn device_tree(
    memory_size: u64,
    cmdline: &str,
    vcpus: u32,
    devices: &[(u64, u64, u32)],
) -> Result<Vec<u8>, vm_fdt::Error> {
    let mut fdt = FdtWriter::new()?;
    let root = fdt.begin_node("")?;
    fdt.property_string("compatible", "linux,dummy-virt")?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_u32("interrupt-parent", GIC_PHANDLE)?;

    let chosen = fdt.begin_node("chosen")?;
    fdt.property_string("bootargs", cmdline)?;
    fdt.property_string("stdout-path", &format!("/pl011@{SERIAL_BASE:x}"))?;
    fdt.end_node(chosen)?;

    let node = fdt.begin_node(&format!("memory@{RAM_BASE:x}"))?;
    fdt.property_string("device_type", "memory")?;
    fdt.property_array_u64("reg", &[RAM_BASE, memory_size])?;
    fdt.end_node(node)?;

    let cpus = fdt.begin_node("cpus")?;
    fdt.property_u32("#address-cells", 1)?;
    fdt.property_u32("#size-cells", 0)?;
    for id in 0..vcpus {
        let cpu = fdt.begin_node(&format!("cpu@{id:x}"))?;
        fdt.property_string("device_type", "cpu")?;
        fdt.property_string("compatible", "arm,arm-v8")?;
        fdt.property_string("enable-method", "psci")?;
        fdt.property_u32("reg", id)?;
        fdt.end_node(cpu)?;
    }
    fdt.end_node(cpus)?;

    let psci = fdt.begin_node("psci")?;
    fdt.property_string("compatible", "arm,psci-0.2")?;
    fdt.property_string("method", "hvc")?;
    fdt.end_node(psci)?;

    let gic = fdt.begin_node(&format!("intc@{GIC_DISTRIBUTOR:x}"))?;
    fdt.property_string("compatible", "arm,gic-v3")?;
    fdt.property_null("interrupt-controller")?;
    fdt.property_u32("#interrupt-cells", 3)?;
    fdt.property_u32("phandle", GIC_PHANDLE)?;
    fdt.property_array_u64(
        "reg",
        &[
            GIC_DISTRIBUTOR,
            0x1_0000,
            GIC_REDISTRIBUTOR,
            u64::from(vcpus) * REDISTRIBUTOR_SIZE,
        ],
    )?;
    fdt.end_node(gic)?;

    let timer = fdt.begin_node("timer")?;
    fdt.property_string("compatible", "arm,armv8-timer")?;
    fdt.property_null("always-on")?;
    let ppi = |number| [1, number, IRQ_LEVEL_HIGH];
    fdt.property_array_u32("interrupts", &[ppi(13), ppi(14), ppi(11), ppi(10)].concat())?;
    fdt.end_node(timer)?;

    let clock = fdt.begin_node("apb-pclk")?;
    fdt.property_string("compatible", "fixed-clock")?;
    fdt.property_u32("#clock-cells", 0)?;
    fdt.property_u32("clock-frequency", 24_000_000)?;
    fdt.property_u32("phandle", CLOCK_PHANDLE)?;
    fdt.end_node(clock)?;

    let uart = fdt.begin_node(&format!("pl011@{SERIAL_BASE:x}"))?;
    fdt.property_string_list(
        "compatible",
        vec!["arm,pl011".into(), "arm,primecell".into()],
    )?;
    fdt.property_array_u64("reg", &[SERIAL_BASE, 0x1000])?;
    fdt.property_array_u32("interrupts", &[0, SERIAL_SPI, IRQ_LEVEL_HIGH])?;
    fdt.property_array_u32("clocks", &[CLOCK_PHANDLE, CLOCK_PHANDLE])?;
    fdt.property_string_list("clock-names", vec!["uartclk".into(), "apb_pclk".into()])?;
    fdt.end_node(uart)?;

    let rtc = fdt.begin_node(&format!("pl031@{RTC_BASE:x}"))?;
    fdt.property_string_list(
        "compatible",
        vec!["arm,pl031".into(), "arm,primecell".into()],
    )?;
    fdt.property_array_u64("reg", &[RTC_BASE, 0x1000])?;
    fdt.property_array_u32("clocks", &[CLOCK_PHANDLE])?;
    fdt.property_string("clock-names", "apb_pclk")?;
    fdt.end_node(rtc)?;

    for &(base, length, spi) in devices {
        let node = fdt.begin_node(&format!("virtio_mmio@{base:x}"))?;
        fdt.property_string("compatible", "virtio,mmio")?;
        fdt.property_array_u64("reg", &[base, length])?;
        fdt.property_array_u32("interrupts", &[0, spi, IRQ_LEVEL_HIGH])?;
        fdt.property_null("dma-coherent")?;
        fdt.end_node(node)?;
    }

    fdt.end_node(root)?;
    fdt.finish()
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestMemoryMmap;

    #[test]
    fn the_device_tree_matches_its_golden_blob() {
        let tree =
            device_tree(512 << 20, "console=ttyAMA0", 2, &[(0x0a00_0000, 0x200, 32)]).unwrap();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/boot/golden/aarch64.dtb");
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(path, &tree).unwrap();
        }
        assert_eq!(tree, std::fs::read(path).unwrap());
    }

    #[test]
    fn loads_at_the_text_offset_and_rejects_what_cannot_boot() {
        let memory: GuestMemory =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(RAM_BASE), 16 << 20)]).unwrap();
        let mut image = vec![0u8; 128];
        image[0] = 0x4d;
        image[8] = 0;
        image[9] = 0;
        image[10] = 8; // text_offset 0x80000
        image[0x38..0x3c].copy_from_slice(b"ARM\x64");
        let (entry, dtb) = load(&memory, &image, "x", 1, &[]).unwrap();
        assert_eq!(entry, RAM_BASE + 0x8_0000);
        assert_eq!(memory.read_obj::<u8>(GuestAddress(entry)).unwrap(), 0x4d);
        assert_eq!(dtb, RAM_BASE + (14 << 20));
        assert_eq!(
            memory.read_obj::<u32>(GuestAddress(dtb)).unwrap().to_be(),
            0xd00d_feed
        );
        assert!(load(&memory, b"not a kernel", "x", 1, &[]).is_err());
        let mut large = image.clone();
        large.resize(15 << 20, 0);
        assert!(load(&memory, &large, "x", 1, &[]).is_err());
    }
}
