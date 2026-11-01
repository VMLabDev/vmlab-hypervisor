.section .text.start,"ax"
.global _start
_start:
    ldr x9, =0x40f00000
    mov sp, x9
    bl main
    adr x9, exit_status
    str x0, [x9, #8]
    lsl x10, x0, #1
    orr x10, x10, #1
    adr x11, tohost
    str x10, [x11]
    mov x0, #0x20
    mov x1, x9
    hlt #0xf000
1:  b 1b

.section .tohost,"aw"
.balign 8
.global tohost
.type tohost, %object
tohost: .quad 0
.size tohost, 8
exit_status: .quad 0x20026, 0
