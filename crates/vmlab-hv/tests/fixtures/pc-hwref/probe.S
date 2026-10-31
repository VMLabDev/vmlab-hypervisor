.section .boot,"ax"
.code16
.global boot
boot:
    cli
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw $0x7c00, %sp
    sti
    movw $dap, %si
    movb $0x42, %ah
    int $0x13
    jc boot_failed
    ljmp $0x1000, $0
boot_failed:
    movw $0x3f8, %dx
    movb $'!', %al
    outb %al, %dx
    cli
1:  hlt
    jmp 1b
.balign 4
dap:
    .byte 16, 0
    .word 32, 0, 0x1000
    .quad 1
.org 510
.word 0xaa55

.section .payload,"ax"
.code16
.global payload
payload:
    cld
    xorl %ebx, %ebx
    xorl %ebp, %ebp
    movw $0x5000, %di
e820_next:
    movl $0xe820, %eax
    movl $0x534d4150, %edx
    movl $20, %ecx
    int $0x15
    jc e820_done
    cmpl $0x534d4150, %eax
    jne e820_done
    incl %ebp
    addw $20, %di
    cmpl $128, %ebp
    jae e820_done
    testl %ebx, %ebx
    jnz e820_next
e820_done:
    movl %ebp, %cs:(e820_count - payload)
    cli
    inb $0x92, %al
    andb $0xfe, %al
    orb $2, %al
    outb %al, $0x92
    lgdtl %cs:(gdtr - payload)
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl $8, $protected
.balign 8
gdt:
    .quad 0, 0x00cf9a000000ffff, 0x00cf92000000ffff
gdtr:
    .word 23
    .long gdt
e820_count:
    .long 0

.code32
protected:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl $0x90000, %esp
    movw $0x3fb, %dx
    movb $0x83, %al
    outb %al, %dx
    movw $0x3f8, %dx
    movb $1, %al
    outb %al, %dx
    incw %dx
    xorb %al, %al
    outb %al, %dx
    movw $0x3fb, %dx
    movb $3, %al
    outb %al, %dx
    decw %dx
    movb $7, %al
    outb %al, %dx
    movw $0x3fc, %dx
    movb $0x0b, %al
    outb %al, %dx
    movl $magic, %esi
    movl $8, %ecx
1:  lodsb
    call putc
    loop 1b

    movl e820_count, %ecx
    imull $20, %ecx
    movl $0x5000, %esi
    movb $1, %al
    call emit

    xorl %ebp, %ebp
pci_next:
    movl %ebp, %eax
    shll $8, %eax
    orl $0x80000000, %eax
    movw $0xcf8, %dx
    outl %eax, %dx
    movw $0xcfc, %dx
    inl %dx, %eax
    cmpw $0xffff, %ax
    je pci_skip
    movw %bp, 0x7000
    xorl %ebx, %ebx
pci_dword:
    movl %ebp, %eax
    shll $8, %eax
    orl $0x80000000, %eax
    orl %ebx, %eax
    movw $0xcf8, %dx
    outl %eax, %dx
    movw $0xcfc, %dx
    inl %dx, %eax
    movl %eax, 0x7002(%ebx)
    addl $4, %ebx
    cmpl $256, %ebx
    jb pci_dword
    movb $2, %al
    movl $0x7000, %esi
    movl $258, %ecx
    call emit
pci_skip:
    incl %ebp
    cmpl $256, %ebp
    jb pci_next

    movl $0xe0000, %esi
rsdp_next:
    cmpl $0x20445352, (%esi)
    jne rsdp_skip
    cmpl $0x20525450, 4(%esi)
    jne rsdp_skip
    movl %esi, %edi
    movl $20, %ecx
    xorl %ebx, %ebx
1:  movb (%edi), %al
    addb %al, %bl
    incl %edi
    loop 1b
    testb %bl, %bl
    jz rsdp_found
rsdp_skip:
    addl $16, %esi
    cmpl $0xffff0, %esi
    jb rsdp_next
    jmp failed
rsdp_found:
    movl 16(%esi), %edi
    movl $20, %ecx
    movb $3, %al
    call emit
    movl %edi, %esi
    call emit_table
    movl 4(%edi), %ebp
    subl $36, %ebp
    shrl $2, %ebp
    addl $36, %edi
acpi_next:
    testl %ebp, %ebp
    jz smbios_scan
    movl (%edi), %esi
    call emit_table
    cmpl $0x50434146, (%esi)
    jne acpi_skip
    pushl %esi
    movl 40(%esi), %esi
    call emit_table
    popl %esi
    pushl %esi
    movl 36(%esi), %esi
    call emit_table
    popl %esi
acpi_skip:
    addl $4, %edi
    decl %ebp
    jmp acpi_next

smbios_scan:
    movl $0xf0000, %esi
smbios_next:
    cmpl $0x5f4d535f, (%esi)
    jne smbios_skip
    cmpb $31, 5(%esi)
    jne smbios_skip
    cmpl $0x494d445f, 16(%esi)
    jne smbios_skip
    cmpb $0x5f, 20(%esi)
    jne smbios_skip
    movl %esi, %edi
    movl $31, %ecx
    xorl %ebx, %ebx
1:  movb (%edi), %al
    addb %al, %bl
    incl %edi
    loop 1b
    testb %bl, %bl
    jz smbios_found
smbios_skip:
    addl $16, %esi
    cmpl $0xffff0, %esi
    jb smbios_next
    jmp failed
smbios_found:
    movzbl 5(%esi), %ecx
    movb $5, %al
    call emit
    movzwl 22(%esi), %ecx
    movl 24(%esi), %esi
    movb $6, %al
    call emit

    movw $0x510, %dx
    movw $0x19, %ax
    outw %ax, %dx
    movl $0x20000, %edi
    movl $4, %ecx
    call fw_read
    movl 0x20000, %eax
    bswap %eax
    cmpl $512, %eax
    ja failed
    movl %eax, %ebp
    shll $6, %eax
    movl %eax, %ecx
    movl $0x20004, %edi
    call fw_read
    addl $4, %ecx
    movl $0x20000, %esi
    movb $7, %al
    call emit
    movl $0x20004, %ebx
fw_next:
    testl %ebp, %ebp
    jz cpuid_dump
    movl (%ebx), %ecx
    bswap %ecx
    cmpl $0x40000, %ecx
    ja failed
    movzwl 4(%ebx), %eax
    rolw $8, %ax
    movw %ax, 0x40000
    movw $0x510, %dx
    outw %ax, %dx
    movl $0x40002, %edi
    call fw_read
    addl $2, %ecx
    movl $0x40000, %esi
    movb $8, %al
    call emit
    addl $64, %ebx
    decl %ebp
    jmp fw_next

cpuid_dump:
    movl $1, %eax
    cpuid
    movl %eax, 0x7000
    movl %edx, 0x7004
    movl $0x80000000, %eax
    cpuid
    movl $32, 0x7008
    cmpl $0x80000008, %eax
    jb cpuid_done
    movl $0x80000008, %eax
    cpuid
    andl $0xff, %eax
    movl %eax, 0x7008
cpuid_done:
    movl $0x7000, %esi
    movl $12, %ecx
    movb $9, %al
    call emit
    xorl %ebx, %ebx
cmos_next:
    movb %bl, %al
    orb $0x80, %al
    outb %al, $0x70
    inb $0x71, %al
    movb %al, 0x7000(%ebx)
    incl %ebx
    cmpl $128, %ebx
    jb cmos_next
    movl $0x7000, %esi
    movl $128, %ecx
    movb $10, %al
    call emit
    xorl %eax, %eax
    xorl %ecx, %ecx
    call emit
halted:
    cli
    hlt
    jmp halted

failed:
    movb $255, %al
    movl $error, %esi
    movl $error_end-error, %ecx
    call emit
    jmp halted

emit_table:
    movl 4(%esi), %ecx
    cmpl $8, %ecx
    jb failed
    cmpl $0x20000, %ecx
    ja failed
    movb $4, %al
    jmp emit

fw_read:
    pushal
    movw $0x511, %dx
    testl %ecx, %ecx
    jz 2f
1:  inb %dx, %al
    stosb
    loop 1b
2:  popal
    ret

emit:
    pushal
    call putc
    movl %ecx, %eax
    call put32
    testl %ecx, %ecx
    jz 2f
1:  lodsb
    call putc
    loop 1b
2:  popal
    ret
put32:
    pushl %ecx
    movl $4, %ecx
1:  call putc
    shrl $8, %eax
    loop 1b
    popl %ecx
    ret
putc:
    pushl %eax
    pushl %edx
    movb %al, %ah
    movw $0x3fd, %dx
1:  inb %dx, %al
    testb $0x20, %al
    jz 1b
    movw $0x3f8, %dx
    movb %ah, %al
    outb %al, %dx
    popl %edx
    popl %eax
    ret
magic:
    .ascii "VMLABPC1"
error:
    .ascii "missing or oversized firmware structure"
error_end:
