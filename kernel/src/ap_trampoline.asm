extern _startup, __ap_stack_pointers

section .ap_text

bits 16
realmode:
    cli
    cld

    mov eax, cr4
    or eax, 1 << 5 ; Enable PAE
    or eax, 1 << 7 ; Enable PGE 
    mov cr4, eax

    ; Set PML4 address
    mov eax, [__kernel_pml4]
    mov cr3, eax

    ; Check for NXE support.
    mov eax, 0x80000001
    cpuid
    and edx, 1 << 20    ; Check if EFER.NXE bit is available
    jz .nxe_enable_fail  ; If not, skip setting it in IA32_EFER.
    mov ecx, 0xC0000080
    rdmsr
    or eax, 1 << 11     ; NXE bit in IA32_EFER
    wrmsr               

    .nxe_enable_fail:

    ; Enable long mode
    mov ecx, 0xC0000080     ; IA32_EFER MSR
    rdmsr
    or eax, 1 << 8          ; Set LME bit
    wrmsr

    ; Set PME & PGE bits
    mov eax, cr0
    or eax, 1 << 31 | 1 << 0
    mov cr0, eax

    ; Serialize pipeline after mode switch
    cpuid

    ; Set GDT & long-jump to long mode
    lgdt [__gdt.pointer]
    jmp __gdt.code:longmode


extern __bsp_init_complete

bits 64
longmode:
    cli

    ; Update segment registers  
    mov ax, __gdt.data
    mov ss, ax
    ; Clear unused segments
    xor ax, ax
    mov es, ax
    mov ds, ax
    mov fs, ax
    mov gs, ax

    .x2_apic_id:
        mov eax, 0x1F
        cpuid
        ; Test all registers to see if any bits are set
        or eax, ebx
        or eax, ecx
        or eax, edx
        test eax, eax
        ; If CPUID leaf not supported, try next source.
        jz .x2_apic_id_backup
        ; Otherwise, APIC ID is stored in `edx`.
        jmp .set_rsp
    .x2_apic_id_backup:
        mov eax, 0xB
        cpuid
        ; Test all registers to see if any bits are set
        or eax, ebx
        or eax, ecx
        or eax, edx
        test eax, eax
        ; If CPUID leaf not supported, try next source.
        jz .apic_id_legacy
        ; Otherwise, APIC ID is stored in `edx`.
        jmp .set_rsp
    .apic_id_legacy:
        ; No advanced APIC IDs are available, so rely on legacy 8-bit ID.
        mov eax, 0x1
        cpuid
        shr ebx, 24     ; APIC ID is in bits 24..32
        and ebx, 0xFF   ; `ebx` or `bl` now contains the APIC ID, so truncate any sign-extended bits
        mov edx, ebx
        jmp .set_rsp

    .set_rsp:
        ; Load effective address of pointer
        lea rsp, [__ap_stack_pointers + (rdx * 8)]
        ; Load absolute address of pointer
        mov rsp, [rsp]

    ; Jump to high-level code
    call _startup

section .ap_data

global __kernel_pml4
__kernel_pml4 resd 1

; Access bits
PRESENT        equ 1 << 7
NOT_SYS        equ 1 << 4
EXEC           equ 1 << 3
DC             equ 1 << 2
RW             equ 1 << 1
USER           equ 3 << 5
ACCESSED       equ 1 << 0

; Flags bits
GRAN_4K       equ 1 << 7
; This flag should not be present with LONG_MODE flag.
; They are mutually excuslive.
SZ_32         equ 1 << 6
LONG_MODE     equ 1 << 5

__gdt:
    .null: equ $ - __gdt
        dq 0
    .code: equ $ - __gdt
        dd 0xFFFF                           ; Limit & Base (low)
        db 0                                ; Base (mid)
        db PRESENT | NOT_SYS | EXEC | RW    ; Access
        db GRAN_4K | LONG_MODE | 0xF        ; Flags
        db 0                                ; Base (high)
    .data: equ $ - __gdt
        dd 0xFFFF                           ; Limit & Base (low)
        db 0                                ; Base (mid)
        db PRESENT | NOT_SYS | RW           ; Access
        db GRAN_4K | SZ_32 | 0xF            ; Flags
        db 0                                ; Base (high)    
    .pointer:
        dw $ - __gdt - 1
        dq __gdt