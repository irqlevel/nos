# Paging and memory

Four-level paging with 4 KiB pages, a higher-half kernel and no user space
yet. This page covers the whole path: the bootstrap map the assembly entry
builds, the real page table that replaces it, how physical memory becomes a
free list, the allocators layered on top, and the rules that keep them from
deadlocking. The code lives in `src/cpp/mm/`, with the PTE encoding in
`arch/*/pte.h` behind `hal/pte.h`.

## The address space

| Range | Contents |
|---|---|
| `0x0000000000000000` – `0x00007FFFFFFFFFFF` | User half. Mapped only by the x86 bootstrap table, and only until the real table takes over; on arm64 TTBR0 walks are disabled outright after boot |
| `0xFFFF800000000000` + phys | `KernelSpaceBase`. The bootstrap linear map, and the permanent home of every MMIO mapping |
| `0xFFFF800001000000` | Kernel image (x86-64; arm64 links at `…40200000`, matching the QEMU virt load address). Split into text RX / rodata RO+NX / data RW+NX late in boot |
| just above the image | The `TmpMap` window: 512 slots, one L1 table, the only way the kernel touches an arbitrary physical page |
| next huge-page boundary | `PageArray`, one 32-byte descriptor per 4 KiB frame, mapped in 2 MiB pages |
| after `PageArray` | The `VaAllocator` arena the page allocator hands out |

## Two page tables

**`BuiltinPageTable`** is the bootstrap linear map: `phys` ↔
`phys + KernelSpaceBase`, arithmetic rather than a walk. On x86 it is the C++
copy of what `boot64.asm` built — four 1 GiB PDPT entries of 2 MiB pages,
covering the low 4 GiB — installed in `Main2` before GRUB's tables are
overwritten. On arm64 the equivalent is built in `arch/arm64/builtin_pt.cpp`.

`MapHighRam()` extends it once the firmware memory map is known: one 1 GiB
block per GiB of RAM above 4 GiB, skipping blocks with no usable RAM in
them. On x86 that needs CPUID PDPE1GB (everything since Nehalem and
Barcelona has it; QEMU's default CPU model does not advertise it, so the
fallback is a path that actually gets exercised); on arm64 a 1 GiB L1 block
is always available at a 4 KiB granule. 512 GiB is where one PDPT/L1 table
runs out, and that is the kernel's ceiling.

The low 4 GiB deliberately stays at 2 MiB granularity even after the
extension: below that line the MTRRs describe MMIO holes with memory types
other than write-back, and a large page spanning two MTRR types is left to
the implementation — in practice the most conservative type wins, which
would make a whole GiB of RAM uncached.

Why this matters: **the bootstrap map's extent is the hard ceiling on the
RAM the kernel can use**. Building the free list threads a next-pointer
through the free pages themselves, which means writing to every one of them
through this map. RAM above `GetMappedLimit()` is real, reported by the
firmware, and unusable — so `GetFreePages` prints both numbers, and
`meminfo` shows them, rather than letting the difference be silent.

**`PageTable`** is the real thing: a 4-level table built page by page, with
`Root` loaded into CR3 (x86) or TTBR1 (arm64) partway through `Main2`. From
that point the linear map is gone and every access to an arbitrary physical
page goes through `TmpMap`.

## `TmpMap`: the window onto physical memory

512 consecutive VAs sharing one L1 table, guarded by `TmpMapLock`.
`TmpMapPage(phys)` finds a free slot, writes the leaf PTE, invalidates the
local TLB and returns the VA; `TmpUnmapPage` reverses it. `TmpMapRange`
takes a run of adjacent slots for a structure that crosses a page boundary
(ACPI tables, mostly).

Two details that are easy to get wrong:

- RAM is mapped write-back, everything else uncached — `TmpMap` serves both
  free pages and MMIO, and `MemoryMap::IsUsableRam` is what tells them apart.
- The slots are shared, and unmapping invalidates only the *local* TLB. So
  `VirtToPhys` — a four-deep walk, each level mapped and unmapped in turn —
  disables preemption for the whole walk: a migration mid-walk would leave a
  stale mapping on the original CPU that a later slot reuse would
  mistranslate.

## From firmware map to free list

`PageTable::Setup()` runs in this order, and each step depends on the one
before:

1. `HighestPhyAddr` is the top of usable RAM, clamped to the bootstrap map.
2. `PageArray` is sized (one `Page` per frame — 512 MiB on a 64 GiB box) and
   given a physically contiguous, huge-page-aligned home by
   `ReservePageArray`, clear of the kernel image and of every reserved
   region. That home is then **added to the memory map as reserved**, which
   is the entire mechanism that keeps the free list off it.
3. `GetFreePages()` walks the usable regions and threads a singly linked
   list through the pages themselves. It skips reserved runs boundary to
   boundary rather than testing each page (a real server has twenty-odd
   reserved regions and sixteen million pages), skips the kernel image, and
   sorts pages whose identity address is shadowed by the kernel's own VA
   window onto a *second* list — `ExcludedPages` — because `Setup` still
   reaches its allocations through the bootstrap map.
4. The kernel image, the `TmpMap` slots and its L1 table get real 4 KiB
   mappings; `PageArray` gets 2 MiB ones (512 times fewer leaf entries, and
   a TLB that can actually cover it).
5. Every descriptor is initialised through the bootstrap map, with its list
   links pointed at the address the descriptor will answer to once
   `PageArray` is live — "self-pointing means not on the free list" is the
   test `AllocContiguousPages` uses.

`SetupFreePagesList()` then runs `DrainEarlyFreeList` twice: once for the
main list, once for the excluded one, which is safe by then because all page
access goes through `TmpMap`. The drain holds a single `TmpMap` slot across
the whole walk instead of using the general pair — one TLB invalidation and
one lock round trip per page instead of two of each.

Nothing is zeroed on the way to the list: `AllocPageNoLock` zeroes every page
it hands out, so a page sitting on the free list is not expected to be clean.
Even so, building the list touches every page twice, about 0.6 µs each, so a
64 GiB machine spends roughly ten seconds of its boot here — hence the
`mm: … MiB onto the free list` progress lines, one per 8 GiB, so a long boot
does not look like a hang.

## Page descriptors and refcounting

`Page` is 32 bytes: a list entry, an atomic refcount, a physical address.
The refcounting is explicit and the easiest thing here to get wrong:

| Call | Effect on the refcount |
|---|---|
| `MapPage(va, page)` | `+1` |
| `GetPage(phys)` | `+1` — the caller must `Put()` |
| `UnmapPage(va)` | net 0 (it does a `GetPage` and a `Put` internally), returns the `Page*` |
| `FreePage(page)` | back onto the free list; does not touch the refcount |

To undo a `MapPage`: `UnmapPage` + `Put`. To free a mapped page:
`UnmapPage` + `FreePage` + `Put`.

The range forms — `MapPages` (array of `Page*`), `MapContiguousPages` (a run
of descriptors), `MapPhysPages` (physical addresses of pages that already
exist) — share one walk down L4/L3/L2 and one temp mapping of the L1 table,
so an n-page block costs 4 temp mappings instead of 4n. All three are
all-or-nothing, and all of them invalidate only the local TLB: the caller
shoots down the other CPUs once for the whole range.

## The allocators

Three layers, each built on the one below.

**`PageTable::AllocPage` / `AllocContiguousPages(n ≤ 128)`** — frames off
the free list. A contiguous run is found by walking the free list and, for
each candidate, checking that the next n-1 descriptors in `PageArray` are
physically consecutive and still free — a self-pointing list entry is what
marks a descriptor as allocated. The walk is deliberately unbounded (a cap
would turn a slow allocation into a spurious OOM, and plain `new` panics on
that) but says so in the log once it has examined more than a thousand
entries, because it runs with the page-table lock held.

**`PageAllocatorImpl`** — eight `FixedPageAllocator` buckets, for 1, 2, 4 …
128 pages. The VA arena starts where `PageArray` ends and spans 70% of the
free page count; each bucket gets an eighth of it and tracks it with a
`VaAllocator` bitmap, whose blocks are aligned to their own size. The
`Mm::` surface on top of it (`mm/new.h`):

| API | Allocates frames? | Tracked VA? | Frees frames on release? |
|---|---|---|---|
| `Mm::Alloc` / `Mm::Free` | yes | yes (via the pools) | yes |
| `Mm::AllocMapPages` / `UnmapFreePages` | yes, contiguous, returns the physical address too | yes | yes |
| `Mm::MapPages` / `UnmapPages` | no — the caller supplies the physical addresses | yes | no |

**`AllocatorImpl` + `Pool`** — power-of-two size classes for small
allocations, one `Pool` per class, each carving 4 KiB pages into blocks with
a small per-block header (a list link and the caller's tag, which is what
makes a leak report name its owner). An 8-byte header goes on every request,
so the smallest class actually reached is 16 bytes and the largest is 2 KiB;
a request of 2040 bytes or more goes straight to the page allocator. `Free`
tells the two apart by alignment — a page-aligned pointer came from the page
allocator — and checks a magic word otherwise. Freed blocks are stamped with
a poison tag, which is how a double free is caught.

`operator new` sits on top of `Mm::Alloc`, and its contract is unusual:
**plain `new T(...)` panics on OOM and never returns nullptr**. The
replaceable plain form is implicitly potentially-throwing, so the compiler
runs the constructor on the result unchecked and may delete a caller's null
test — a null check after plain `new` is dead code. An allocation that wants
to observe OOM must use `new (Mm::NoThrow) T(...)` or
`Mm::TAlloc<T, Tag>()`, both of which are checked. The reasoning is in
`mm/new.h`.

## MMIO, cacheability and W^X

`MapMmioRegion(phys, size, policy)` builds 4 KiB leaf entries at
`phys + KernelSpaceBase`, allocating table pages as it goes. Two policies:
`MmioUncached` for device registers (every load and store reaches the
device, in program order) and `MmioWriteCombining` for memory-like device
RAM with no read side effects — a framebuffer — where stores may be
buffered, merged and reordered, which is what makes drawing affordable. The
write-combining writer must call `Hal::WcFlush()` when the pixels have to be
visible, and must never use it for registers; on a CPU without PAT the
policy silently degrades to uncached.

Every MMIO mapping is non-executable: nothing is ever fetched from a device,
and on real hardware a speculative fetch from device memory can trigger read
side effects.

These mappings live outside the `VaAllocator`, are permanent, and invalidate
only the local TLB — so **`MapMmioRegion` callers must run on the BSP before
the APs start**. On arm64 a request that falls inside the boot device GiB is
answered from the premapped window instead (`Hal::MmioPremappedVa`).

Never map DMA or device memory at `phys + KernelSpaceBase` by hand: go
through `Mm::MapPages` / `Mm::AllocMapPages` so the VA is tracked.

**W^X** is `ProtectRange` walking 4 KiB leaves and clearing the writable or
setting the no-execute bit. Late in boot the kernel image is split into text
RX, rodata RO+NX, data RW+NX on both architectures. Self-modifying code,
executing from a heap buffer, or writing through a pointer into `.rodata`
faults instead of silently working — `wxprobe=on` deliberately does the last
of those to prove the protection is on.

## TLB shootdown, and the deadlock it can cause

Every mapping change invalidates only the issuing CPU's TLB.
`CpuTable::InvalidateTlbAll/Address/Range` fan the invalidation out to the
others as an IPI task and wait for every one to acknowledge — except on
arm64, where `TLBI` broadcasts in hardware and `Hal::TlbShootdownNeedsIpi()`
answers false.

This is the origin of the single most important rule in the codebase:
**never allocate or free with a spinlock held.** `Mm::Free`/`Mm::Alloc` can
reach the page allocator, which shoots down every other CPU and waits for
all of them to answer. A CPU spinning on the lock you hold has interrupts
off and can never answer, so the two wait for each other forever and the
machine stops dead with no panic and nothing to reset it. Collect what needs
releasing into a local list under the lock, unlock, then release. Better
still, do not allocate on a datapath at all — network frames come from a
pool built once at boot for exactly this reason.

`SendTlbIPI` gives up after ten seconds and panics naming the CPUs that never
answered, so a new instance of this bug is a report rather than a dead
machine.

## Looking at it from the shell

- `meminfo` — the firmware memory map, and how much of it the kernel
  actually uses (RAM reported vs. RAM reachable).
- `memusage` — free and total page counts.
- `memcheck` — `CheckFreeList` walks every descriptor and reports any page
  that is on the free list but must not be: inside a reserved region, inside
  the kernel image, or outside usable RAM. That invariant is what the whole
  carve-out scheme rests on, and its violation is silent until something
  overwrites `PageArray` or an ACPI table.
