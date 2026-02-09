extern Main
extern ApMain

global Start32
global ApStart16

%define MAX_CPUS 8
%define CPU_STACK_SIZE 4096

section .trampolinedata nobits
align 4096
p4_table:
    resb 4096
align 4096
p3_low_table:
    resb 4096
align 4096
p2_low_table:
    resb 4 * 4096
align 4096
p3_high_table:
    resb 4096
align 4096
p2_high_table:
    resb 4 * 4096
align 4096
stack_bottom:
    resb CPU_STACK_SIZE * MAX_CPUS
stack_top:

section .trampolinerodata
gdt64:
    dq 0 ; zero entry
.code: equ $ - gdt64 ; new
    dq (1<<43) | (1<<44) | (1<<47) | (1<<53) ; code segment
.pointer:
    dw $ - gdt64 - 1
    dq gdt64

gdt32:
    dq 0x0000000000000000       ; Null Descriptor
.code equ $ - gdt32                 ; Code segment
    dq 0x00cf9a000000ffff
.data equ $ - gdt32                 ; Data segment
    dq 0x00cf92000000ffff
.desc:
    dw $ - gdt32 - 1            ; 16-bit Size (Limit)
    dd gdt32                    ; 32-bit Base Address

BITS 16
section .trampoline
ApStart16:
    lgdt [gdt32.desc]
    mov eax, cr0
    or al, 0x01
    mov cr0, eax
    jmp gdt32.code:ap_start32_stub         ; Jump to 32-bit code
ap_start32_stub:
    BITS 32
    jmp gdt32.code:ap_start32         ; Jump to 32-bit code

BITS 32
section .multiboot
align 8
    dd 0xe85250d6                ; magic number (multiboot 2)
    dd 0                         ; architecture 0 (protected mode i386)
    dd 0x18			 ; header length
    ; checksum
    dd 0x17ADAF12

    ; insert optional multiboot tags here

    ; required end tag
    dw 0    ; type
    dw 0    ; flags
    dd 8    ; size

section .trampoline
align 4
stack_counter:
    dd 0
align 8
mbinfo:
    dq 0
align 8
mbsig:
    dq 0

%macro AllocStack 0
    xor eax, eax
    inc eax
    lock xadd dword [stack_counter], eax
    cmp eax, MAX_CPUS
    jge .nostack
    mov ebx, CPU_STACK_SIZE
    mul ebx
    mov edx, stack_top
    sub edx, eax
    mov eax, edx
    jmp .out
.nostack:
    xor eax, eax
    mov al, "3"
    jmp error
.out:
%endmacro

; Prints `ERR: ` and the given error code to screen and hangs.
; parameter: error code (in ascii) in al
error:
    mov dword [0xb8000], 0x4f524f45
    mov dword [0xb8004], 0x4f3a4f52
    mov dword [0xb8008], 0x4f204f20
    mov byte  [0xb800a], al
    cli
    hlt

check_multiboot:
    cmp eax, 0x36d76289
    jne .no_multiboot
    ret

.no_multiboot:
    mov al, "0"
    jmp error

check_cpuid:
    ; Check if CPUID is supported by attempting to flip the ID bit (bit 21)
    ; in the FLAGS register. If we can flip it, CPUID is available.

    ; Copy FLAGS in to EAX via stack
    pushfd
    pop eax

    ; Copy to ECX as well for comparing later on
    mov ecx, eax

    ; Flip the ID bit
    xor eax, 1 << 21

    ; Copy EAX to FLAGS via the stack
    push eax
    popfd

    ; Copy FLAGS back to EAX (with the flipped bit if CPUID is supported)
    pushfd
    pop eax

    ; Restore FLAGS from the old version stored in ECX (i.e. flipping the
    ; ID bit back if it was ever flipped).
    push ecx
    popfd

    ; Compare EAX and ECX. If they are equal then that means the bit
    ; wasn't flipped, and CPUID isn't supported.
    cmp eax, ecx
    je .no_cpuid
    ret
.no_cpuid:
    mov al, "1"
    jmp error

check_long_mode:
    ; test if extended processor info in available
    mov eax, 0x80000000    ; implicit argument for cpuid
    cpuid                  ; get highest supported argument
    cmp eax, 0x80000001    ; it needs to be at least 0x80000001
    jb .no_long_mode       ; if it's less, the CPU is too old for long mode

    ; use extended info to test if long mode is available
    mov eax, 0x80000001    ; argument for extended processor info
    cpuid                  ; returns various feature bits in ecx and edx
    test edx, 1 << 29      ; test if the LM-bit is set in the D-register
    jz .no_long_mode       ; If it's not set, there is no long mode
    ret
.no_long_mode:
    mov al, "2"
    jmp error

setup_low_page_tables:
    ; map first P4 entry to P3 table
    mov eax, p3_low_table
    or eax, 0b10011 ; present + writable + cache disabled
    mov [p4_table], eax

    mov ebx, 0
    mov edi, 0
    mov esi, p2_low_table
.map_p3_table:
    ; map first P3 entry to P2 table

    mov eax, esi
    or eax, 0b10011 ; present + writable + cache disabled
    mov [p3_low_table + edi * 8], eax

   ; map each P2 entry to a huge 2MiB page
    mov ecx, 0         ; counter variable

.map_p2_table:
    ; map ecx-th P2 entry to a huge page that starts at address 2MiB*ecx
    mov eax, 0x200000  ; 2MiB
    mul ecx            ; start address of ecx-th page

    add eax, ebx

    or eax, 0b10010011 ; present + writable + cache disabled + huge
    mov [esi + ecx * 8], eax ; map ecx-th entry

    inc ecx            ; increase counter
    cmp ecx, 512       ; if counter == 512, the whole P2 table is mapped
    jne .map_p2_table  ; else map the next entry
    inc edi
    add esi, 4096
    add ebx, 0x40000000 ; 1GB
    cmp edi, 4
    jne .map_p3_table

    ret

setup_high_page_tables:
    ; map first P4 entry to P3 table
    mov eax, p3_high_table
    or eax, 0b10011 ; present + writable + cache disabled
    mov [p4_table + 256 * 8], eax

    mov ebx, 0
    mov edi, 0
    mov esi, p2_high_table
.map_p3_table:
    ; map first P3 entry to P2 table

    mov eax, esi
    or eax, 0b10011 ; present + writable + cache disabled
    mov [p3_high_table + edi * 8], eax

   ; map each P2 entry to a huge 2MiB page
    mov ecx, 0         ; counter variable

.map_p2_table:
    ; map ecx-th P2 entry to a huge page that starts at address 2MiB*ecx
    mov eax, 0x200000  ; 2MiB
    mul ecx            ; start address of ecx-th page

    add eax, ebx

    or eax, 0b10010011 ; present + writable + cache disabled + huge
    mov [esi + ecx * 8], eax ; map ecx-th entry

    inc ecx            ; increase counter
    cmp ecx, 512       ; if counter == 512, the whole P2 table is mapped
    jne .map_p2_table  ; else map the next entry
    inc edi
    add esi, 4096
    add ebx, 0x40000000 ; 1GB
    cmp edi, 4
    jne .map_p3_table

    ret

enable_paging:
    ; load P4 to cr3 register (cpu uses this to access the P4 table)
    mov eax, p4_table
    mov cr3, eax

    ; enable PAE-flag in cr4 (Physical Address Extension)
    mov eax, cr4
    or eax, 1 << 5
    mov cr4, eax

    ; set the long mode bit in the EFER MSR (model specific register)
    mov ecx, 0xC0000080
    rdmsr
    or eax, 1 << 8
    wrmsr

    ; enable paging in the cr0 register
    mov eax, cr0
    or eax, 1 << 31
    mov cr0, eax

    ret

Start32:
    mov [mbsig], eax
    mov [mbinfo], ebx
    AllocStack
    mov esp, eax
    mov eax, [mbsig]
    call check_multiboot
    call check_cpuid
    call check_long_mode
    call setup_low_page_tables
    call setup_high_page_tables
    call enable_paging
    ; load the 64-bit GDT
    lgdt [gdt64.pointer]
    jmp gdt64.code:long_mode_start

ap_start32:
    AllocStack
    mov esp, eax
    call enable_paging
    ; load the 64-bit GDT
    lgdt [gdt64.pointer]
    jmp gdt64.code:long_mode_ap_start

BITS 64
long_mode_start:
    ; load 0 into all data segment registers
    mov ax, 0
    mov ss, ax
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
start64:
    mov rdi, [mbinfo]
    mov rax, Main
    call rax

long_mode_ap_start:
    ; load 0 into all data segment registers
    mov ax, 0
    mov ss, ax
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
ap_start64:
    mov rax, ApMain
    call rax
