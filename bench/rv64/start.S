.section .text.start,"ax"
.global _start
_start:
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop
    li sp, 0x80f00000
    call main
    slli a0, a0, 1
    ori a0, a0, 1
    la t0, tohost
    sd a0, 0(t0)
    # Linux userspace execution provides an independent CRC check via QEMU.
    srli a0, a0, 1
    li a7, 93
    ecall
1:  j 1b
.section .tohost,"aw"
.balign 8
.global tohost
tohost: .quad 0
# QEMU's spike machine discovers the HTIF exit device by these symbols and
# requires the 8-byte size on each; fromhost is its response channel. The
# directives live in the symbol table only: the executed stream, the entry
# point and the tohost watch address 0x80001000 are unchanged, and the flat
# image grows by eight zero bytes at 0x80001008.
fromhost: .quad 0
.size tohost, 8
.size fromhost, 8
