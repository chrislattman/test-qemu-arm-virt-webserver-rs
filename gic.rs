//! Minimal GICv2 driver: distributor and CPU interface setup, and interrupt
//! acknowledgement for the IRQ handler.

use crate::virt::{GIC_CPU_BASE, GIC_DIST_BASE};

const GICD_CTLR: *mut u32 = GIC_DIST_BASE as *mut u32;
const GICD_ISENABLER: usize = GIC_DIST_BASE + 0x100;
const GICD_IPRIORITYR: usize = GIC_DIST_BASE + 0x400;
const GICD_ITARGETSR: usize = GIC_DIST_BASE + 0x800;

const GICC_CTLR: *mut u32 = GIC_CPU_BASE as *mut u32;
const GICC_PMR: *mut u32 = (GIC_CPU_BASE + 0x004) as *mut u32;
const GICC_IAR: *const u32 = (GIC_CPU_BASE + 0x00C) as *const u32;
const GICC_EOIR: *mut u32 = (GIC_CPU_BASE + 0x010) as *mut u32;

pub const SPURIOUS_ID: u32 = 1023;

pub fn init() {
    unsafe {
        GICD_CTLR.write_volatile(0); // Disable distributor for configuration
        GICC_CTLR.write_volatile(0); // Disable CPU interface for configuration

        // Leave all interrupts in Group 0 (the reset default). GICD_CTLR/GICC_CTLR = 1
        // only enable Group 0, and Group 1 interrupts can't be acknowledged via
        // GICC_IAR without GICC_CTLR.AckCtl, which would cause an endless IRQ storm.

        // Allow all interrupt priorities through. Interrupts don't nest here
        // (handlers run with IRQs masked), so priorities don't matter.
        GICC_PMR.write_volatile(0xFF);

        GICD_CTLR.write_volatile(1);
        GICC_CTLR.write_volatile(1);
    }
}

pub fn enable(id: u32) {
    let id = id as usize;
    unsafe {
        ((GICD_IPRIORITYR + id) as *mut u8).write_volatile(0xA0);
        if id >= 32 {
            // SPIs: deliver to CPU 0 (SGI/PPI targets are fixed)
            ((GICD_ITARGETSR + id) as *mut u8).write_volatile(0x01);
        }
        ((GICD_ISENABLER + 4 * (id / 32)) as *mut u32).write_volatile(1 << (id % 32));
    }
}

/// Acknowledges the highest priority pending interrupt, returning GICC_IAR
/// (the interrupt ID is the low 10 bits)
pub fn acknowledge() -> u32 {
    unsafe { GICC_IAR.read_volatile() }
}

pub fn end_of_interrupt(iar: u32) {
    unsafe { GICC_EOIR.write_volatile(iar) }
}
