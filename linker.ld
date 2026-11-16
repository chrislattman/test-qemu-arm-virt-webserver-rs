ENTRY(_start)

SECTIONS
{
    . = 0x40080000; /* RAM base + 0x80000: where QEMU loads a -kernel image (DTB sits at RAM base) */

    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    .rodata : { *(.rodata .rodata.*) }
    .eh_frame : { KEEP(*(.eh_frame)) } /* Unwind info, used by debuggers for backtraces */
    .data : { *(.data .data.*) }

    /* Zeroed by startup.S. Includes the embassy tasks (with their socket
       buffers) and the virtio rings and buffers */
    .bss (NOLOAD) : {
        . = ALIGN(16);
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    /* The only stack: the executor's loop and the interrupt handlers. Tasks
       don't have stacks of their own; their state lives in .bss. Measured
       peak use under load: about 3KB in release builds, but about 70KB in
       debug builds, which copy large futures around on the stack. Nothing
       catches an overflow: the stack would grow down into the end of .bss */
    . = ALIGN(16);
    . += 0x40000; /* 256KB */
    _stack_top = .;
}
