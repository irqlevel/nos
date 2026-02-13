BITS 64

section .text

global asm_memset
global asm_memcpy
global asm_memcmp
global asm_strlen
global asm_strcmp

; void asm_memset(void* ptr, u8 c, size_t size)
; rdi = ptr, sil = fill byte, rdx = size
asm_memset:
	mov rcx, rdx
	; Broadcast fill byte across all 8 bytes of rax
	movzx eax, sil
	mov rdx, 0x0101010101010101
	imul rax, rdx
	; Qword fills
	mov rdx, rcx
	shr rcx, 3
	cld
	rep stosq
	; Remaining bytes
	mov rcx, rdx
	and rcx, 7
	rep stosb
	ret

; void asm_memcpy(void* dst, const void* src, size_t size)
; rdi = dst, rsi = src, rdx = size
asm_memcpy:
	mov rcx, rdx
	; Qword copies
	mov rdx, rcx
	shr rcx, 3
	cld
	rep movsq
	; Remaining bytes
	mov rcx, rdx
	and rcx, 7
	rep movsb
	ret

; int asm_memcmp(const void* p1, const void* p2, size_t size)
; rdi = p1, rsi = p2, rdx = size
asm_memcmp:
	mov rcx, rdx
	test rcx, rcx
	jz .memcmp_equal
	cld
	repe cmpsb
	je .memcmp_equal
	movzx eax, byte [rdi-1]
	movzx ecx, byte [rsi-1]
	sub eax, ecx
	jg .memcmp_greater
	mov eax, -1
	ret
.memcmp_greater:
	mov eax, 1
	ret
.memcmp_equal:
	xor eax, eax
	ret

; size_t asm_strlen(const char* s)
; rdi = s
asm_strlen:
	mov rcx, -1
	xor al, al
	cld
	repne scasb
	not rcx
	dec rcx
	mov rax, rcx
	ret

; int asm_strcmp(const char* s1, const char* s2)
; rdi = s1, rsi = s2
asm_strcmp:
.strcmp_loop:
	movzx eax, byte [rdi]
	movzx ecx, byte [rsi]
	cmp al, cl
	jne .strcmp_diff
	test al, al
	jz .strcmp_equal
	inc rdi
	inc rsi
	jmp .strcmp_loop
.strcmp_diff:
	sub eax, ecx
	jg .strcmp_greater
	mov eax, -1
	ret
.strcmp_greater:
	mov eax, 1
	ret
.strcmp_equal:
	xor eax, eax
	ret
