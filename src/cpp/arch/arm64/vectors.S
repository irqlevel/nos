/*
 * EL1 exception vector table. Each 128-byte slot only branches to a
 * shared body (SAVE_CONTEXT alone is bigger than a slot); the bodies
 * build a full Kernel::Context (arch/arm64/context.h: x0-x30, sp, elr,
 * spsr, esr, far = 36 slots) on the interrupted stack and call the C++
 * handler with it in x0.
 *
 * Only the "current EL, SPx" synchronous and IRQ vectors carry real
 * handlers; everything else lands in the unexpected-vector panic path
 * with the vector index as the second argument.
 */

.macro SAVE_CONTEXT
    sub     sp, sp, #(36 * 8)
    stp     x0, x1, [sp, #(0 * 8)]
    stp     x2, x3, [sp, #(2 * 8)]
    stp     x4, x5, [sp, #(4 * 8)]
    stp     x6, x7, [sp, #(6 * 8)]
    stp     x8, x9, [sp, #(8 * 8)]
    stp     x10, x11, [sp, #(10 * 8)]
    stp     x12, x13, [sp, #(12 * 8)]
    stp     x14, x15, [sp, #(14 * 8)]
    stp     x16, x17, [sp, #(16 * 8)]
    stp     x18, x19, [sp, #(18 * 8)]
    stp     x20, x21, [sp, #(20 * 8)]
    stp     x22, x23, [sp, #(22 * 8)]
    stp     x24, x25, [sp, #(24 * 8)]
    stp     x26, x27, [sp, #(26 * 8)]
    stp     x28, x29, [sp, #(28 * 8)]
    add     x1, sp, #(36 * 8)          /* SP at the exception point */
    stp     x30, x1, [sp, #(30 * 8)]
    mrs     x1, elr_el1
    mrs     x2, spsr_el1
    stp     x1, x2, [sp, #(32 * 8)]
    mrs     x1, esr_el1
    mrs     x2, far_el1
    stp     x1, x2, [sp, #(34 * 8)]
.endm

.macro RESTORE_CONTEXT_ERET
    ldp     x1, x2, [sp, #(32 * 8)]
    msr     elr_el1, x1
    msr     spsr_el1, x2
    ldr     x30, [sp, #(30 * 8)]
    ldp     x28, x29, [sp, #(28 * 8)]
    ldp     x26, x27, [sp, #(26 * 8)]
    ldp     x24, x25, [sp, #(24 * 8)]
    ldp     x22, x23, [sp, #(22 * 8)]
    ldp     x20, x21, [sp, #(20 * 8)]
    ldp     x18, x19, [sp, #(18 * 8)]
    ldp     x16, x17, [sp, #(16 * 8)]
    ldp     x14, x15, [sp, #(14 * 8)]
    ldp     x12, x13, [sp, #(12 * 8)]
    ldp     x10, x11, [sp, #(10 * 8)]
    ldp     x8, x9, [sp, #(8 * 8)]
    ldp     x6, x7, [sp, #(6 * 8)]
    ldp     x4, x5, [sp, #(4 * 8)]
    ldp     x2, x3, [sp, #(2 * 8)]
    ldp     x0, x1, [sp, #(0 * 8)]
    add     sp, sp, #(36 * 8)
    eret
.endm

    .text

vec_sync:
    SAVE_CONTEXT
    mov     x0, sp
    bl      ArmSyncEntry
    RESTORE_CONTEXT_ERET

vec_irq:
    SAVE_CONTEXT
    mov     x0, sp
    bl      ArmIrqEntry
    RESTORE_CONTEXT_ERET

.macro UNEXPECTED_BODY name, index
vec_unexpected_\name:
    SAVE_CONTEXT
    mov     x0, sp
    mov     x1, #\index
    bl      ArmUnexpectedEntry
    RESTORE_CONTEXT_ERET
.endm

UNEXPECTED_BODY sp0_sync, 0
UNEXPECTED_BODY sp0_irq, 1
UNEXPECTED_BODY sp0_fiq, 2
UNEXPECTED_BODY sp0_serror, 3
UNEXPECTED_BODY fiq, 6
UNEXPECTED_BODY serror, 7
UNEXPECTED_BODY low64_sync, 8
UNEXPECTED_BODY low64_irq, 9
UNEXPECTED_BODY low64_fiq, 10
UNEXPECTED_BODY low64_serror, 11
UNEXPECTED_BODY low32_sync, 12
UNEXPECTED_BODY low32_irq, 13
UNEXPECTED_BODY low32_fiq, 14
UNEXPECTED_BODY low32_serror, 15

    .globl Arm64Vectors
    .align 11
Arm64Vectors:
    /* Current EL with SP0 */
    b       vec_unexpected_sp0_sync
    .align 7
    b       vec_unexpected_sp0_irq
    .align 7
    b       vec_unexpected_sp0_fiq
    .align 7
    b       vec_unexpected_sp0_serror
    .align 7

    /* Current EL with SPx */
    b       vec_sync
    .align 7
    b       vec_irq
    .align 7
    b       vec_unexpected_fiq
    .align 7
    b       vec_unexpected_serror
    .align 7

    /* Lower EL, AArch64 */
    b       vec_unexpected_low64_sync
    .align 7
    b       vec_unexpected_low64_irq
    .align 7
    b       vec_unexpected_low64_fiq
    .align 7
    b       vec_unexpected_low64_serror
    .align 7

    /* Lower EL, AArch32 */
    b       vec_unexpected_low32_sync
    .align 7
    b       vec_unexpected_low32_irq
    .align 7
    b       vec_unexpected_low32_fiq
    .align 7
    b       vec_unexpected_low32_serror
