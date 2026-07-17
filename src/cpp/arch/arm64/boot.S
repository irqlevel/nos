/*
 * arm64 boot: Linux Image header, EL2->EL1 drop, BSS zero, bootstrap
 * page tables, MMU enable, jump to the higher-half virtual entry.
 *
 * Runs with the MMU off at the load address (QEMU virt: phys 0x40200000),
 * while the kernel is linked at 0xFFFF800040200000. Until the MMU is on,
 * only PC-relative addressing (adrp/adr) is valid: it yields physical
 * addresses because link-time code<->symbol offsets are preserved.
 *
 * Bootstrap translation (mirrors the x86 4x1GiB linear map):
 *   TTBR1: L0[256] -> L1 with 4x1GiB blocks at KernelSpaceBase + phys
 *          block 0 = Device-nGnRE (PL011/GIC/virtio visible immediately),
 *          blocks 1-3 = Normal WB (RAM; kernel image is in block 1).
 *   TTBR0: L0[0] -> L1[1] identity 1GiB Normal block covering the kernel,
 *          used only for the instant the MMU turns on.
 */

#define SCTLR_EL1_RES1   0x30D00800
#define SCTLR_M          (1 << 0)
#define SCTLR_C          (1 << 2)
#define SCTLR_I          (1 << 12)

/* MAIR indices fixed for the whole kernel:
   idx0 = Normal WB write-allocate, idx1 = Device-nGnRE, idx2 = Normal NC */
#define MAIR_VALUE       0x4404FF

/* TCR_EL1: T0SZ=16 T1SZ=16 (48-bit both halves), 4K granules,
   WBWA inner/outer, inner-shareable walks, IPS=40 bits */
#define TCR_VALUE        0x2B5103510

/* Block descriptor low attributes: AF | SH | AttrIndx | valid block */
#define BLOCK_NORMAL     0x701   /* AF=1 SH=11(inner) AttrIndx=0 valid */
#define BLOCK_DEVICE     0x405   /* AF=1 SH=00 AttrIndx=1 valid */

#define SPSR_EL1H_DAIF   0x3C5   /* EL1h, DAIF masked */

    .section .head.text, "ax"
    .globl Start64

_head:
    b       Start64                 /* code0: jump over the header */
    .long   0                       /* code1 */
    .quad   0x200000                /* text_offset: load offset from RAM base */
    .quad   _image_size             /* image_size (includes BSS) */
    .quad   0x2                     /* flags: LE, 4K page size hint */
    .quad   0                       /* res2 */
    .quad   0                       /* res3 */
    .quad   0                       /* res4 */
    .ascii  "ARM\x64"               /* magic */
    .long   0                       /* res5 */

    .text
Start64:
    /* x0 = DTB physical pointer (QEMU/bootloader contract); keep in x27 */
    mov     x27, x0

    /* Drop from EL2 to EL1h if entered at EL2 (virtualization=on or some
       loaders); QEMU virt and HVF normally enter at EL1 already. */
    mrs     x1, CurrentEL
    lsr     x1, x1, #2
    cmp     x1, #2
    b.ne    el1_entry

    mov     x2, #(1 << 31)          /* HCR_EL2.RW: EL1 is AArch64 */
    msr     hcr_el2, x2
    mrs     x2, cnthctl_el2
    orr     x2, x2, #3              /* EL1PCTEN|EL1PCEN: timers at EL1 */
    msr     cnthctl_el2, x2
    msr     cntvoff_el2, xzr
    ldr     x2, =SCTLR_EL1_RES1
    msr     sctlr_el1, x2
    adr     x2, el1_entry
    msr     elr_el2, x2
    mov     x2, #SPSR_EL1H_DAIF
    msr     spsr_el2, x2
    eret

el1_entry:
    /* Zero BSS (the bootstrap tables live there) */
    adrp    x1, __bss_start
    add     x1, x1, :lo12:__bss_start
    adrp    x2, __bss_end
    add     x2, x2, :lo12:__bss_end
1:  cmp     x1, x2
    b.hs    2f
    str     xzr, [x1], #8
    b       1b
2:

    /* --- Build TTBR1 tables: L0[256] -> L1, L1[0..3] = 1GiB blocks --- */
    adrp    x3, BootPgTblHighL0
    adrp    x4, BootPgTblHighL1

    orr     x5, x4, #3              /* table descriptor: valid + table */
    str     x5, [x3, #(256 * 8)]

    ldr     x6, =BLOCK_DEVICE       /* block 0: phys 0x00000000, device */
    str     x6, [x4]

    mov     x7, #0x40000000         /* block 1: RAM (kernel image) */
    ldr     x8, =BLOCK_NORMAL
    orr     x6, x7, x8
    str     x6, [x4, #8]

    mov     x7, #0x80000000         /* block 2 */
    orr     x6, x7, x8
    str     x6, [x4, #16]

    mov     x7, #0xC0000000         /* block 3 */
    orr     x6, x7, x8
    str     x6, [x4, #24]

    /* --- Build TTBR0 identity tables: L0[0] -> L1, L1[1] = kernel GiB --- */
    adrp    x3, BootPgTblIdL0
    adrp    x4, BootPgTblIdL1

    orr     x5, x4, #3
    str     x5, [x3]

    mov     x7, #0x40000000
    orr     x6, x7, x8              /* Normal block, VA == PA */
    str     x6, [x4, #8]

    /* --- Translation regime configuration --- */
    ldr     x1, =MAIR_VALUE
    msr     mair_el1, x1
    ldr     x1, =TCR_VALUE
    msr     tcr_el1, x1

    adrp    x1, BootPgTblIdL0
    msr     ttbr0_el1, x1
    adrp    x1, BootPgTblHighL0
    msr     ttbr1_el1, x1

    dsb     ish
    isb

    /* MMU + caches on */
    mrs     x1, sctlr_el1
    ldr     x2, =(SCTLR_M | SCTLR_C | SCTLR_I)
    orr     x1, x1, x2
    msr     sctlr_el1, x1
    isb

    /* Jump to the link-time (virtual) address */
    ldr     x1, =high_entry
    br      x1

high_entry:
    /* SPSel already 1 (EL1h); set the boot stack (virtual, via adrp on a
       virtual PC) and enter C++ with the DTB pointer as a linear-map VA */
    msr     daifset, #0xf
    adrp    x1, BootStackTop
    add     x1, x1, :lo12:BootStackTop
    mov     sp, x1

    ldr     x1, =0xFFFF800000000000
    add     x0, x27, x1
    bl      MainArm64

3:  wfi
    b       3b

    /* PSCI CPU_ON secondary entry: x0 = cpu index (context id). MMU off
       at the physical address; the BSP filled Arm64ApTtbr1 with the real
       kernel table root and Arm64ApStackTop[index] with virtual stack
       tops before the CPU_ON call. TTBR0 reuses the boot identity table
       for the switch, exactly like the BSP path. */
    .globl SecondaryEntry
SecondaryEntry:
    mov     x27, x0                 /* cpu index */

    mrs     x1, CurrentEL
    lsr     x1, x1, #2
    cmp     x1, #2
    b.ne    ap_el1
    mov     x2, #(1 << 31)
    msr     hcr_el2, x2
    mrs     x2, cnthctl_el2
    orr     x2, x2, #3
    msr     cnthctl_el2, x2
    msr     cntvoff_el2, xzr
    ldr     x2, =SCTLR_EL1_RES1
    msr     sctlr_el1, x2
    adr     x2, ap_el1
    msr     elr_el2, x2
    mov     x2, #SPSR_EL1H_DAIF
    msr     spsr_el2, x2
    eret
ap_el1:
    ldr     x1, =MAIR_VALUE
    msr     mair_el1, x1
    ldr     x1, =TCR_VALUE
    msr     tcr_el1, x1

    adrp    x1, BootPgTblIdL0
    msr     ttbr0_el1, x1
    adrp    x1, Arm64ApTtbr1
    add     x1, x1, :lo12:Arm64ApTtbr1
    ldr     x1, [x1]
    msr     ttbr1_el1, x1

    dsb     ish
    isb

    mrs     x1, sctlr_el1
    ldr     x2, =(SCTLR_M | SCTLR_C | SCTLR_I)
    orr     x1, x1, x2
    msr     sctlr_el1, x1
    isb

    ldr     x1, =ap_high_entry
    br      x1

ap_high_entry:
    msr     daifset, #0xf
    adrp    x1, Arm64ApStackTop
    add     x1, x1, :lo12:Arm64ApStackTop
    ldr     x1, [x1, x27, lsl #3]
    mov     sp, x1
    mov     x0, x27
    bl      ApMainArm64
4:  wfi
    b       4b

    /* Bootstrap page tables and boot stack (BSS, page-aligned) */
    .section .bss
    .align 12
    .globl BootPgTblHighL0
BootPgTblHighL0:
    .space 4096
    .globl BootPgTblHighL1
BootPgTblHighL1:
    .space 4096
BootPgTblIdL0:
    .space 4096
BootPgTblIdL1:
    .space 4096
BootStack:
    .space 16384
    .globl BootStackTop
BootStackTop:
    .globl Arm64ApTtbr1
Arm64ApTtbr1:
    .space 8
    .globl Arm64ApStackTop
Arm64ApStackTop:
    .space (8 * 8)
