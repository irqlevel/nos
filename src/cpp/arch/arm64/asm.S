/*
 * arm64 context-switch and setjmp primitives (the portable extern "C"
 * contract from hal/cpu.h; x86 twin lives in arch/x86_64/asm.asm).
 *
 * SwitchContext stack frame, low to high (7 x 16 bytes):
 *   [DAIF, pad][x19,x20][x21,x22][x23,x24][x25,x26][x27,x28][x29,x30]
 * Hal::BuildTaskFrame (cpu_arm64.cpp) fabricates this frame for a new
 * task with x30 = TaskEntryThunk, x19 = arg, x20 = entry.
 */

    .text

    /* void SwitchContext(ulong nextSp, ulong* currSp,
                          void (*callback)(void*), void* ctx) */
    .globl SwitchContext
SwitchContext:
    mrs     x9, daif
    stp     x29, x30, [sp, #-16]!
    stp     x27, x28, [sp, #-16]!
    stp     x25, x26, [sp, #-16]!
    stp     x23, x24, [sp, #-16]!
    stp     x21, x22, [sp, #-16]!
    stp     x19, x20, [sp, #-16]!
    stp     x9, xzr, [sp, #-16]!

    mov     x9, sp
    str     x9, [x1]                /* *currSp = old sp */
    mov     sp, x0                  /* switch stacks */

    cbz     x2, 1f
    mov     x0, x3
    blr     x2                      /* callback(ctx) on the new stack */
1:
    ldp     x9, x10, [sp], #16
    ldp     x19, x20, [sp], #16
    ldp     x21, x22, [sp], #16
    ldp     x23, x24, [sp], #16
    ldp     x25, x26, [sp], #16
    ldp     x27, x28, [sp], #16
    ldp     x29, x30, [sp], #16
    msr     daif, x9
    ret

    /* First entry into a new task: SwitchContext pops the fabricated
       frame and returns here with x19 = arg, x20 = entry. */
    .globl Arm64TaskEntryThunk
Arm64TaskEntryThunk:
    mov     x0, x19
    br      x20

    /* void Arm64RunOnStack(ulong stackTop, void (*fn)(void*), void* ctx) */
    .globl Arm64RunOnStack
Arm64RunOnStack:
    /* Task::Stack::StackTop sits 8 below a 16-byte boundary; SP must stay
       16-aligned (AAPCS64, and an SP-alignment fault wherever SCTLR.SA=1) */
    and     x9, x0, #0xFFFFFFFFFFFFFFF0
    mov     sp, x9
    mov     x0, x2
    blr     x1
1:  wfi
    b       1b

    /* long SetJmp(void* ctx) — ctx is Kernel::JmpContext */
    .globl SetJmp
SetJmp:
    stp     x19, x20, [x0]
    stp     x21, x22, [x0, #16]
    stp     x23, x24, [x0, #32]
    stp     x25, x26, [x0, #48]
    stp     x27, x28, [x0, #64]
    stp     x29, x30, [x0, #80]
    mov     x9, sp
    str     x9, [x0, #96]
    mov     x0, #0
    ret

    /* void LongJmp(void* ctx, long result) */
    .globl LongJmp
LongJmp:
    ldp     x19, x20, [x0]
    ldp     x21, x22, [x0, #16]
    ldp     x23, x24, [x0, #32]
    ldp     x25, x26, [x0, #48]
    ldp     x27, x28, [x0, #64]
    ldp     x29, x30, [x0, #80]
    ldr     x9, [x0, #96]
    mov     sp, x9
    mov     x0, x1
    ret
