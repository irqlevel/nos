//! Exceptions: a fault in the loader is said -- which, where, its error
//! code and CR2 -- and the guest turned off, rather than left to be a
//! triple fault the VM's report shows only as a shutdown. Interrupts stay
//! off throughout: the IDT has the 32 exception vectors and nothing else.

use core::arch::{asm, global_asm};
use core::cell::UnsafeCell;

/* One stub a vector: those the CPU pushes no error code for push a 0, so
 * the frame is the same for all -- vector, error code, then the CPU's RIP,
 * CS, RFLAGS, RSP and SS -- and all go to one handler. */
global_asm!(
    r#"
    .section .text.isr, "ax"
    .code64
    .macro ISR_NOERR vec
isr_\vec:
    push 0
    push \vec
    jmp isr_common
    .endm
    .macro ISR_ERR vec
isr_\vec:
    push \vec
    jmp isr_common
    .endm

    ISR_NOERR 0
    ISR_NOERR 1
    ISR_NOERR 2
    ISR_NOERR 3
    ISR_NOERR 4
    ISR_NOERR 5
    ISR_NOERR 6
    ISR_NOERR 7
    ISR_ERR 8
    ISR_NOERR 9
    ISR_ERR 10
    ISR_ERR 11
    ISR_ERR 12
    ISR_ERR 13
    ISR_ERR 14
    ISR_NOERR 15
    ISR_NOERR 16
    ISR_ERR 17
    ISR_NOERR 18
    ISR_NOERR 19
    ISR_NOERR 20
    ISR_ERR 21
    ISR_NOERR 22
    ISR_NOERR 23
    ISR_NOERR 24
    ISR_NOERR 25
    ISR_NOERR 26
    ISR_NOERR 27
    ISR_NOERR 28
    ISR_ERR 29
    ISR_ERR 30
    ISR_NOERR 31

isr_common:
    mov rdi, [rsp]
    mov rsi, [rsp + 8]
    mov rdx, [rsp + 16]
    mov rcx, cr2
    and rsp, -16
    call hvboot_exception
    ud2

    .section .rodata.isr_table, "a"
    .balign 8
    .globl hvboot_isr_table
hvboot_isr_table:
    .quad isr_0, isr_1, isr_2, isr_3, isr_4, isr_5, isr_6, isr_7
    .quad isr_8, isr_9, isr_10, isr_11, isr_12, isr_13, isr_14, isr_15
    .quad isr_16, isr_17, isr_18, isr_19, isr_20, isr_21, isr_22, isr_23
    .quad isr_24, isr_25, isr_26, isr_27, isr_28, isr_29, isr_30, isr_31
"#
);

const VECTORS: usize = 32;
const GATE_BYTES: usize = 16;
/// The code segment the loader runs on (`pvh_entry`'s GDT).
const CODE_SELECTOR: u16 = 0x10;
/// A present 64-bit interrupt gate at DPL 0.
const INTERRUPT_GATE: u8 = 0x8E;

#[repr(C, align(16))]
struct Idt(UnsafeCell<[u8; VECTORS * GATE_BYTES]>);

// SAFETY: written once, by `install`, before anything can fault into it --
// one CPU, interrupts off.
unsafe impl Sync for Idt {}

static IDT: Idt = Idt(UnsafeCell::new([0; VECTORS * GATE_BYTES]));

extern "C" {
    static hvboot_isr_table: [u64; VECTORS];
}

/// Fill the IDT with the stubs and load it.
pub fn install() {
    // SAFETY: the table is the 32 stubs' addresses the assembly above lays
    // out, read-only data.
    let stubs = unsafe { &*core::ptr::addr_of!(hvboot_isr_table) };
    // SAFETY: the only reference to the IDT's bytes while it is filled: no
    // exception can come before the `lidt` below makes it the CPU's.
    let idt = unsafe { &mut *IDT.0.get() };
    for (v, &stub) in stubs.iter().enumerate() {
        let g = &mut idt[v * GATE_BYTES..(v + 1) * GATE_BYTES];
        g[0..2].copy_from_slice(&(stub as u16).to_le_bytes());
        g[2..4].copy_from_slice(&CODE_SELECTOR.to_le_bytes());
        g[4] = 0;
        g[5] = INTERRUPT_GATE;
        g[6..8].copy_from_slice(&((stub >> 16) as u16).to_le_bytes());
        g[8..12].copy_from_slice(&((stub >> 32) as u32).to_le_bytes());
        g[12..16].fill(0);
    }
    #[repr(C, packed)]
    struct Pointer {
        limit: u16,
        base: u64,
    }
    let ptr = Pointer { limit: (VECTORS * GATE_BYTES - 1) as u16, base: IDT.0.get() as u64 };
    // SAFETY: the IDT is a static, filled above with gates to the stubs.
    unsafe { asm!("lidt [{}]", in(reg) core::ptr::addr_of!(ptr), options(readonly, nostack, preserves_flags)) };
}

fn name(vector: u64) -> &'static str {
    match vector {
        0 => "divide error",
        1 => "debug",
        2 => "NMI",
        3 => "breakpoint",
        4 => "overflow",
        5 => "bound range",
        6 => "invalid opcode",
        7 => "device not available",
        8 => "double fault",
        10 => "invalid TSS",
        11 => "segment not present",
        12 => "stack fault",
        13 => "general protection",
        14 => "page fault",
        16 => "x87 floating point",
        17 => "alignment check",
        18 => "machine check",
        19 => "SIMD floating point",
        21 => "control protection",
        _ => "reserved",
    }
}

#[no_mangle]
extern "C" fn hvboot_exception(vector: u64, error: u64, rip: u64, cr2: u64) -> ! {
    say!("exception {} ({}) at {:#x}, error code {:#x}, CR2 {:#x}", vector, name(vector), rip, error, cr2);
    crate::io::power_off()
}
