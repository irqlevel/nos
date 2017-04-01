BITS 32

section .text

extern IO8042Interrupt
extern DummyInterrupt
extern SerialInterrupt

global get_cr0_32
global get_cr1_32
global get_cr2_32
global get_cr3_32
global get_cr4_32
global get_esp_32
global get_eip_32
global get_eflags_32
global pause_32
global get_cs_32
global get_ds_32
global get_ss_32
global get_es_32
global get_fs_32
global get_gs_32
global get_idt_32
global put_idt_32
global get_gdt_32
global spin_lock_lock_32
global spin_lock_unlock_32
global check_cpuid_support_32
global check_long_mode_support_32
global outb
global inb
global enable
global disable
global hlt
global IO8042InterruptStub
global DummyInterruptStub
global SerialInterruptStub

get_cr0_32:
	mov eax, cr0
	ret

get_cr1_32:
	mov eax, cr1
	ret

get_cr2_32:
	mov eax, cr2
	ret

get_cr3_32:
	mov eax, cr3
	ret

get_cr4_32:
	mov eax, cr4
	ret

get_esp_32:
	mov eax, esp
	ret

get_eip_32:
	call get_eip_32_f
get_eip_32_f:
	pop eax
	ret

get_eflags_32:
	pushfd
	pop eax
	ret

pause_32:
	pause
	ret

get_cs_32:
	xor eax, eax
	mov ax, cs
	ret

get_ds_32:
	xor eax, eax
	mov ax, ds
	ret

get_ss_32:
	xor eax, eax
	mov ax, es
	ret

get_es_32:
	xor eax, eax
	mov ax, es
	ret

get_fs_32:
	xor eax, eax
	mov ax, fs
	ret

get_gs_32:
	xor eax, eax
	mov ax, gs
	ret

get_idt_32:
	push ebp
	mov ebp, esp
	mov eax, [ebp+8]
	sidt [eax]
	mov esp, ebp
	pop ebp
	ret

put_idt_32:
	push ebp
	mov ebp, esp
	mov eax, [ebp+8]
	lidt [eax]
	mov esp ,ebp
	pop ebp
	ret

get_gdt_32:
	push ebp
	mov ebp, esp
	mov eax, [ebp+8]
	sgdt [eax]
	mov esp, ebp
	pop ebp
	ret

spin_lock_lock_32:
	push ebp
	mov ebp, esp
	push edx
	push eax
	push ecx
	mov edx, [ebp + 8]
spin_lock_lock32_again:
	xor eax, eax
	xor ecx, ecx
	inc ecx
	lock cmpxchg [edx], ecx
	je spin_lock_lock32_complete
	pause
	jmp spin_lock_lock32_again
spin_lock_lock32_complete:
	pop ecx
	pop eax
	pop edx
	mov esp, ebp
	pop ebp
	ret

spin_lock_unlock_32:
	push ebp
	mov ebp, esp
	push edx
	push ecx
	mov edx, [ebp + 8]
	xor ecx, ecx
	lock and [edx], ecx
	pop ecx
	pop edx
	mov esp, ebp
	pop ebp
	ret

check_cpuid_support_32:
	pushfd			; Store the FLAGS-register.
	pop eax			; Restore the A-register.
	mov ecx, eax		; Set the C-register to the A-register.
	xor eax, 1 << 21	; Flip the ID-bit, which is bit 21.
	push eax		; Store the A-register.
	popfd			; Restore the FLAGS-register.
	pushfd			; Store the FLAGS-register.
	pop eax			; Restore the A-register.
	push ecx		; Store the C-register.
	popfd			; Restore the FLAGS-register.
	xor eax, ecx		; Do a XOR-operation on the A-register and the C-register.
	jz no_cpuid		; The zero flag is set, no CPUID.
	xor eax, eax
	inc eax
	ret
no_cpuid:
	xor eax, eax
	ret

check_long_mode_support_32:
	call check_cpuid_support_32
	test eax, eax
	jz no_long_mode_support
	mov eax, 0x80000000	; Set the A-register to 0x80000000.
	cpuid			; CPU identification.
	cmp eax, 0x80000001	; Compare the A-register with 0x80000001.
	jb no_long_mode_support	; It is less, there is no long mode.
	mov eax, 0x80000001	; Set the A-register to 0x80000001.
	cpuid			; CPU identification.
	test edx, 1 << 29	; Test if the LM-bit, which is bit 29, is set in the D-register.
	jz no_long_mode_support ; They aren't, there is no long mode.
	xor eax, eax
	inc eax
	ret
no_long_mode_support:
	xor eax, eax
	ret

outb:
	push ebp
	mov ebp, esp
        push edx
        mov edx, [ebp + 8]
        mov eax, [ebp + 12]
        out dx, al
        mov esp, ebp
        pop ebp
        ret

inb:
        push ebp
        mov ebp, esp
        push edx
        mov edx, [ebp + 8]
        xor eax, eax
        in al, dx
        mov esp, ebp
        pop ebp
        ret

enable:
        sti
        ret

disable:
        cli
        ret

hlt:
        hlt
        ret

IO8042InterruptStub:
	pushad
	cld
	call IO8042Interrupt
	popad
	iret

DummyInterruptStub:
	pushad
	cld
	call DummyInterrupt
	popad
	iret

SerialInterruptStub:
	pushad
	cld
	call SerialInterrupt
	popad
	iret