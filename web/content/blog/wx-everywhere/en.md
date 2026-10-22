---
title: "W^X everywhere: the image, the heap and every module"
description: "No page in nos is both writable and executable. How the image was split, the hole under every runtime mapping, and the test that keeps it shut."
date: 2026-09-17
tags: [memory, safety, x86-64, arm64, modules, testing]
---

W^X is a promise about every page: it may be written or it may be executed, never both. A page that is both turns a stray write into code that runs; a `.text` that can be written turns a wild pointer into a quietly patched kernel. nos wants the opposite, because a failure must be loud: a write into code, or a jump into data, should fault on the spot and say where.

Keeping that promise took two months and both architectures, and part of the work was finding out that the documentation had been making the promise before the code kept it.

## The image, split three ways

The first step, on July 17 (arm64 in [329a8e7](https://github.com/irqlevel/nos/commit/329a8e7), x86-64 the same afternoon in [47ca40e](https://github.com/irqlevel/nos/commit/47ca40e)), was the kernel image. Both linker scripts got page-aligned symbols between the sections, `KernelText` after `.text` and `KernelRodata` after `.rodata`. Late in boot, once the real page table is live, `ProtectRange` walks the image's 4 KiB leaves and rewrites their permissions ([main.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/main.cpp)):

```cpp
pt.ProtectRange(t0, t1 - t0, false, true);   /* RX */
pt.ProtectRange(t1, r1 - t1, false, false);  /* RO+NX */
pt.ProtectRange(r1, e - r1, true, false);    /* RW+NX */
```

On arm64 that means AP[2] for read-only and PXN/UXN for never-execute, which the CPU honours from the start. x86-64 needs two switches, which `Hal::EnableWxSupport()` flips: `EFER.NXE`, without which the NX bit means nothing, and `CR0.WP`, without which ring 0 ignores the read-only bit. W^X without WP would have left the kernel free to write its own `.text`. A boot parameter checked the result: `wxprobe=on` writes to `.text` after the split. x86-64 died with a page fault, error code 0x3 (present, write), arm64 with a permission data abort (DFSC 0x0F), and neither reached the line that would have said the write succeeded.

The same day a review of the arm64 port ([7408805](https://github.com/irqlevel/nos/commit/7408805)) closed a way around all of it. The 1 GiB identity map `boot.S` installs in TTBR0 for the moment the MMU turns on had stayed live forever, and it was RWX, so the kernel text was writable and executable at its physical alias whatever the high-half mapping said. `TCR.EPD0` now switches TTBR0 walks off as soon as execution reaches the high half. On August 29 ([6ebed66](https://github.com/irqlevel/nos/commit/6ebed66)) every MMIO leaf on x86-64 became NX as well (arm64's already were): nothing is ever fetched from a device, and on real hardware a speculative fetch from device memory can have read side effects ([Paging](/docs/paging#mmio-cacheability-and-wx)).

## The hole under the claim

By September CLAUDE.md said that executing from a heap buffer faults, and so did `docs/paging.md`. It did not. `MapRangeLocked`, which builds the leaves behind the heap, task stacks, DMA buffers and module images, wrote them with W and P and never NX, and neither did the code that sets up the `TmpMap` window and `PageArray`. Everything the kernel mapped after boot was writable and executable at once, on both architectures. Only the image, MMIO and a module's finished segments carried NX.

Nothing had noticed, because nothing breaks. A missing NX bit is invisible until something executes data, and no smoke boot does that on purpose. It is exactly the kind of failure this project fears most: one that looks like success.

The fix ([b54a189](https://github.com/irqlevel/nos/commit/b54a189), September 17) puts NX on every leaf the kernel writes at run time ([page_table.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/mm/page_table.cpp)):

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

`SetupPage`, which builds the table at boot, now takes `executable` as an argument, and exactly one caller passes `true`: the kernel image, which the kernel runs out of from the root switch until `ProtectRange` splits it. The tables above the leaves stay permissive on purpose. On both architectures an NX bit on an L4, L3 or L2 entry takes execute permission away from the whole subtree, and no leaf can give it back, and a module's text lives under those tables. Whether a page runs is the leaf's decision.

## Bit 63 is reserved until it isn't

On x86-64 the fix had a trap in it. Until `EFER.NXE` is set, bit 63 of a page-table entry does not mean "no execute": it is a reserved bit, and an entry that carries it faults on the first touch. `EnableWxSupport()` used to run just before `ProtectRange`, which was fine while nothing before that point set NX. Now the `TmpMap` window and the early drain of the free-page list set it, and they run long before, so the call moved to the top of `Main2`, ahead of any page table:

```cpp
/* Before any page table is built: every mapping the kernel makes at
   runtime carries NX, and with EFER.NXE off bit 63 is a reserved bit --
   the first touch of such a page would fault instead of reading. The APs
   do the same before they load this table (ApMain2). */
Hal::EnableWxSupport();
```

The APs already had the order right: NXE first, then the real page table, then the GDT and the IDT so that a fault is a panic report rather than a reset, and only then anything that can print. arm64 needs no switch; PXN and UXN are always honoured.

## A test that fails when the fix is taken out

The text probe could not have caught this hole, because the image was never the problem. So `wxprobe` grew a second mode. `wxprobe=heap` takes a page the allocator has just handed out, writes a `ret` into it and calls it; arm64's twin writes `0xD65F03C0`, its own `ret`:

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

[wx-test.sh](https://github.com/irqlevel/nos/blob/master/scripts/wx-test.sh) boots the kernel once per probe (on x86-64 from an ISO of its own for each, since the command line comes from GRUB; on arm64 with `-append`) and passes only if each boot dies with a panic that names the address the probe printed: `cr2` on x86-64, `far` on arm64. A kernel that keeps running prints `SUCCEEDED (W^X broken!)`, and the script fails. On September 17 the text probe died at the image base. The heap probe died with error code 0x11 (present, instruction fetch) and `rip == cr2` on x86-64, and with exception class 0x21, an instruction abort, and `far == elr` on arm64.

Then the test itself was tested. With the one `SetNoExecute()` in `MapRangeLocked` taken back out, the heap probe printed "SUCCEEDED (W^X broken!)" and wx-test exited 1. A gate that passes either way measures nothing; this one measures the fix. It is the gate to run after touching a mapping path, on both architectures ([Testing](/docs/testing)).

## Modules: the program header decides

Loadable modules ([Modules](/docs/modules)) are the one place where nos puts new code into memory at run time, so they get the strictest form of the rule. A `.ko` is linked with `-z separate-loadable-segments -z max-page-size=4096`, so that every segment starts on a page of its own, since permissions are per page. Before it maps anything, the loader refuses a segment that asks for both ([module.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/module.cpp)):

```cpp
if ((ph.Flags & Elf::PfW) != 0 && (ph.Flags & Elf::PfX) != 0)
{
    out.Printf("module: segment %lu is both writable and executable\n", i);
    return MakeError(Stdlib::Error::InvalidValue);
}
```

It then maps the image writable, and since b54a189 non-executable, copies the segments in and relocates them. Only after that does each segment get the permissions its program header asks for, read-only, read-execute or read-write, through `SetRangeProtection`. Every CPU's TLB is shot down, and on arm64 the new code is cleaned from the data cache and invalidated in every CPU's instruction cache. At no point of a load is a page of a module both writable and executable.

The C++ fuzzer holds the loader to that for every `.ko` it can build from its input, sound or damaged ([00631ab](https://github.com/irqlevel/nos/commit/00631ab)). Its stand-in for `SetRangeProtection` treats a request for writable and executable as a broken invariant, and after a load every page of the image must carry exactly its own segment's permissions, with the instruction cache synced and the shootdown sent ([fuzzing the kernel on the host](/blog/fuzzing-the-kernel-on-the-host)).

## What is left

One thing in nos is still writable and executable, by design: the bootstrap map built before the real page table exists, which stops translating anything once that table is loaded. After that point everything is W^X, on x86-64 and arm64 alike: the image, the heap, stacks, DMA buffers, MMIO and every module. And the next time a mapping path forgets NX, it will not be a sentence in a document quietly disagreeing with the code. It will be wx-test exiting 1.
