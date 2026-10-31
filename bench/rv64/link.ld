OUTPUT_ARCH(riscv)
ENTRY(_start)
SECTIONS {
    . = 0x80000000;
    .text.start : { *(.text.start) }
    . = 0x80001000;
    .tohost : { *(.tohost) }
    .text ALIGN(4096) : { *(.text .text.*) }
    .rodata ALIGN(4096) : { *(.rodata .rodata.*) }
    .data ALIGN(4096) : { *(.data .data.*) }
    __global_pointer$ = . + 0x800;
    .sdata : { *(.sdata .sdata.*) }
    .bss : { *(.bss .bss.* .sbss .sbss.* COMMON) }
    . = 0x80ef0000;
    .stack (NOLOAD) : { . += 0x10000; }
}
