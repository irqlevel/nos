section .text
global gobios

real_idt:
	dw 0x3ff	; 256 entries, 4b each = 1K
	dd 0		; Real Mode IVT @ 0x0000

prot_cr0:
	dd 0

prot_idt:
	dw 3

prot_gdt:
	dw 3

prot_esp:
	dd 1
prot_ebp:
	dd 1

prot_cs:
	dw 1
prot_ds:
	dw 1
prot_es:
	dw 1
prot_fs:
	dw 1
prot_gs:
	dw 1
prot_ss:
	dw 1

REAL_MODE_STACK	equ 0x8000

enter_real_mode:
	BITS 32
	cli
	sidt [prot_idt]
	sgdt [prot_gdt]
	mov [prot_ebp], ebp
	mov [prot_esp], esp
	mov eax, [esp]
	mov [REAL_MODE_STACK], eax	; Save return address
	mov [prot_cs], cs
	mov [prot_ds], ds
	mov [prot_es], es
	mov [prot_fs], fs
	mov [prot_gs], gs
	mov [prot_ss], ss

	mov eax, cr0
	mov [prot_cr0], eax
	and eax, 0x7FFFFFFE	; Disable paging
	mov cr0, eax
	jmp 0:real_mode_start
real_mode_start:
	BITS 16
	mov sp, REAL_MODE_STACK
	xor ax, ax
	mov ds, ax
	mov es, ax
	mov fs, ax
	mov gs, ax
	mov ss, ax
	lidt [real_idt]
	sti
	xor eax, eax
	ret

leave_real_mode:
	BITS 16
	cli
	lidt [prot_idt]
	lgdt [prot_gdt]
	mov eax, [prot_cr0]
	mov cr0, eax
	mov eax, [esp]
	mov [REAL_MODE_STACK], eax
	jmp prot_cs:prot_mode_start
prot_mode_start:
	BITS 32
	mov ds, [prot_ds]
	mov es, [prot_es]
	mov fs, [prot_fs]
	mov gs, [prot_gs]
	mov ss, [prot_ss]

	mov eax, [REAL_MODE_STACK]
	mov esp, [prot_esp]
	mov ebp, [prot_ebp]
	mov [esp], eax
	sti
	xor eax, eax
	ret

gobios:
	BITS 32
	call enter_real_mode
	BITS 16
	call leave_real_mode
	BITS 32
	ret
