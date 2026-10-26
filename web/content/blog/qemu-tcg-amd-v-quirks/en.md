---
title: "When the bug is in the emulator: two QEMU TCG quirks under AMD-V"
description: "On QEMU 10.2's TCG, a guest's CR4 reads never exit, and a data watchpoint in a nested guest hangs the CPU for good. How we found both and how the gates cope."
date: 2026-10-12
tags: [hypervisor, qemu, debugging, testing, x86-64]
---

Most new code in nos's hypervisor first runs on a CPU that does not exist. We develop on a Mac, where x86-64 nos runs under QEMU's TCG, its pure-software CPU emulator. TCG emulates AMD's SVM, nested paging included, but not Intel's VMX at all, which is why the hypervisor got its AMD-V backend first ([The hypervisor](/hypervisor)). With `-cpu max` the emulation is complete enough to boot an unmodified Linux inside nos. It also means every gate (a test each change has to pass) checks two implementations of the same manual at once: ours and QEMU's.

This had bitten us before. Until QEMU 9.2, TCG did not run a guest with paging off through the nested page table at all. That is why the built-in guests start in long mode, and why the gates refuse an older QEMU under TCG: one guest starts a CPU in real mode. In October 2026 two more disagreements turned up within two days of each other, both while we were closing holes that a review of the hypervisor had found. Neither was a bug in nos, but both looked exactly like one. Everything below is QEMU 10.2.0, TCG, `-cpu max`, with nos as the L1 hypervisor, the one running directly on the emulated CPU.

## CR4: a read that never exits

### The hole

Under AMD-V, nos intercepted no control register, so a guest owned its CR4. It could clear CR4.MCE, and according to Linux's source, a kernel built without `X86_MCE` does. A machine check while that guest ran would then not be an exception but a shutdown. The host would see what looked like a triple fault in the guest, restart it, and never hear about the #MC.

The fix, [6576c12](https://github.com/irqlevel/nos/commit/6576c12191e9e65f61da2e4b2e285762ad54b70c), has two halves. The first is in `hvarch`, the crate that keeps the host's CPU in the host's hands. On every entry it forces MCE into the CR4 the guest runs with and intercepts every CR4 write, whatever the policy above asked for ([`svm.rs`](https://github.com/irqlevel/nos/blob/master/src/crates/hvarch/src/x86/svm.rs)):

```rust
const HOST_CR: u32 = vmcb::intercept::cr::WRITE_CR4;
// ...
            v.save.cr4 |= CR4_MCE;
            let c = &mut v.control;
            c.intercept_cr |= HOST_CR;
```

In `hv`, the safe half, the reads are intercepted too, so the guest reads back the CR4 it wrote while the real register keeps MCE ([`cr.rs`](https://github.com/irqlevel/nos/blob/master/src/crates/hv/src/cr.rs)). The proof is a built-in guest, `cr4`, with its own #GP handler. It clears MCE and tries three things that must each raise a #GP: setting PKE (which its CPUID never offers), clearing PAE in long mode and setting bit 32. It also sets PGE through R9 and reads CR4 back after each write. The VMCB, the block of memory that tells an AMD CPU how to run a guest, lists what to intercept. On a CPU that does what the VMCB asks, the host performs all eleven accesses: six reads and five writes.

### What TCG did

Under `hv-test.py` on TCG, no read caused a VM exit (a switch from the guest back to the hypervisor). The guest read the real register, with MCE set no matter what it had written, and the host performed only the writes. Yet the VMCB did ask: `READ_CR4`, bit 4 of the CR intercept word, is set for every guest.

The answer was in QEMU's x86 decoder. In 10.2, `decode_op` in `target/i386/tcg/decode-new.c.inc` checks a `mov` from *any* control register against CR0's read intercept. It adds the register's number to the exit code only when that code is not zero. But SVM numbers these exits from zero up, as nos's VMCB layout shows ([`vmcb.rs`](https://github.com/irqlevel/nos/blob/master/src/crates/hvarch/src/x86/svm/vmcb.rs)):

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

A CR0 read is exit code 0x000, the one code that fails the "not zero" test. So under TCG every CR4 read is checked against CR0's read bit. nos does not set that bit, so the read simply runs. Writes start at 0x010 and are checked properly, and so are the debug registers, at 0x020 and up. A real AMD CPU and KVM's nested AMD-V stop the read, as asked.

### How the test copes

The tempting workaround is to intercept CR0 reads as well. That would catch the CR4 reads on TCG, but on a real CPU the same intercept also catches SMSW, an instruction a guest's user processes may run. Real hardware would pay for the emulator's slip, so we did not do it.

Instead, the test learned where it runs. CPUID leaf 0x4000_0000 carries the signature of the hypervisor underneath, and QEMU's TCG signs itself `TCGTCGTCGTCG` ([`guests.rs`](https://github.com/irqlevel/nos/blob/master/src/crates/hv/src/guests.rs)):

```rust
fn under_qemu_tcg() -> bool {
    hvarch::x86::cpu::hypervisor_signature() == Some(*b"TCGTCGTCGTCG")
}
```

On TCG, the guest's check expects the real register's values, with MCE in them, and five accesses. On any other AMD-V machine it expects the CR4 values it wrote, and eleven accesses. What it never relaxes is the request: either way, the VMCB must ask for the reads, and every write must go through the host. The gate allows exactly those two outcomes:

```python
    "cr4": [r"while the CPU's CR4 kept MCE; PKE, PAE cleared and bit 32 each a #GP, CR4 as it was",
            r"(?:it read back what it wrote, MCE cleared and then PGE set|its reads the CPU's own, QEMU's TCG)",
            r"\b(?:11|5) of CR4 performed, 3 refused"],
```

What TCG cannot exercise, we check on the host. The `platform` target of `hv-fuzz.py` runs `hv` over its own model of the CPU and holds every scripted `mov` of CR4 to what it should have done. A control build that deliberately answered reads from the register instead of the guest's CR4 failed at the fourth input. And on TCG the guest still catches the bug this change exists for: a control run that kept MCE nowhere failed there at the first write.

## DR0–DR3: a store that never finishes

### The hole

Two days later came the debug registers. The four breakpoint address registers belong to the CPU, not to the VMCB: `vmrun` switches DR6 and DR7, but not DR0–DR3. Nothing intercepted a guest's `mov` of them, so a guest could read the breakpoint addresses that the last guest on its CPU had left. [25eed37](https://github.com/irqlevel/nos/commit/25eed37093c3ff142ec34327506de29bbce48ef6) intercepts every read and write of these registers and keeps each guest's four. Before each `vmrun` it loads the guest's values into the CPU, writing only the registers that differ.

### The hang

The `dr` built-in guest runs two VMs, one after the other, on the same CPU. Each VM reads DR0–DR3 (the second must read 0 where the first left its own values), writes a pattern and reads it back. Then it sets a breakpoint that only its own DR0 can fire. The first version used a data watchpoint: DR0 pointed at a dword, DR7 = 0xD0401 (L0, a 4-byte write), and then came a store to that dword.

Under TCG, `hv run` never came back.

A hang with no report is the worst failure this project knows, and this one looked like a wedged hypervisor in every respect. The QEMU monitor (`-monitor unix:...`) showed what the CPU was doing. We ran `info registers` a few times, and every time RIP was on the store, DR0 and DR7 held the guest's values, and DR6 had no B0. The vCPU took no interrupts, not even the host's timer, which normally ends a guest's turn. So the host never got its CPU back, and none of the time limits nos keeps on a guest could fire.

### Whose bug

New intercepts and a new load path had just gone into the world switch, the code that moves the CPU between host and guest, so nos was the obvious suspect. The decisive experiment was a control build with no DR intercept and no DR load at all. The guest wrote DR0 natively, and nos's new code was out of the path. It hung just the same. QEMU's own switching of DR7 at VMRUN and #VMEXIT looks right when you read it: both go through `cpu_x86_update_dr7` in `svm_helper.c`. So this is what we observed. QEMU 10.2's TCG never gets past a store that a data breakpoint watches inside a nested guest. It retries the store for good and takes no interrupt, whoever's DR0–DR3 are in the CPU. A real AMD CPU and KVM's nested SVM are not known to do this.

### An execution breakpoint instead

The property under test does not need a data watchpoint. Now each VM points DR0 at a `ret` of its own. VM A's is at one byte and VM B's at the next, so a breakpoint left at the other's address would not fire. The VM arms the breakpoint for execution and calls the `ret`:

```rust
    0x48, 0x8B, 0x3C, 0x25, 0x08, 0x71, 0x00, 0x00,         // 95: mov rdi, [0x7108] ; its breakpoint: one of the two rets
    0x0F, 0x23, 0xC7,                                       // 9d: mov dr0, rdi
    0xB8, 0x01, 0x04, 0x00, 0x00,                           // a0: mov eax, 0x401 ; DR7: L0, on execution
    0x0F, 0x23, 0xF8,                                       // a5: mov dr7, rax
    0xFF, 0xD7,                                             // a8: call rdi ; a #DB at the ret, before it runs
```

An instruction breakpoint is a fault: the #DB comes before the `ret` runs, and returning to it would raise the #DB again. So the guest's handler records DR6 and the return address, and it turns DR7 off before its `iretq`. Each VM's breakpoint must fire exactly once, with DR6.B0, at its own `ret`. Only the VM's own DR0, loaded before its entry, can make that happen. Control runs showed that the test bites. With the old world switch, the second VM read the first one's breakpoint address and pattern. With the intercept but no load, the breakpoint never fired. On the host, seven mutants of `dr.rs` (copies with a bug put in on purpose) each failed `hv-fuzz.py` within 145 inputs.

## What we keep from it

- **An emulator is one more implementation of the manual.** A gate that runs only on an emulator can invent a hypervisor bug or hide one. So the `cr4` guest says which world it saw ("its reads the CPU's own, QEMU's TCG not stopping a read of CR4 though the VMCB asks") instead of passing quietly either way.
- **Relax what you observe, never what you ask for**, and check the rest elsewhere: in the fuzzer's CPU model, under nested KVM, on silicon. Under VT-x, which nos has used on the EX44 since 2026-10-20 ([86deabd](https://github.com/irqlevel/nos/commit/86deabddfe35caef0024ac90188b857136e18161)), `cr4` and `dr` have expectations of their own.
- **Answer a hang with a control build.** Taking your own code out of the path is the quickest way to tell your bug from the emulator's. And `info registers`, run a few times, shows where a CPU that never returns is stuck.

Both quirks are written down where the next person will trip over them: in the docs under [built-in guests](/docs/hypervisor#the-built-in-guests) and [What entering a guest cannot be told to do](/docs/hypervisor#what-entering-a-guest-cannot-be-told-to-do), in the [`hv-test.py` section](/docs/testing#hv-testpy----the-extension-turned-on-and-off-again-and-guests-under-it) of the testing docs, and in the comments of the two guests that work around them.
