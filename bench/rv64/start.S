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
