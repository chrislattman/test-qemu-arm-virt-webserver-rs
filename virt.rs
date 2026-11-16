//! QEMU "virt" machine memory map and interrupt numbers.
//! See hw/arm/virt.c in the QEMU source, or dump the device tree with
//! `qemu-system-aarch64 -M virt,dumpdtb=virt.dtb` and `dtc virt.dtb`.

// --- GICv2 ---
pub const GIC_DIST_BASE: usize = 0x0800_0000;
pub const GIC_CPU_BASE: usize = 0x0801_0000;

// --- UART PL011 ---
pub const UART0_BASE: usize = 0x0900_0000;

// --- virtio-mmio transports: 32 slots, 0x200 bytes apart ---
pub const VIRTIO_MMIO_BASE: usize = 0x0a00_0000;
pub const VIRTIO_MMIO_STRIDE: usize = 0x200;
pub const VIRTIO_MMIO_COUNT: usize = 32;

// --- Interrupt IDs as seen by the GIC ---
// PPIs are IDs 16-31; SPI n is ID 32 + n
pub const IRQ_EL1_PHYS_TIMER: u32 = 30; // PPI 14: EL1 physical timer

/// virtio-mmio slot n is SPI 16 + n
pub const fn irq_virtio_mmio(slot: usize) -> u32 {
    32 + 16 + slot as u32
}
