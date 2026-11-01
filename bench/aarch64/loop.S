.section .text,"ax"
.global main
main:
    movz x9, #(LOOP_ITERATIONS & 0xffff)
    movk x9, #((LOOP_ITERATIONS >> 16) & 0xffff), lsl #16
    mov x10, #0
1:  add x10, x10, #1
    subs x9, x9, #1
    b.ne 1b
    mov x0, #0
    ret
