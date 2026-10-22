---
title: "When the emulator is the bug: two QEMU TCG quirks under AMD-V"
description: "On QEMU 10.2's TCG a guest's CR4 reads never exit, and a data watchpoint in a nested guest hangs the CPU for good. How we found both, and how the gates cope."
date: 2026-10-12
tags: [hypervisor, qemu, debugging, testing, x86-64]
---

Most of the work on nos's hypervisor gets its first run on a CPU that does not exist. The development machine is a Mac, x86-64 nos runs there under QEMU's TCG, and TCG emulates AMD's SVM, nested paging included, and no Intel VMX at all — which is why the hypervisor got its AMD-V backend first ([The hypervisor](/hypervisor)). Under `-cpu max` that emulation is complete enough to boot unmodified Linux inside nos. It also means every gate tests two implementations of the same manual at once: ours, and QEMU's.

It had bitten before: until QEMU 9.2, TCG did not put a guest with paging off through the nested page table at all, which is why the built-in guests start in long mode, and why the gates refuse an older QEMU under TCG for the sake of the one guest that starts a CPU in real mode. In October 2026 two more disagreements turned up within two days of each other, both while we were closing holes a review of the hypervisor had found. Neither was nos's bug; both looked exactly like one. Everything below is QEMU 10.2.0, TCG, `-cpu max`, with nos as the L1 hypervisor.

## CR4: a read that never exits

### The hole

Under AMD-V nos intercepted no control register, so a guest owned its CR4. It could clear CR4.MCE — by Linux's source, a kernel built without `X86_MCE` does — and a machine check while it ran would then be no exception but a shutdown: the host would see what looked like the guest's triple fault, restart it, and never hear of the #MC.

The fix, [6576c12](https://github.com/irqlevel/nos/commit/6576c12191e9e65f61da2e4b2e285762ad54b70c), has two halves. In `hvarch`, the crate that keeps the host's CPU the host's, every entry forces MCE into the CR4 the guest runs with and intercepts every CR4 write, whatever the policy above asked ([`svm.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hvarch/src/x86/svm.rs)):

```rust
const HOST_CR: u32 = vmcb::intercept::cr::WRITE_CR4;
// ...
            v.save.cr4 |= CR4_MCE;
            let c = &mut v.control;
            c.intercept_cr |= HOST_CR;
```

In `hv`, the safe half, the reads are intercepted too, so the guest reads back the CR4 it wrote while the register keeps MCE ([`cr.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/cr.rs)). The proof is a built-in guest, `cr4`, with a #GP handler of its own: it clears MCE, tries PKE (which its CPUID never offers), PAE cleared in long mode and bit 32, sets PGE through R9 and reads CR4 back after each write. On a CPU that does what the VMCB asks, the host performs all eleven accesses — six reads, five writes.

### What TCG did

Under `hv-test.py` on TCG the reads were never stopped. The guest read the register itself — MCE set, whatever it had written — and the host performed only the writes. Yet the VMCB did ask: `READ_CR4`, bit 4 of the CR intercept word, is set for every guest.

The answer was in QEMU's x86 decoder. In 10.2, `decode_op` in `target/i386/tcg/decode-new.c.inc` checks a `mov` from *any* control register against CR0's read intercept, and adds the register's number to the exit code only when that code is not zero. SVM numbers these exits from zero up, as nos's VMCB layout has them ([`vmcb.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hvarch/src/x86/svm/vmcb.rs)):

```rust
    /// A `mov` from CR4, and one to it: a control register's reads at
    /// 0x000 and on, its writes at 0x010 and on.
    pub const CR4_READ: u64 = 0x004;
    pub const CR4_WRITE: u64 = 0x014;
    /// A `mov` from a debug register, at 0x020 and on by its number, and
    /// one to it, at 0x030 and on.
    pub const DR_READ: u64 = 0x020;
    pub const DR_WRITE: u64 = 0x030;
```

CR0's read is exit code 0x000 — the one code that fails "not zero". So under TCG every CR4 read is checked against CR0's read bit, which nos does not set, and simply runs. Writes start at 0x010 and are checked properly; so are the debug registers, at 0x020 and up. A real AMD CPU and KVM's nested AMD-V stop the read as asked.

### How the test copes

The tempting workaround — intercepting CR0's reads as well — would catch the CR4 reads on TCG, but on a real CPU that intercept also catches SMSW, which a guest's user processes may run. Real hardware would pay for an emulator's slip, so it was not done.

Instead the test learned where it runs. CPUID leaf 0x4000_0000 carries the signature of the hypervisor underneath, and QEMU's TCG signs itself `TCGTCGTCGTCG` ([`guests.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/guests.rs)):

```rust
fn under_qemu_tcg() -> bool {
    hvarch::x86::cpu::hypervisor_signature() == Some(*b"TCGTCGTCGTCG")
}
```

On TCG the guest's check expects the register's values, MCE in them, and five accesses; on any other AMD-V machine, the CR4s it wrote and eleven. What it never relaxes is the request: either way the VMCB must ask for the reads, and every write must be the host's. The gate allows exactly those two outcomes:

```python
    "cr4": [r"while the CPU's CR4 kept MCE; PKE, PAE cleared and bit 32 each a #GP, CR4 as it was",
            r"(?:it read back what it wrote, MCE cleared and then PGE set|its reads the CPU's own, QEMU's TCG)",
            r"\b(?:11|5) of CR4 performed, 3 refused"],
```

What TCG cannot exercise is checked on the host: `hv-fuzz.py`'s `platform` target runs `hv` over a CPU model of its own and holds every scripted `mov` of CR4 to what it should have done; a control build that answered reads from the register instead of the guest's CR4 failed at the fourth input. And on TCG the guest still catches the bug the change exists for: a control run with MCE kept nowhere failed there at the first write.

## DR0–DR3: a store that never finishes

### The hole

Two days later, the debug registers. The four breakpoint addresses are the CPU's, not the VMCB's — `vmrun` switches DR6 and DR7 but not DR0–DR3 — and nothing intercepted a guest's `mov` of them, so a guest could read the breakpoint addresses the last guest on its CPU had left. [25eed37](https://github.com/irqlevel/nos/commit/25eed37093c3ff142ec34327506de29bbce48ef6) intercepts every read and write of them, keeps each guest's four, and makes the CPU's the guest's before each `vmrun`, writing only the registers that differ.

### The hang

The `dr` built-in guest runs two VMs one after the other on the same CPU. Each reads DR0–DR3 — the second must read 0 where the first left its own — writes a pattern and reads it back, then sets a breakpoint only its own DR0 can fire. The first version used a data watchpoint: DR0 at a dword, DR7 = 0xD0401 — L0, a 4-byte write — then a store to it.

Under TCG, `hv run` never came back.

A hang with no report is the worst failure this project knows, and this one looked like a hypervisor wedge in every respect. The QEMU monitor (`-monitor unix:...`) showed what the CPU was doing: `info registers`, taken a few times, had RIP on the store every time, DR0 and DR7 the guest's, and DR6 without B0. The vCPU took no interrupt — not even the host's timer, which normally ends a guest's turn — so the host never got its CPU back, and no time limit nos keeps on a guest could fire.

### Whose bug

New intercepts and a new load path had just gone into the world switch, so the obvious suspect was nos. The decisive experiment was a control build with no DR intercept and no DR load at all — the guest writing DR0 natively, nos's new code out of the path. It hung the same. QEMU's own switching of DR7 at VMRUN and #VMEXIT looks right on reading: both go through `cpu_x86_update_dr7` in `svm_helper.c`. So, as observed: QEMU 10.2's TCG never gets past a store that a data breakpoint watches inside a nested guest, retrying it for good with no interrupt taken, whoever's DR0–DR3 are in the CPU. A real AMD CPU and KVM's nested SVM are not known to do this.

### An execution breakpoint instead

The property under test does not need a data watchpoint. Each VM now points DR0 at a `ret` of its own — VM A's at one byte, VM B's at the next, so a breakpoint left at the other's address would not fire — arms it for execution, and calls it:

```rust
    0x48, 0x8B, 0x3C, 0x25, 0x08, 0x71, 0x00, 0x00,         // 95: mov rdi, [0x7108] ; its breakpoint: one of the two rets
    0x0F, 0x23, 0xC7,                                       // 9d: mov dr0, rdi
    0xB8, 0x01, 0x04, 0x00, 0x00,                           // a0: mov eax, 0x401 ; DR7: L0, on execution
    0x0F, 0x23, 0xF8,                                       // a5: mov dr7, rax
    0xFF, 0xD7,                                             // a8: call rdi ; a #DB at the ret, before it runs
```

An instruction breakpoint is a fault — the #DB comes before the `ret` runs, and returning to it would raise it again — so the guest's handler records DR6 and the return address and turns DR7 off before its `iretq`. Each VM's breakpoint must fire exactly once, with DR6.B0, at its own `ret`, which only its own DR0 loaded before its entry can do. Control runs showed the test bites: with the old world switch the second VM read the first's breakpoint address and pattern, and with the intercept but no load the breakpoint never fired. On the host, seven mutants of `dr.rs` each failed `hv-fuzz.py` within 145 inputs.

## What we keep from it

- **An emulator is one more implementation of the manual.** A gate that runs only there can invent a hypervisor bug or hide one, so the `cr4` guest says which world it saw — "its reads the CPU's own, QEMU's TCG not stopping a read of CR4 though the VMCB asks" — instead of passing quietly either way.
- **Relax what you observe, never what you ask for**, and check the rest elsewhere: the fuzzer's CPU model, nested KVM, silicon. Under VT-x, on the EX44 since 2026-10-20 ([86deabd](https://github.com/irqlevel/nos/commit/86deabddfe35caef0024ac90188b857136e18161)), `cr4` and `dr` run with expectations of their own.
- **A hang gets a control build.** Taking your own code out of the path is the quickest way to tell your bug from the emulator's, and `info registers`, taken a few times, says where a CPU that never returns is stuck.

Both quirks are written down where the next person will trip over them: in the docs' [built-in guests](/docs/hypervisor#the-built-in-guests) and [What entering a guest cannot be told to do](/docs/hypervisor#what-entering-a-guest-cannot-be-told-to-do), in the [`hv-test.py` section](/docs/testing#hv-testpy----the-extension-turned-on-and-off-again-and-guests-under-it) of the testing docs, and in the comments of the two guests that work around them.
