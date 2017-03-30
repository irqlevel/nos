MB_ALIGN    equ  1<<0
MB_MEMINFO  equ  1<<1
MB_FLAGS    equ  MB_ALIGN | MB_MEMINFO
MB_MAGIC    equ  0x1BADB002
MB_CHECKSUM equ -(MB_MAGIC + MB_FLAGS)

section .multiboot
align 4
dd MB_MAGIC
dd MB_FLAGS
dd MB_CHECKSUM

section .bootstrap_stack, nobits
align 4
stack_bottom:
resb 16384
stack_top:

section .text
global _start
_start:
	mov esp, stack_top
	extern kernel_main
	call kernel_main
	sub esp, 4
	mov [esp], dword 0x0
	extern __cxa_finalize
	call __cxa_finalize
	add esp, 4
	cli
.hang:
	hlt
	jmp .hang
