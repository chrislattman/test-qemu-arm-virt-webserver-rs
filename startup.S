// Boot code and exception vectors for the QEMU "virt" machine.
//
// QEMU's -kernel loader enters here at EL1 (virt has no EL2/EL3 unless
// virtualization=on or secure=on), with the MMU off and IRQs masked.
// Everything runs at EL1 on the SP_EL1 stack: the embassy executor's loop
// (platform.rs) and the interrupt handlers.

// These are assembler constants rather than #defines: global_asm! (main.rs)
// doesn't run the C preprocessor.

// MAIR_EL1: Attr0 = 0x00 (Device-nGnRnE), Attr1 = 0xFF (Normal, Inner/Outer
// Write-Back Write-Allocate)
.set MAIR_VALUE,        0xFF << 8

// TCR_EL1: 4KB granule, 32-bit (4GB) VA and PA space, TTBR0 walks only
.set TCR_T0SZ,          64 - 32             // VA size 2^32: lookup starts at level 1
.set TCR_IRGN0_WBWA,    1 << 8              // Table walks: inner write-back cacheable
.set TCR_ORGN0_WBWA,    1 << 10             // Table walks: outer write-back cacheable
.set TCR_SH0_INNER,     3 << 12             // Table walks: inner shareable
.set TCR_EPD1,          1 << 23             // No TTBR1 (upper VA range) walks
// TG0 = 0 (4KB granule) and IPS = 0 (4GB physical addresses)
.set TCR_VALUE,         TCR_T0SZ | TCR_IRGN0_WBWA | TCR_ORGN0_WBWA | TCR_SH0_INNER | TCR_EPD1

// Level 1 block descriptors (each entry maps 1GB)
.set PTE_BLOCK,         0x1
.set PTE_ATTR_DEVICE,   0 << 2              // MAIR Attr0
.set PTE_ATTR_NORMAL,   1 << 2              // MAIR Attr1
.set PTE_SH_INNER,      3 << 8
.set PTE_AF,            1 << 10             // Access flag, so the first access doesn't fault
.set PTE_PXN,           1 << 53
.set PTE_UXN,           1 << 54
.set PTE_DEVICE,        PTE_BLOCK | PTE_ATTR_DEVICE | PTE_AF | PTE_PXN | PTE_UXN
.set PTE_NORMAL,        PTE_BLOCK | PTE_ATTR_NORMAL | PTE_SH_INNER | PTE_AF

.set SCTLR_M,           1 << 0              // MMU enable
.set SCTLR_A,           1 << 1              // Alignment check enable
.set SCTLR_C,           1 << 2              // Data cache enable
.set SCTLR_I,           1 << 12             // Instruction cache enable

.section ".text.boot", "ax"
.global _start

_start:
    .cfi_startproc
    .cfi_undefined x30          // Outermost frame for debuggers
    // Force CPU cores other than Core 0 into an infinite sleep loop
    mrs     x0, mpidr_el1
    and     x0, x0, #0xFF
    cbz     x0, master_cpu
hang:
    wfe
    b       hang

master_cpu:
    ldr     x0, =_stack_top
    mov     sp, x0

    ldr     x0, =vector_table
    msr     vbar_el1, x0

    // Enable the MMU with an identity map. With the MMU off, every data access
    // is treated as Device memory, where unaligned accesses and atomic
    // (exclusive) accesses aren't guaranteed to work; embassy relies on atomics.
    // The FPU stays off: the softfloat target never uses FP/SIMD registers.
    // (On real hardware the caches would need invalidating first; QEMU doesn't
    // model them.)
    ldr     x0, =MAIR_VALUE
    msr     mair_el1, x0
    ldr     x0, =TCR_VALUE
    msr     tcr_el1, x0
    ldr     x0, =page_table_l1
    msr     ttbr0_el1, x0
    isb
    tlbi    vmalle1
    dsb     nsh
    isb
    mrs     x0, sctlr_el1
    orr     x0, x0, #SCTLR_M
    orr     x0, x0, #SCTLR_C
    orr     x0, x0, #SCTLR_I
    bic     x0, x0, #SCTLR_A
    msr     sctlr_el1, x0
    isb

    // Zero .bss, which Rust requires for zero-initialized statics
    ldr     x0, =__bss_start
    ldr     x1, =__bss_end
zero_bss:
    cmp     x0, x1
    b.hs    zero_bss_done
    str     xzr, [x0], #8
    b       zero_bss
zero_bss_done:

    // IRQs stay masked until the executor's loop first unmasks them
    bl      main

    // Defensive fallback hang loop
    b       hang
    .cfi_endproc

// Identity map of the low 4GB. virt puts all MMIO (GIC, UART, virtio-mmio,
// ...) below 1GB and RAM from 1GB up.
.section ".rodata.page_table", "a"
.balign 4096
page_table_l1:
    .quad   0x00000000 | PTE_DEVICE     // 0-1GB: MMIO
    .quad   0x40000000 | PTE_NORMAL     // 1-2GB: RAM
    .quad   0                           // 2-4GB: unmapped (PCIe windows, unused here)
    .quad   0

.text

// --- ARMv8 Exception Vector Table ---
// Everything runs at EL1 with SP_EL1, so only the "Current EL with SP_ELx"
// IRQ entry is expected; anything else is a fault.
.macro vfault kind
    .balign 128
    mov     x0, #\kind
    b       fault_entry
.endm

.balign 2048
vector_table:
    // Current Exception Level with SP_EL0 (offsets 0x000 - 0x180): never used
    vfault  0; vfault 1; vfault 2; vfault 3

    // Current Exception Level with SP_ELx (offsets 0x200 - 0x380)
    vfault  0                   // Synchronous
    .balign 128
    b       irq_entry           // IRQ
    vfault  2                   // FIQ
    vfault  3                   // SError

    // Lower Exception Level AArch64 (offsets 0x400 - 0x580): never used
    vfault  0; vfault 1; vfault 2; vfault 3

    // Lower Exception Level AArch32 (offsets 0x600 - 0x780): never used
    vfault  0; vfault 1; vfault 2; vfault 3

// Saves the registers a Rust function may clobber, calls handle_irq
// (platform.rs) and returns to the interrupted code. IRQs stay masked
// throughout (the CPU masks them on exception entry), so handlers don't nest
// and ELR_EL1/SPSR_EL1 don't need saving. No FP/SIMD registers to save either,
// since the softfloat target never uses them.
irq_entry:
    .cfi_startproc
    .cfi_undefined x30          // Outermost frame for debuggers: return address is in ELR_EL1, not x30
    sub     sp, sp, #(20 * 8)
    stp     x0, x1, [sp, #(0 * 8)]
    stp     x2, x3, [sp, #(2 * 8)]
    stp     x4, x5, [sp, #(4 * 8)]
    stp     x6, x7, [sp, #(6 * 8)]
    stp     x8, x9, [sp, #(8 * 8)]
    stp     x10, x11, [sp, #(10 * 8)]
    stp     x12, x13, [sp, #(12 * 8)]
    stp     x14, x15, [sp, #(14 * 8)]
    stp     x16, x17, [sp, #(16 * 8)]
    stp     x18, x30, [sp, #(18 * 8)]

    bl      handle_irq

    ldp     x0, x1, [sp, #(0 * 8)]
    ldp     x2, x3, [sp, #(2 * 8)]
    ldp     x4, x5, [sp, #(4 * 8)]
    ldp     x6, x7, [sp, #(6 * 8)]
    ldp     x8, x9, [sp, #(8 * 8)]
    ldp     x10, x11, [sp, #(10 * 8)]
    ldp     x12, x13, [sp, #(12 * 8)]
    ldp     x14, x15, [sp, #(14 * 8)]
    ldp     x16, x17, [sp, #(16 * 8)]
    ldp     x18, x30, [sp, #(18 * 8)]
    add     sp, sp, #(20 * 8)
    eret
    .cfi_endproc

fault_entry:
    .cfi_startproc
    .cfi_undefined x30
    mrs     x1, esr_el1
    mrs     x2, elr_el1
    mrs     x3, far_el1
    bl      handle_fault        // Doesn't return
    b       hang
    .cfi_endproc
