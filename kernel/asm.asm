BITS 64

section .text

extern DummyInterrupt
extern IO8042Interrupt
extern SerialInterrupt
extern PitInterrupt
extern IPInterrupt

extern ExcDivideByZero
extern ExcDebugger
extern ExcNMI
extern ExcBreakpoint
extern ExcOverflow
extern ExcBounds
extern ExcInvalidOpcode
extern ExcCoprocessorNotAvailable
extern ExcDoubleFault
extern ExcCoprocessorSegmentOverrun
extern ExcInvalidTaskStateSegment
extern ExcSegmentNotPresent
extern ExcStackFault
extern ExcGeneralProtectionFault
extern ExcPageFault
extern ExcReserved
extern ExcMathFault
extern ExcAlignmentCheck
extern ExcMachineCheck
extern ExcSIMDFpException
extern ExcVirtException
extern ExcControlProtection

global GetCr0
global GetCr1
global GetCr2
global GetCr3
global GetCr4
global SetCr3
global GetRsp
global GetRbp
global GetRip
global GetRflags
global SetRflags
global Pause
global GetCs
global GetDs
global GetSs
global GetEs
global GetFs
global GetGs
global LoadIdt
global StoreIdt
global LoadGdt
global StoreGdt
global SpinLockLock
global SpinLockUnlock
global Outb
global Inb
global Out
global In
global ReadMsr
global WriteMsr
global InterruptEnable
global InterruptDisable
global Hlt
global SetRsp
global SetRbp
global ReadTsc
global ReadTscp

global SwitchContext

global AtomicInc
global AtomicDec
global AtomicRead
global AtomicWrite
global AtomicReadAndDec
global AtomicReadAndInc
global AtomicTestAndSetBit
global AtomicTestAndClearBit
global AtomicTestBit
global AtomicCmpxchg

global DummyInterruptStub
global IO8042InterruptStub
global SerialInterruptStub
global PitInterruptStub
global IPInterruptStub

global ExcDivideByZeroStub
global ExcDebuggerStub
global ExcNMIStub
global ExcBreakpointStub
global ExcOverflowStub
global ExcBoundsStub
global ExcInvalidOpcodeStub
global ExcCoprocessorNotAvailableStub
global ExcDoubleFaultStub
global ExcCoprocessorSegmentOverrunStub
global ExcInvalidTaskStateSegmentStub
global ExcSegmentNotPresentStub
global ExcStackFaultStub
global ExcGeneralProtectionFaultStub
global ExcPageFaultStub
global ExcReservedStub
global ExcMathFaultStub
global ExcAlignmentCheckStub
global ExcMachineCheckStub
global ExcSIMDFpExceptionStub
global ExcVirtExceptionStub
global ExcControlProtectionStub
global SetJmp
global LongJmp

GetCr0:
	mov rax, cr0
	ret

GetCr1:
	mov rax, cr1
	ret

GetCr2:
	mov rax, cr2
	ret

GetCr3:
	mov rax, cr3
	ret

SetCr3:
	mov cr3, rdi
	ret

GetCr4:
	mov rax, cr4
	ret

GetRbp:
    mov rax, rbp
    ret

GetRsp:
	mov rax, rsp
	ret

GetRip:
	call GetRipF
GetRipF:
	pop rax
	ret

GetRflags:
	pushfq
	pop rax
	ret

SetRflags:
	push rdi
	popfq
	ret

Pause:
	pause
	ret

GetCs:
	xor rax, rax
	mov ax, cs
	ret

GetDs:
	xor rax, rax
	mov ax, ds
	ret

GetSs:
	xor rax, rax
	mov ax, ss
	ret

GetEs:
	xor rax, rax
	mov ax, es
	ret

GetFs:
	xor rax, rax
	mov ax, fs
	ret

GetGs:
	xor rax, rax
	mov ax, gs
	ret

StoreIdt:
	sidt [rdi]
	ret

LoadIdt:
	lidt [rdi]
	ret

StoreGdt:
	sgdt [rdi]
	ret

LoadGdt:
	lgdt [rdi]
	ret

SpinLockLock:
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

SpinLockUnlock:
	xor rax, rax
	lock and qword [rdi], rax
	ret

Outb:
	push rdx
	mov rdx, rdi
	mov rax, rsi
	out dx, al
	pop rdx
	ret

Inb:
	push rdx
	mov rdx, rdi
	xor rax, rax
	in al, dx
	pop rdx
	ret

Out:
	push rdx
	mov rdx, rdi
	mov rax, rsi
	out dx, eax
	pop rdx
	ret

In:
	push rdx
	mov rdx, rdi
	xor rax, rax
	in eax, dx
	pop rdx
	ret

ReadMsr: ;Read MSR specified by ECX into EDX:EAX.
	mov ecx, edi
	rdmsr
	shl rdx, 32
	mov edi, eax
	or rdx, rdi
	mov rax, rdx
	ret

WriteMsr: ;Write the value in EDX:EAX to MSR specified by ECX.
	mov ecx, edi
	mov rdx, rsi
	mov eax, edx
	shr rdx, 32
	wrmsr
	ret

InterruptEnable:
	sti
	ret

InterruptDisable:
	cli
	ret

Hlt:
	hlt
	ret

SetRsp:
	mov rax, [rsp] ; save return address
	mov rsp, rdi
	push rax
	ret

SetRbp:
    mov rbp, rdi
    ret

ReadTsc:
	rdtsc; result in edx:eax
	shl rdx, 32
	or rdx, rax
	mov rax, rdx
	ret

ReadTscp:
	; rdi - &cpuIndex
	rdtscp; result in edx:eax, cpuIndex in ecx
	mov qword [rdi], rcx
	shl rdx, 32
	or rdx, rax
	mov rax, rdx
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

SwitchContext:
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

AtomicInc:
	lock inc qword [rdi]
	ret

AtomicDec:
	lock dec qword [rdi]
	ret

AtomicRead:
	xor rax, rax
	lock xadd qword [rdi], rax
	ret

AtomicWrite:
	lock xchg qword [rdi], rsi
	ret

AtomicReadAndDec:
	xor rax, rax
	dec rax
	lock xadd qword [rdi], rax
	ret

AtomicReadAndInc:
	xor rax, rax
	inc rax
	lock xadd qword [rdi], rax
	ret

AtomicTestAndSetBit:
	xor rax, rax
	lock bts qword [rdi], rsi
	jnc .return
	inc rax
.return:
	ret

AtomicTestAndClearBit:
	xor rax, rax
	lock btr qword [rdi], rsi
	jnc .return
	inc rax
.return:
	ret

AtomicTestBit:
	xor rax, rax
	bt qword [rdi], rsi
	jnc .return
	inc rax
.return:
	ret

AtomicCmpxchg:
	mov rax, rdx
	lock cmpxchg qword [rdi], rsi
	ret

%macro InterruptStub 1
%1InterruptStub:
	PushAll
	mov rdi, rsp
	cld
	call %1Interrupt
	PopAll
	iretq
%endmacro

%macro ExceptionStub 1
%1Stub:
	PushAll
	mov rdi, rsp
	cld
	call %1
	PopAll
	iretq
%endmacro

InterruptStub Dummy
InterruptStub IO8042
InterruptStub Serial
InterruptStub Pit
InterruptStub IP

ExceptionStub ExcDivideByZero
ExceptionStub ExcDebugger
ExceptionStub ExcNMI
ExceptionStub ExcBreakpoint
ExceptionStub ExcOverflow
ExceptionStub ExcBounds
ExceptionStub ExcInvalidOpcode
ExceptionStub ExcCoprocessorNotAvailable
ExceptionStub ExcDoubleFault
ExceptionStub ExcCoprocessorSegmentOverrun
ExceptionStub ExcInvalidTaskStateSegment
ExceptionStub ExcSegmentNotPresent
ExceptionStub ExcStackFault
ExceptionStub ExcGeneralProtectionFault
ExceptionStub ExcPageFault
ExceptionStub ExcReserved
ExceptionStub ExcMathFault
ExceptionStub ExcAlignmentCheck
ExceptionStub ExcMachineCheck
ExceptionStub ExcSIMDFpException
ExceptionStub ExcVirtException
ExceptionStub ExcControlProtection

SetJmp:
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

LongJmp:
	mov rax, rsi
	mov rbx, qword [rdi]
	mov rsp, qword [rdi + 8h]
	mov rbp, qword [rdi + 10h]
	mov r12, qword [rdi + 18h]
	mov r13, qword [rdi + 20h]
	mov r14, qword [rdi + 28h]
	mov r15, qword [rdi + 30h]
	jmp qword [rdi + 38h]
