//! An exit answered where it is taken, and the guest entered again at once
//! (`FastExit`): KVM's fast path for the same writes
//! (`handle_fastpath_set_msr_irqoff`). Nothing here touches the CPU: the
//! trait the run loops of `super::svm` and `super::vmx` call, and hv-fuzz's
//! CPU model, as it is.

/// What a guest CPU's owner answers inside the exit -- on the CPU the guest
/// ran on, with interrupts off since before the entry, the guest's x87, SSE
/// and AVX registers, CR2 and debug registers still the CPU's -- so that the
/// world switch's own loop enters the guest again past the instruction:
/// none of the rest of an exit's work done, the run loop's round left out.
/// On the EX44 that work was 0.5 us of every exit's host time, and a guest's
/// IPIs and timer writes were most of its exits.
///
/// Whatever answers is held, by its caller's borrow, for as long as the
/// entry lasts, and may do no more than it could with interrupts off: no
/// sleep, no allocation, no lock a CPU holds with interrupts on, and no use
/// of a floating-point register -- the guest's are in them. A refusal does
/// nothing at all: the exit goes its way, its instruction undone, and is
/// answered by the run loop as it always was.
pub trait FastExit {
    /// The guest's WRMSR of `value` to `msr`, at `rip`: true when it is
    /// answered here, and the instruction is to be stepped past and the
    /// guest entered again; false, nothing done, for the exit to go its
    /// way. `ran` is the world switch's stamps of the entry it ended, for a
    /// record of the exit.
    fn wrmsr(&mut self, msr: u32, value: u64, rip: u64, ran: crate::x86::svm::Run) -> bool;

    /// The guest's HLT at `rip`, with interrupts on in it, nothing waiting
    /// in its virtual APIC and posted interrupts what can end it (VT-x's;
    /// AMD-V offers none here): true once an interrupt has been posted for
    /// it -- waited for here, with interrupts off, for as long as the
    /// answer chooses -- for the HLT to be stepped past and the guest
    /// entered again at once, the CPU taking what was posted as it enters:
    /// its notification is pending on this CPU, or comes, since the vCPU is
    /// in its guest as posters see it. False, nothing done, for the exit to
    /// go its way, the run loop's halt.
    fn hlt(&mut self, rip: u64, ran: crate::x86::svm::Run) -> bool;
}
