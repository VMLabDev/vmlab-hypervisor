OUTPUT_ARCH(aarch64)
ENTRY(_start)
SECTIONS {
    . = 0x40080000;
    .text.start : { *(.text.start) }
    . = 0x40081000;
    .tohost : { *(.tohost) }
    .text ALIGN(4096) : { *(.text .text.*) }
    .rodata ALIGN(4096) : { *(.rodata .rodata.*) }
    .data ALIGN(4096) : { *(.data .data.*) }
    .bss : { *(.bss .bss.* COMMON) }
    . = 0x40ef0000;
    .stack (NOLOAD) : { . += 0x10000; }
}
