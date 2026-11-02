; A standalone guest for S1 debugger tests. Enter long mode with identity and
; high-half aliases for the first 2 MiB, then access one eight-byte RAM value.
bits 16
org 0

%define ROM_BASE 0xf0000
%define HIGH_BASE 0xffff800000000000

start:
    cli
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x7000
    lgdt [cs:gdt_pointer]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp dword 0x08:(ROM_BASE + protected_mode)

align 8
gdt:
    dq 0
    ; Descriptor accessed bits are pre-set because the GDT is in ROM.
    dq 0x00cf9b000000ffff
    dq 0x00cf93000000ffff
    dq 0x00af9b000000ffff
gdt_end:
gdt_pointer:
    dw gdt_end - gdt - 1
    dd ROM_BASE + gdt

bits 32
protected_mode:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    xor eax, eax
    mov edi, 0x1000
    mov ecx, 0x3000 / 4
    rep stosd
    mov dword [0x1000], 0x2003
    mov dword [0x1800], 0x2003
    mov dword [0x2000], 0x3003
    mov dword [0x3000], 0x83
    mov eax, 0x1000
    mov cr3, eax
    mov eax, cr4
    or eax, 0x20
    mov cr4, eax
    mov ecx, 0xc0000080
    rdmsr
    or eax, 0x100
    wrmsr
    mov eax, cr0
    or eax, 0x80000000
    mov cr0, eax
    jmp 0x18:(ROM_BASE + long_mode)

bits 64
long_mode:
    mov rsp, 0x7000
    mov rax, HIGH_BASE + ROM_BASE + high_entry
    jmp rax
high_entry:
    mov rax, HIGH_BASE + 0x9000
    mov qword [rax], 0x12345678
    inc qword [rax]
    mov rbx, [rax]
    jmp $

times 0xfff0 - ($ - $$) db 0xf4
bits 16
    jmp 0xf000:start
times 0x10000 - ($ - $$) db 0xf4
