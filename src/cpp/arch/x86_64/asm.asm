BITS 64

section .text

extern dummy_interrupt_dispatch
extern spurious_interrupt
extern io8042_interrupt
extern serial_interrupt
extern pit_interrupt
extern hpet_interrupt
extern ip_interrupt
extern lapic_timer_interrupt
extern posted_interrupt
extern shared_interrupt
extern dispatch_rust_interrupt
extern dispatch_rust_msix

extern exc_divide_by_zero
extern exc_debugger
extern exc_nmi
extern exc_breakpoint
extern exc_overflow
extern exc_bounds
extern exc_invalid_opcode
extern exc_coprocessor_not_available
extern exc_double_fault
extern exc_coprocessor_segment_overrun
extern exc_invalid_task_state_segment
extern exc_segment_not_present
extern exc_stack_fault
extern exc_general_protection_fault
extern exc_page_fault
extern exc_reserved
extern exc_math_fault
extern exc_alignment_check
extern exc_machine_check
extern exc_simd_fp_exception
extern exc_virt_exception
extern exc_control_protection

global get_cr0
global get_cr1
global get_cr2
global get_cr3
global get_cr4
global set_cr3
global get_rsp
global get_rbp
global get_rip
global get_rflags
global set_rflags
global cpu_relax
global get_cs
global get_ds
global get_ss
global get_es
global get_fs
global get_gs
global load_idt
global store_idt
global load_gdt
global store_gdt
global load_tr
global spin_lock_lock
global spin_lock_unlock
global outb
global inb
global outw
global inw
global outl
global inl
global read_msr
global write_msr
global interrupt_enable
global interrupt_disable
global cpu_halt
global set_rsp
global set_rbp
global read_tsc
global read_tscp
global cpuid_call

global switch_context

global atomic_inc
global atomic_dec
global atomic_add
global atomic_read
global atomic_write
global atomic_read_and_dec
global atomic_read_and_inc
global atomic_test_and_set_bit
global atomic_test_and_clear_bit
global atomic_test_bit
global atomic_cmpxchg

global dummy_interrupt_stub_table
global spurious_interrupt_stub
global io8042_interrupt_stub
global serial_interrupt_stub
global pit_interrupt_stub
global hpet_interrupt_stub
global ip_interrupt_stub
global lapic_timer_interrupt_stub
global posted_interrupt_stub
global shared_interrupt_stub
global rust_interrupt_stub0
global rust_interrupt_stub1
global rust_interrupt_stub2
global rust_interrupt_stub3
global rust_interrupt_stub4
global rust_interrupt_stub5
global rust_interrupt_stub6
global rust_interrupt_stub7
global rust_msix_stub0
global rust_msix_stub1
global rust_msix_stub2
global rust_msix_stub3
global rust_msix_stub4
global rust_msix_stub5
global rust_msix_stub6
global rust_msix_stub7
global rust_msix_stub8
global rust_msix_stub9
global rust_msix_stub10
global rust_msix_stub11
global rust_msix_stub12
global rust_msix_stub13
global rust_msix_stub14
global rust_msix_stub15
global rust_msix_stub16
global rust_msix_stub17
global rust_msix_stub18
global rust_msix_stub19
global rust_msix_stub20
global rust_msix_stub21
global rust_msix_stub22
global rust_msix_stub23
global rust_msix_stub24
global rust_msix_stub25
global rust_msix_stub26
global rust_msix_stub27
global rust_msix_stub28
global rust_msix_stub29
global rust_msix_stub30
global rust_msix_stub31

global exc_divide_by_zero_stub
global exc_debugger_stub
global exc_nmi_stub
global exc_breakpoint_stub
global exc_overflow_stub
global exc_bounds_stub
global exc_invalid_opcode_stub
global exc_coprocessor_not_available_stub
global exc_double_fault_stub
global exc_coprocessor_segment_overrun_stub
global exc_invalid_task_state_segment_stub
global exc_segment_not_present_stub
global exc_stack_fault_stub
global exc_general_protection_fault_stub
global exc_page_fault_stub
global exc_reserved_stub
global exc_math_fault_stub
global exc_alignment_check_stub
global exc_machine_check_stub
global exc_simd_fp_exception_stub
global exc_virt_exception_stub
global exc_control_protection_stub
global set_jmp
global long_jmp

get_cr0:
	mov rax, cr0
	ret

get_cr1:
	mov rax, cr1
	ret

get_cr2:
	mov rax, cr2
	ret

get_cr3:
	mov rax, cr3
	ret

set_cr3:
	mov cr3, rdi
	ret

get_cr4:
	mov rax, cr4
	ret

get_rbp:
    mov rax, rbp
    ret

get_rsp:
	mov rax, rsp
	ret

get_rip:
	call GetRipF
GetRipF:
	pop rax
	ret

get_rflags:
	pushfq
	pop rax
	ret

set_rflags:
	push rdi
	popfq
	ret

cpu_relax:
	pause
	ret

get_cs:
	xor rax, rax
	mov ax, cs
	ret

get_ds:
	xor rax, rax
	mov ax, ds
	ret

get_ss:
	xor rax, rax
	mov ax, ss
	ret

get_es:
	xor rax, rax
	mov ax, es
	ret

get_fs:
	xor rax, rax
	mov ax, fs
	ret

get_gs:
	xor rax, rax
	mov ax, gs
	ret

store_idt:
	sidt [rdi]
	ret

load_idt:
	lidt [rdi]
	ret

store_gdt:
	sgdt [rdi]
	ret

load_gdt:
	lgdt [rdi]
	ret

load_tr:
	ltr di
	ret

spin_lock_lock:
	xor rsi, rsi
	inc rsi
.again:
	xor rax, rax
	lock cmpxchg qword [rdi], rsi
	je .complete
	pause
	jmp .again
.complete:
	ret

spin_lock_unlock:
	xor rax, rax
	lock and qword [rdi], rax
	ret

outb:
	push rdx
	mov rdx, rdi
	mov rax, rsi
	out dx, al
	pop rdx
	ret

inb:
	push rdx
	mov rdx, rdi
	xor rax, rax
	in al, dx
	pop rdx
	ret

outw:
	push rdx
	mov rdx, rdi
	mov rax, rsi
	out dx, ax
	pop rdx
	ret

inw:
	push rdx
	mov rdx, rdi
	xor rax, rax
	in ax, dx
	pop rdx
	ret

outl:
	push rdx
	mov rdx, rdi
	mov rax, rsi
	out dx, eax
	pop rdx
	ret

inl:
	push rdx
	mov rdx, rdi
	xor rax, rax
	in eax, dx
	pop rdx
	ret

read_msr: ;Read MSR specified by ECX into EDX:EAX.
	mov ecx, edi
	rdmsr
	shl rdx, 32
	mov edi, eax
	or rdx, rdi
	mov rax, rdx
	ret

write_msr: ;Write the value in EDX:EAX to MSR specified by ECX.
	mov ecx, edi
	mov rdx, rsi
	mov eax, edx
	shr rdx, 32
	wrmsr
	ret

interrupt_enable:
	sti
	ret

interrupt_disable:
	cli
	ret

cpu_halt:
	hlt
	ret

set_rsp:
	mov rax, [rsp] ; save return address
	mov rsp, rdi
	push rax
	ret

set_rbp:
    mov rbp, rdi
    ret

read_tsc:
	rdtsc; result in edx:eax
	shl rdx, 32
	or rdx, rax
	mov rax, rdx
	ret

read_tscp:
	; rdi - &cpuIndex
	rdtscp; result in edx:eax, cpuIndex in ecx
	mov qword [rdi], rcx
	shl rdx, 32
	or rdx, rax
	mov rax, rdx
	ret

cpuid_call:
	; rdi = leaf, rsi = subleaf, rdx = &CpuidResult
	push rbx
	mov eax, edi
	mov ecx, esi
	mov r8, rdx
	cpuid
	mov dword [r8], eax
	mov dword [r8 + 4], ebx
	mov dword [r8 + 8], ecx
	mov dword [r8 + 12], edx
	pop rbx
	ret

%macro PushAll 0
	push rsp
	pushfq
	push rbp
	push rax
	push rbx
	push rcx
	push rdx
	push rdi
	push rsi
	push r8
	push r9
	push r10
	push r11
	push r12
	push r13
	push r14
	push r15
%endmacro

; What PushAll pushes, in bytes: the frame stubs below build their alignment
; on. Kept in step with it by hand; the sums below check what depends on it.
%assign PushAllSize 17 * 8

%macro PopAll 0
	pop r15
	pop r14
	pop r13
	pop r12
	pop r11
	pop r10
	pop r9
	pop r8
	pop rsi
	pop rdi
	pop rdx
	pop rcx
	pop rbx
	pop rax
	pop rbp
	popfq
	add rsp, 8; simulate pop rsp(see push rsp in PushAll)
%endmacro

switch_context:
	;rdi = nextTask->Rsp
	;rsi = &currTask->Rsp
	;rdx = complete callback
	;rcx = complete callback ctx
	PushAll
	mov rax, rsp
	mov rsp, rdi
	mov [rsi], rax
	mov rdi, rcx
	call rdx ; call complete callback
	PopAll
	ret

atomic_inc:
	lock inc qword [rdi]
	ret

atomic_dec:
	lock dec qword [rdi]
	ret

atomic_add:
	; rdi = *pvalue, rsi = delta
	lock xadd qword [rdi], rsi
	ret

; A sequentially consistent load on x86-64 is a plain aligned MOV. This used
; `lock xadd` with zero, which reads by way of a read-modify-write: it takes
; the cache line for exclusive ownership and dirties it, so every "read"
; invalidates that line in every other CPU's cache. Task::SelectNextTaskQueue
; reads all 64 queue counters on every context switch, and at line rate that
; was the top of the profile -- 64 cache lines stolen from their owners per
; switch, to look at numbers nobody was changing.
;
; Nothing is weakened. The C++ mapping for x86-64 is SC-load = MOV, SC-store =
; XCHG, and AtomicWrite below is already `lock xchg`; the pair is what makes
; the ordering, not the load.
atomic_read:
	mov rax, qword [rdi]
	ret

atomic_write:
	lock xchg qword [rdi], rsi
	ret

atomic_read_and_dec:
	xor rax, rax
	dec rax
	lock xadd qword [rdi], rax
	ret

atomic_read_and_inc:
	xor rax, rax
	inc rax
	lock xadd qword [rdi], rax
	ret

atomic_test_and_set_bit:
	xor rax, rax
	lock bts qword [rdi], rsi
	jnc .return
	inc rax
.return:
	ret

atomic_test_and_clear_bit:
	xor rax, rax
	lock btr qword [rdi], rsi
	jnc .return
	inc rax
.return:
	ret

atomic_test_bit:
	xor rax, rax
	bt qword [rdi], rsi
	jnc .return
	inc rax
.return:
	ret

atomic_cmpxchg:
	mov rax, rdx
	lock cmpxchg qword [rdi], rsi
	ret

; The ABI wants RSP on a 16-byte boundary at every call. The CPU aligns RSP
; to 16 and pushes SS, RSP, RFLAGS, CS and RIP, and for some exceptions an
; error code on top of those; PushAll follows. An interrupt stub then calls
; from a boundary as the frame stands; one under an error code is 8 off it,
; and pads. It did not: every #GP, #PF and the rest ran its C++ handler 8
; off the ABI, found while chasing a UBSan report of the same thing on the
; boot stacks (docs/build.md). The assembly fails if the sums stop adding up.
%define CpuFrameSize 40
%define ErrorCodeSize 8
%define ErrorCodePad 8
%if (CpuFrameSize + PushAllSize) % 16 != 0
	%error "an interrupt stub would call C++ with RSP off a 16-byte boundary"
%endif
%if (CpuFrameSize + ErrorCodeSize + PushAllSize + ErrorCodePad) % 16 != 0
	%error "an exception stub under an error code would call C++ with RSP off a 16-byte boundary"
%endif

%macro InterruptStub 1
%1_interrupt_stub:
	PushAll
	mov rdi, rsp
	cld
	call %1_interrupt
	PopAll
	iretq
%endmacro

%macro ExceptionStub 1
%1_stub:
	PushAll
	mov rdi, rsp
	cld
	call %1
	PopAll
	iretq
%endmacro

%macro ExceptionStubErr 1
%1_stub:
	PushAll
	mov rdi, rsp
	cld
	sub rsp, ErrorCodePad
	call %1
	add rsp, ErrorCodePad
	PopAll
	add rsp, ErrorCodeSize ; remove error code pushed by CPU
	iretq
%endmacro

InterruptStub spurious
InterruptStub io8042
InterruptStub serial
InterruptStub pit
InterruptStub hpet
InterruptStub ip
InterruptStub lapic_timer
InterruptStub posted
InterruptStub shared

; One entry point per IDT vector. Every slot starts out pointing here
; (Idt::Idt), and the vector number is baked into the stub so a stray
; interrupt can name itself: firmware that leaves an LVT armed, or a
; masked 8259 answering an ExtINT INTA cycle, otherwise lands in an
; anonymous panic with nothing to chase.
%assign DummyVector 0
%rep 256
dummy_interrupt_stub%+DummyVector:
	PushAll
	mov rdi, rsp
	mov esi, DummyVector
	cld
	call dummy_interrupt_dispatch
	PopAll
	iretq
%assign DummyVector DummyVector+1
%endrep

section .rodata
align 8
dummy_interrupt_stub_table:
%assign DummyVector 0
%rep 256
	dq dummy_interrupt_stub%+DummyVector
%assign DummyVector DummyVector+1
%endrep

section .text

%macro RustInterruptSlot 1
rust_interrupt_stub%1:
	PushAll
	mov rdi, rsp
	mov esi, %1
	cld
	call dispatch_rust_interrupt
	PopAll
	iretq
%endmacro

RustInterruptSlot 0
RustInterruptSlot 1
RustInterruptSlot 2
RustInterruptSlot 3
RustInterruptSlot 4
RustInterruptSlot 5
RustInterruptSlot 6
RustInterruptSlot 7

%macro RustMsixSlot 1
rust_msix_stub%1:
	PushAll
	mov rdi, rsp
	mov esi, %1
	cld
	call dispatch_rust_msix
	PopAll
	iretq
%endmacro

RustMsixSlot 0
RustMsixSlot 1
RustMsixSlot 2
RustMsixSlot 3
RustMsixSlot 4
RustMsixSlot 5
RustMsixSlot 6
RustMsixSlot 7
RustMsixSlot 8
RustMsixSlot 9
RustMsixSlot 10
RustMsixSlot 11
RustMsixSlot 12
RustMsixSlot 13
RustMsixSlot 14
RustMsixSlot 15
RustMsixSlot 16
RustMsixSlot 17
RustMsixSlot 18
RustMsixSlot 19
RustMsixSlot 20
RustMsixSlot 21
RustMsixSlot 22
RustMsixSlot 23
RustMsixSlot 24
RustMsixSlot 25
RustMsixSlot 26
RustMsixSlot 27
RustMsixSlot 28
RustMsixSlot 29
RustMsixSlot 30
RustMsixSlot 31

ExceptionStub exc_divide_by_zero
ExceptionStub exc_debugger
ExceptionStub exc_nmi
ExceptionStub exc_breakpoint
ExceptionStub exc_overflow
ExceptionStub exc_bounds
ExceptionStub exc_invalid_opcode
ExceptionStub exc_coprocessor_not_available
ExceptionStubErr exc_double_fault
ExceptionStub exc_coprocessor_segment_overrun
ExceptionStubErr exc_invalid_task_state_segment
ExceptionStubErr exc_segment_not_present
ExceptionStubErr exc_stack_fault
ExceptionStubErr exc_general_protection_fault
ExceptionStubErr exc_page_fault
ExceptionStub exc_reserved
ExceptionStub exc_math_fault
ExceptionStubErr exc_alignment_check
ExceptionStub exc_machine_check
ExceptionStub exc_simd_fp_exception
ExceptionStub exc_virt_exception
ExceptionStubErr exc_control_protection

set_jmp:
	pop rsi
	xor eax, eax
	mov qword [rdi], rbx
	mov qword [rdi + 8h], rsp
	push rsi
	mov qword [rdi + 10h], rbp
	mov qword [rdi + 18h], r12
	mov qword [rdi + 20h], r13
	mov qword [rdi + 28h], r14
	mov qword [rdi + 30h], r15
	mov qword [rdi + 38h], rsi
	ret

long_jmp:
	mov rax, rsi
	mov rbx, qword [rdi]
	mov rsp, qword [rdi + 8h]
	mov rbp, qword [rdi + 10h]
	mov r12, qword [rdi + 18h]
	mov r13, qword [rdi + 20h]
	mov r14, qword [rdi + 28h]
	mov r15, qword [rdi + 30h]
	jmp qword [rdi + 38h]
