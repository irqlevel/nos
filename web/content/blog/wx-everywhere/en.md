---
title: "W^X everywhere: the kernel image, the heap and every module"
description: "No page in nos can be both written and executed. How we split the kernel image, found a hole under every runtime mapping, and keep it shut with a test."
date: 2026-09-17
tags: [memory, safety, x86-64, arm64, modules, testing]
---

W^X ("write xor execute") is a promise about every page: it can be written or it can be executed, but never both. A page that is both turns a stray write into code that runs. A writable `.text` lets a wild pointer quietly patch the kernel. nos wants the opposite, because a failure must be loud. A write into code, or a jump into data, should fault on the spot and say where it happened.

Keeping that promise took two months of work on both architectures. Part of that work was finding out that the documentation had made the promise before the code kept it.

## Splitting the image three ways

We started with the kernel image on July 17: arm64 in [329a8e7](https://github.com/irqlevel/nos/commit/329a8e7), and x86-64 the same afternoon in [47ca40e](https://github.com/irqlevel/nos/commit/47ca40e). Both linker scripts got page-aligned symbols between the sections: `KernelText` after `.text` and `KernelRodata` after `.rodata`. Late in boot, once the real page table is live, `ProtectRange` walks the image's leaf entries and rewrites their permissions. A leaf is a last-level page-table entry, and here each one maps a 4 KiB page ([main.cpp](https://github.com/irqlevel/nos/blob/master/src/base/kernel/main.cpp)):

```cpp
pt.ProtectRange(t0, t1 - t0, false, true);   /* RX */
pt.ProtectRange(t1, r1 - t1, false, false);  /* RO+NX */
pt.ProtectRange(r1, e - r1, true, false);    /* RW+NX */
```

On arm64, read-only is the AP[2] bit, and never-execute is PXN and UXN (execute-never for the kernel and for user code). The CPU honours them from the start. x86-64 needs two switches, and `Hal::EnableWxSupport()` flips both. Without `EFER.NXE` the NX bit means nothing, and without `CR0.WP` ring 0 ignores the read-only bit. W^X without WP would have left the kernel free to write its own `.text`.

A boot parameter checked the result: with `wxprobe=on`, the kernel writes to `.text` after the split. x86-64 died with a page fault, error code 0x3 (present, write). arm64 died with a permission data abort, fault status code DFSC 0x0F. Neither reached the line that would have said the write succeeded.

The same day, a review of the arm64 port ([7408805](https://github.com/irqlevel/nos/commit/7408805)) closed a way around all of this. For the moment the MMU turns on, `boot.S` installs a 1 GiB identity map (virtual address equals physical address) in TTBR0, the register that points to the page table for the lower half of the address space. That map stayed live forever, and it was RWX. So at its physical address the kernel text was writable and executable, whatever the kernel's own mapping in the high half of the address space said. Now the `TCR.EPD0` bit switches off TTBR0 walks as soon as execution reaches the high half.

On August 29 ([6ebed66](https://github.com/irqlevel/nos/commit/6ebed66)) every MMIO leaf on x86-64 became NX as well (on arm64 they already were). No code is ever fetched from a device, and on real hardware a speculative fetch from device memory can have read side effects ([Paging](/docs/paging#mmio-cacheability-and-wx)).

## A promise the code did not keep

By September, CLAUDE.md (the project's rulebook) said that executing from a heap buffer faults, and so did `docs/paging.md`. It did not. `MapRangeLocked` builds the leaf entries behind the heap, task stacks, DMA buffers and module images. It set W and P on them (writable, present) but never NX. Neither did the code that sets up the `TmpMap` window and `PageArray`. So everything the kernel mapped after boot was writable and executable at once, on both architectures. Only the image, MMIO and a module's finished segments carried NX.

Nobody had noticed, because nothing breaks. A missing NX bit is invisible until something executes data, and no smoke boot does that on purpose. It is exactly the kind of failure this project fears most: one that looks like success.

The fix ([b54a189](https://github.com/irqlevel/nos/commit/b54a189), September 17) puts NX on every leaf the kernel writes at run time ([page_table.cpp](https://github.com/irqlevel/nos/blob/master/src/base/mm/page_table.cpp)):

```cpp
page->Get();
l1Entry->SetAddress(page->GetPhyAddress());
l1Entry->SetWritable();
/* RW+NX is what every runtime mapping gets: the heap, stacks,
   DMA buffers and a module image before it is laid out. The one
   thing that runs from such a page is a module's text, and the
   loader asks for it by name (SetRangeProtection) once the
   segment is written -- so nothing is ever writable and
   executable at the same time. */
l1Entry->SetNoExecute();
l1Entry->SetPresent();
```

`SetupPage`, which builds the table at boot, now takes an `executable` argument. Exactly one caller passes `true`: the kernel image. The kernel runs out of it from the moment it switches to its own page table until `ProtectRange` splits it. The tables above the leaves stay permissive on purpose. On both architectures, an NX bit on an L4, L3 or L2 entry takes execute permission away from the whole subtree, and no leaf can give it back. A module's text lives under those tables too. So the leaf alone decides whether a page can run.

## Bit 63 is reserved until it isn't

On x86-64 the fix had a trap in it. Until `EFER.NXE` is set, bit 63 of a page-table entry does not mean "no execute". It is a reserved bit, and an entry that has it set faults on the first access. `EnableWxSupport()` used to run just before `ProtectRange`, which was fine while nothing before that point set NX. Now the `TmpMap` window and the early drain of the free-page list set it, and they run much earlier. So the call moved to the top of `Main2`, ahead of any page table:

```cpp
/* Before any page table is built: every mapping the kernel makes at
   runtime carries NX, and with EFER.NXE off bit 63 is a reserved bit --
   the first touch of such a page would fault instead of reading. The APs
   do the same before they load this table (ApMain2). */
Hal::EnableWxSupport();
```

The secondary CPUs (the APs) already did things in the right order. They enable NXE first, then load the real page table, then the GDT and the IDT, so that a fault becomes a panic report rather than a reset. Only after that do they run anything that can print. arm64 needs no switch, because PXN and UXN are always honoured.

## A test that fails without the fix

The text probe could not have caught this hole, because the image was never the problem. So `wxprobe` got a second mode. `wxprobe=heap` takes a page the allocator has just handed out, writes a `ret` instruction into it and calls it. On arm64 the probe writes `0xD65F03C0`, which is arm64's own `ret`:

```cpp
static void WxProbeHeap()
{
    static const u8 RetOpcode = 0xC3;

    ulong phys = 0;
    void* va = Mm::AllocMapPages(1, &phys);
    // ...
    *reinterpret_cast<volatile u8*>(va) = RetOpcode;
    Hal::SyncInstructionCache((ulong)va, sizeof(RetOpcode));
    Trace(0, "W^X probe: calling into a heap page 0x%lX (expect page fault)", (ulong)va);
    reinterpret_cast<void (*)()>(va)();
    Trace(0, "W^X probe: heap execute SUCCEEDED (W^X broken!)");
}
```

[wx-test.sh](https://github.com/irqlevel/nos/blob/master/scripts/wx-test.sh) boots the kernel once for each probe. On x86-64 the command line comes from GRUB, so each probe boots from an ISO of its own; on arm64 the script passes it with `-append`. The test passes only if every boot dies with a panic that names the address the probe printed. That address is in `cr2` on x86-64 and in `far` on arm64, the registers that hold the faulting address. A kernel that keeps running prints `SUCCEEDED (W^X broken!)`, and the script fails.

On September 17 the text probe died at the image base. On x86-64 the heap probe died with error code 0x11 (present, instruction fetch) and `rip == cr2`, which means the CPU faulted fetching the instruction itself. On arm64 it died with exception class 0x21, an instruction abort, and `far == elr`, which means the same.

Then we tested the test. We took the one `SetNoExecute()` back out of `MapRangeLocked`. The heap probe printed "SUCCEEDED (W^X broken!)", and wx-test exited 1. A gate (a test that a change has to pass) that passes either way measures nothing. This one measures the fix. It is the gate to run, on both architectures, after touching a mapping path ([Testing](/docs/testing)).

## Modules: the program header decides

Loadable modules ([Modules](/docs/modules)) are the only place where nos puts new code into memory at run time, so they get the strictest form of the rule. A `.ko` is linked with `-z separate-loadable-segments -z max-page-size=4096`, so that every segment starts on a page of its own. That matters because permissions are set per page. Before it maps anything, the loader rejects a segment that asks to be both writable and executable ([module.cpp](https://github.com/irqlevel/nos/blob/master/src/base/kernel/module.cpp)):

```cpp
if ((ph.Flags & Elf::PfW) != 0 && (ph.Flags & Elf::PfX) != 0)
{
    out.Printf("module: segment %lu is both writable and executable\n", i);
    return MakeError(Stdlib::Error::InvalidValue);
}
```

Then the loader maps the image writable (and, since b54a189, non-executable), copies the segments in and relocates them. Only after that does each segment get the permissions its program header asks for, through `SetRangeProtection`: read-only, read-execute or read-write. The TLB is shot down on every CPU, so no CPU keeps a stale translation. On arm64 the new code is also cleaned from the data cache and invalidated in every CPU's instruction cache. At no point during a load is a page of a module both writable and executable.

The C++ fuzzer holds the loader to this for every `.ko` it can build from its input, sound or damaged ([00631ab](https://github.com/irqlevel/nos/commit/00631ab)). Its stand-in for `SetRangeProtection` treats a request for writable and executable as a broken invariant. After a load, every page of the image must carry exactly its own segment's permissions, the instruction cache must be synced, and the shootdown must have been sent ([fuzzing the kernel on the host](/blog/fuzzing-the-kernel-on-the-host)).

## What is left

One thing in nos is still writable and executable, by design: the bootstrap map, built before the real page table exists. It stops translating anything once that table is loaded. After that point everything is W^X, on x86-64 and arm64 alike: the image, the heap, stacks, DMA buffers, MMIO and every module. And the next time a mapping path forgets NX, the result will not be a sentence in a document that quietly disagrees with the code. It will be wx-test exiting 1.
