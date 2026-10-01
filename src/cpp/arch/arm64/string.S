/*
 * arm64 asm_memset, asm_memcpy and asm_memmove: the routines lib/stdlib.cpp
 * builds MemSet, MemCpy, MemMove -- and memset, memcpy, memmove -- on, so
 * every copy the kernel's Rust makes as well, which links those C names
 * (src/rust/.cargo/config.toml builds core without compiler-builtins' own).
 * The x86 twins are rep stos/movs in arch/x86_64/stdlib_asm.asm.
 *
 * They were byte loops in C++ built at -O0, about 2 GB/s on an M-series
 * core. These move 64 bytes an iteration through four ldp/stp pairs, in the
 * general registers only: the kernel saves no FP/SIMD state across a
 * context switch (-mgeneral-regs-only), so no vector register may be used.
 *
 * Stores are aligned: the destination is brought to a 16-byte boundary a
 * byte at a time first, so even a block of Device memory written whole is
 * written in aligned pairs. Loads need not be: SCTLR_EL1.A is clear
 * (boot.S), and an unaligned load of Normal memory is a plain load.
 *
 * Only x0-x10 are used, all caller-saved (AAPCS64); none of the three
 * returns a value.
 */

    .text

/* void asm_memset(void* dst, unsigned char c, size_t n) */
    .globl asm_memset
    .type asm_memset, %function
asm_memset:
    /* The byte in all eight of x1's */
    and     x1, x1, #0xff
    orr     x1, x1, x1, lsl #8
    orr     x1, x1, x1, lsl #16
    orr     x1, x1, x1, lsl #32
    cmp     x2, #16
    b.lo    3f
0:  tst     x0, #15                 /* to a 16-byte boundary: at most 15 */
    b.eq    1f
    strb    w1, [x0], #1
    sub     x2, x2, #1
    b       0b
1:  cmp     x2, #64
    b.lo    2f
    stp     x1, x1, [x0]
    stp     x1, x1, [x0, #16]
    stp     x1, x1, [x0, #32]
    stp     x1, x1, [x0, #48]
    add     x0, x0, #64
    sub     x2, x2, #64
    b       1b
2:  cmp     x2, #16
    b.lo    3f
    stp     x1, x1, [x0], #16
    sub     x2, x2, #16
    b       2b
3:  cbz     x2, 4f
    strb    w1, [x0], #1
    sub     x2, x2, #1
    b       3b
4:  ret
    .size asm_memset, . - asm_memset

/* void asm_memcpy(void* dst, const void* src, size_t n)

   Also asm_memmove's forward copy, which it is safe for whenever dst is
   below src: each 64 (or 16) bytes are all loaded before any of them is
   stored, and a store below src cannot reach a byte not yet loaded. */
    .globl asm_memcpy
    .type asm_memcpy, %function
asm_memcpy:
    cmp     x2, #16
    b.lo    3f
0:  tst     x0, #15                 /* dst to a 16-byte boundary */
    b.eq    1f
    ldrb    w3, [x1], #1
    strb    w3, [x0], #1
    sub     x2, x2, #1
    b       0b
1:  cmp     x2, #64
    b.lo    2f
    ldp     x3, x4, [x1]
    ldp     x5, x6, [x1, #16]
    ldp     x7, x8, [x1, #32]
    ldp     x9, x10, [x1, #48]
    stp     x3, x4, [x0]
    stp     x5, x6, [x0, #16]
    stp     x7, x8, [x0, #32]
    stp     x9, x10, [x0, #48]
    add     x1, x1, #64
    add     x0, x0, #64
    sub     x2, x2, #64
    b       1b
2:  cmp     x2, #16
    b.lo    3f
    ldp     x3, x4, [x1], #16
    stp     x3, x4, [x0], #16
    sub     x2, x2, #16
    b       2b
3:  cbz     x2, 4f
    ldrb    w3, [x1], #1
    strb    w3, [x0], #1
    sub     x2, x2, #1
    b       3b
4:  ret
    .size asm_memcpy, . - asm_memcpy

/* void asm_memmove(void* dst, const void* src, size_t n)

   Forward, as asm_memcpy, unless dst lands inside (src, src + n) -- the one
   overlap a forward copy would read after writing. dst - src taken unsigned
   says it in one comparison: below n only for such a dst (or dst == src,
   which has nothing to move). Backward is asm_memcpy mirrored: from the
   ends down, the destination's end brought to a 16-byte boundary first, and
   each block loaded whole before any of it is stored -- a store above src
   cannot reach a byte not yet loaded. */
    .globl asm_memmove
    .type asm_memmove, %function
asm_memmove:
    sub     x3, x0, x1
    cmp     x3, x2
    b.hs    asm_memcpy
    cbz     x3, 4f
    add     x0, x0, x2
    add     x1, x1, x2
    cmp     x2, #16
    b.lo    3f
0:  tst     x0, #15
    b.eq    1f
    ldrb    w3, [x1, #-1]!
    strb    w3, [x0, #-1]!
    sub     x2, x2, #1
    b       0b
1:  cmp     x2, #64
    b.lo    2f
    ldp     x3, x4, [x1, #-16]
    ldp     x5, x6, [x1, #-32]
    ldp     x7, x8, [x1, #-48]
    ldp     x9, x10, [x1, #-64]
    stp     x3, x4, [x0, #-16]
    stp     x5, x6, [x0, #-32]
    stp     x7, x8, [x0, #-48]
    stp     x9, x10, [x0, #-64]
    sub     x1, x1, #64
    sub     x0, x0, #64
    sub     x2, x2, #64
    b       1b
2:  cmp     x2, #16
    b.lo    3f
    ldp     x3, x4, [x1, #-16]!
    stp     x3, x4, [x0, #-16]!
    sub     x2, x2, #16
    b       2b
3:  cbz     x2, 4f
    ldrb    w3, [x1, #-1]!
    strb    w3, [x0, #-1]!
    sub     x2, x2, #1
    b       3b
4:  ret
    .size asm_memmove, . - asm_memmove
