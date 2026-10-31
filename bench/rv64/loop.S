# The scalar-loop interpreter workload as a flat image (bench/micro.py).
#
# `micro loop N` (crates/vmlab-hv-tcg/examples/micro.rs) executes exactly
# these three encodings N times with x1 starting at 0 and x2 preset to N:
# 3*N instructions, x1 ending at N. The block is position-independent (the
# bne offset is -8), so it sits here behind a constant two-instruction x2
# stub at the common entry 0x80000000. The interpreter workload presets x2
# with set_gpr instead of executing the stub, and stops at the tohost store
# instead of executing the exit after it; everything between is identical.
# With LOOP_ITERATIONS=1 the same image is the QEMU startup baseline: one
# loop iteration plus the eight stub and exit instructions. Zero would loop
# 2^64 times: the bne tests x2 after the decrement, so `micro loop` rejects
# it too.
.section .text.start,"ax"
.global _start
_start:
    lui   x2, %hi(LOOP_ITERATIONS)
    addi  x2, x2, %lo(LOOP_ITERATIONS)
    .word 0x00108093         # addi x1, x1, 1
    .word 0xfff10113         # addi x2, x2, -1
    .word 0xfe011ce3         # bne  x2, x0, -8
    .option push
    .option norelax
    la    t0, tohost          # 0x80001000 per bench/rv64/link.ld
    .option pop
    li    a0, 0
    slli  a0, a0, 1
    ori   a0, a0, 1          # (exit code << 1) | 1
    sd    a0, 0(t0)          # HTIF exit; the interpreter stops here
1:  j 1b
.section .tohost,"aw"
.balign 8
.global tohost
tohost: .quad 0
fromhost: .quad 0
.size tohost, 8
.size fromhost, 8
