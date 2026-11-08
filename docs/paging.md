# Paging and memory

Four-level paging with 4 KiB pages, a higher-half kernel and no user space
yet. This page covers the whole path: the bootstrap map the assembly entry
builds, the real page table that replaces it, how physical memory becomes a
free list, the allocators layered on top, and the rules that keep them from
deadlocking. The code lives in `src/base/mm/`, with the PTE encoding in
`arch/*/pte.h` behind `hal/pte.h`.

## The address space

| Range | Contents |
|---|---|
| `0x0000000000000000` – `0x00007FFFFFFFFFFF` | User half. Mapped only by the x86 bootstrap table, and only until the real table takes over; on arm64 TTBR0 walks are disabled outright after boot |
| `0xFFFF800000000000` + phys | `KERNEL_SPACE_BASE`. The bootstrap linear map, and the permanent home of every MMIO mapping |
| `0xFFFF800001000000` | Kernel image (x86-64; arm64 links at `…40200000`, matching the QEMU virt load address). Split into text RX / rodata RO+NX / data RW+NX late in boot |
| just above the image | The `tmp_map` window: 512 slots, one L1 table, the only way the kernel touches an arbitrary physical page -- 448 shared under a lock, and one for each CPU for copying a frame |
| next huge-page boundary | `page_array_`, one 32-byte descriptor per 4 KiB frame, mapped in 2 MiB pages |
| after `page_array_` | The `VaAllocator` arena the page allocator hands out |

## Two page tables

**`BuiltinPageTable`** is the bootstrap linear map: `phys` ↔
`phys + KERNEL_SPACE_BASE`, arithmetic rather than a walk. On x86 it is the C++
copy of what `boot64.asm` built — four 1 GiB PDPT entries of 2 MiB pages,
covering the low 4 GiB — installed in `main2` before GRUB's tables are
overwritten. On arm64 the equivalent is built in `arch/arm64/builtin_pt.cpp`.

`map_high_ram()` extends it once the firmware memory map is known: one 1 GiB
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
through this map. RAM above `get_mapped_limit()` is real, reported by the
firmware, and unusable — so `get_free_pages` prints both numbers, and
`meminfo` shows them, rather than letting the difference be silent.

**`PageTable`** is the real thing: a 4-level table built page by page, with
`Root` loaded into CR3 (x86) or TTBR1 (arm64) partway through `main2`. From
that point the linear map is gone and every access to an arbitrary physical
page goes through `tmp_map`.

## `tmp_map`: the window onto physical memory

512 consecutive VAs sharing one L1 table. The first 448 are shared, guarded
by `tmp_map_lock_`: `tmp_map_page(phys)` finds a free slot, writes the leaf PTE,
invalidates the local TLB, takes a reference on the page and returns the
VA; `tmp_unmap_page` reverses it. `tmp_map_range` takes a run of adjacent slots
for a structure that crosses a page boundary (ACPI tables, mostly).

Both flush only the TLB of the CPU they run on, so a slot is used on the CPU
that mapped it, and unmapped there: the caller is held to one CPU from the
map to the unmap -- a spinlock (the page-table walks hold `lock_`),
`preempt_disable_task` (`zero_page`), interrupts off (`virt_to_phys`). A task
moved half way would leave the slot's translation cached on the CPU it left,
for the next user of the slot there to read and write through. What maps a
slot for good -- the VGA buffer, the local APIC, the IO-APIC -- does it
before the scheduler preempts, before any other CPU runs. Both ends check:
`BUG_ON(preempt_can_move())`, a panic at the first call made by a caller
that could be moved, whether or not a move happened to cut it.

The last 64 are a slot for each CPU, and serve one thing: a copy into or
out of a frame -- a page handed out by address and mapped nowhere, what a
guest's memory is made of (`kcore::frame`) -- which is every read and write
of a guest's memory but its devices' rings, which are mapped through
`mm::map_pages` while a queue is up (`kcore::frame::Mapped`). `map_frame_slot(phys)` maps this CPU's slot and
`unmap_frame_slot` clears it, with interrupts off from one to the other,
which makes the slot the copy's alone: no lock, no slot to search for, no
reference (the frame is the caller's for the call), no memory-map lookup
(a frame is RAM), and one TLB invalidation instead of two -- the slot was
not present, and no TLB keeps anything of an entry that was not
(`hal::pte_made_valid` is x86's compiler barrier and arm64's `dsb ishst;
isb`). The frame is mapped only for the length of its copy, and nowhere
the moment it is done. Through the shared slots, their lock and their two
flushes had cost a busy guest's vCPU as much as the copy itself.

Two details that are easy to get wrong:

- RAM is mapped write-back, everything else uncached — `tmp_map` serves both
  free pages and MMIO, and `MemoryMap::is_usable_ram` is what tells them apart.
- The slots are shared, and unmapping invalidates only the *local* TLB. So
  `virt_to_phys` — a four-deep walk, each level mapped and unmapped in turn —
  disables preemption for the whole walk: a migration mid-walk would leave a
  stale mapping on the original CPU that a later slot reuse would
  mistranslate.

## From firmware map to free list

`PageTable::setup()` runs in this order, and each step depends on the one
before:

1. `highest_phy_addr_` is the top of usable RAM, clamped to the bootstrap map.
2. `page_array_` is sized (one `Page` per frame — 512 MiB on a 64 GiB box) and
   given a physically contiguous, huge-page-aligned home by
   `reserve_page_array`, clear of the kernel image and of every reserved
   region. That home is then **added to the memory map as reserved**, which
   is the entire mechanism that keeps the free list off it.
3. `get_free_pages()` walks the usable regions and threads a singly linked
   list through the pages themselves. It skips reserved runs boundary to
   boundary rather than testing each page (a real server has twenty-odd
   reserved regions and sixteen million pages), skips the kernel image, and
   sorts pages whose identity address is shadowed by the kernel's own VA
   window onto a *second* list — `excluded_pages_` — because `setup` still
   reaches its allocations through the bootstrap map.
4. The kernel image, the `tmp_map` slots and its L1 table get real 4 KiB
   mappings; `page_array_` gets 2 MiB ones (512 times fewer leaf entries, and
   a TLB that can actually cover it).
5. Every descriptor is initialised through the bootstrap map, with its list
   links pointed at the address the descriptor will answer to once
   `page_array_` is live — "self-pointing means not on the free list" is the
   test `alloc_contiguous_pages` uses.

`setup_free_pages_list()` then runs `drain_early_free_list` twice: once for the
main list, once for the excluded one, which is safe by then because all page
access goes through `tmp_map`. The drain holds a single `tmp_map` slot across
the whole walk instead of using the general pair — one TLB invalidation and
one lock round trip per page instead of two of each.

Nothing is zeroed on the way to the list: a page goes on as the firmware,
the loader or the last boot left it, and is zeroed before anyone is handed it
-- by the allocation, or by the zeroing tasks first ([below](#zeroed-ahead-free-pages-and-the-two-lists)).
Even so, building the list touches every page twice, about 0.6 µs each, so a
64 GiB machine spends roughly ten seconds of its boot here — hence the
`mm: … MiB onto the free list` progress lines, one per 8 GiB, so a long boot
does not look like a hang.

## Zeroed ahead: free pages and the two lists

Every page is handed out zeroed -- `alloc_page`, `alloc_page_keeping` (a
guest's memory, through `kernel_frame_alloc`), `alloc_contiguous_pages`, a
block of the huge pool -- so what one owner of a page finds in it is never
what the last one left. What a page held stays in RAM, though, from its free
until it is next handed out: a guest's memory, once the guest is gone, is
there for as long as nothing asks for it. So free pages are zeroed ahead, by
two tasks of the lowest priority (`mm/page_zeroer.cpp`, `zerofree=`), and
the free pages are on two lists:

- the **other list**, `free_pages_list_` -- pages as their last owner left
  them. A free goes on its head; `alloc_page` and the kernel's own
  allocations take from there first and zero what they take, while it is
  still in the cache.
- the **zeroed list**, `zeroed_pages_list_` -- pages known to read as zeros,
  each marked (bit 0 of its descriptor's address, `Page::ZEROED_BIT`, which a
  page-aligned address leaves free). `alloc_page_keeping` takes from there
  first and does not zero; the kernel's own allocations take from it only
  when the other list is empty.

`zero_free_page(keep)` moves a page from the other list's cold end to the
zeroed one: the page is taken off under the page-table lock, zeroed off it
through the CPU's frame slot, and put on the zeroed list, interrupts off from
the take to the put -- a microsecond. Meanwhile it is on neither list but
still counted free (it is nobody's), and marked, so that a free of it -- a
page freed twice -- panics (`free_page_no_lock`: a page handed out never has
the mark). A page is taken only while more than `keep` are free; the tasks
keep the kernel's reserve, where the one page off the lists can make no
allocation fail. `alloc_contiguous_pages` finds a run across both lists and,
once it has let go of the lock, zeroes only the pages of it that were not
marked.

The huge pool has a third bitmap, `huge_pool_zeroed_bits_`, of its free blocks
known zero. `zero_free_huge_page` zeroes a page of a free block a call, under
the pool's own lock (`huge_pool_lock_`, so that it keeps no 4 KiB allocation
waiting), the block staying free throughout; once all 512 are done the block
is known zero, and `alloc_huge_frame`, which takes such a block first, hands
it out as it is. With none known zero it takes the block the zeroing is part
way through next, and zeroes only the pages the zeroing has not done -- those
before `huge_zero_next_` were zeroed under the pool's lock, and the block has
been free since -- while the zeroing goes on with another. Both look for
blocks from the same place, so while a guest takes its memory they meet
block after block: a zeroing task on an idle CPU does part of each of the
guest's blocks, and the allocation the rest, rather than all of it again.

The tasks (`PageZeroer`): one starts with the huge pool, the other with the 4
KiB pages, each going on to the other's work when its own has none. They are
free to move, so the load balancer takes one displaced from a busy CPU to an
idle one, and nice 19; a task zeroes only while its CPU runs nothing else --
the runnable count, looked at between pages -- in batches of 256 pages, and
waits a second when there is nothing left, so that its CPU sleeps deep. On a
2 GiB arm64 machine under HVF they had all 1.94 GiB of free memory zeroed
within a second of `boot: complete`, a batch of 256 pages taking 280-460 us.
Each batch is a record (`trace on mm`, [Tracing](tracing.md#the-memorys-events)).

`zerofree=off` starts no task: nothing is zeroed ahead, and every page is
zeroed as it is handed out, as before. `zerofree=verify` reads every page and
block an allocation takes as known zero before handing it out, and a word that
is not zero is a panic naming the page: something wrote to a free page -- a
use after free -- caught where it would otherwise have reached a guest.

## Page descriptors and refcounting

`Page` is 32 bytes: a list entry, an atomic refcount, a physical address.
The refcounting is explicit and the easiest thing here to get wrong:

| Call | Effect on the refcount |
|---|---|
| `map_page(va, page)` | `+1` |
| `get_page(phys)` | `+1` — the caller must `put()` |
| `unmap_page(va)` | net 0 (it does a `get_page` and a `put` internally), returns the `Page*` |
| `free_page(page)` | back onto the free list; does not touch the refcount |

To undo a `map_page`: `unmap_page` + `put`. To free a mapped page:
`unmap_page` + `free_page` + `put`.

The range forms — `map_pages` (array of `Page*`), `map_contiguous_pages` (a run
of descriptors), `map_phys_pages` (physical addresses of pages that already
exist) — share one walk down L4/L3/L2 and one temp mapping of the L1 table,
so an n-page block costs 4 temp mappings instead of 4n. All three are
all-or-nothing, and all of them invalidate only the local TLB: the caller
shoots down the other CPUs once for the whole range.

## The allocators

Three layers, each built on the one below.

**`PageTable::alloc_page` / `alloc_contiguous_pages(n ≤ 128)`** — frames off
the free list. A contiguous run is found by walking the free list and, for
each candidate, checking that the next n-1 descriptors in `page_array_` are
physically consecutive and still free — a self-pointing list entry is what
marks a descriptor as allocated. The walk is deliberately unbounded (a cap
would turn a slow allocation into a spurious OOM, and plain `new` panics on
that) but says so in the log once it has examined more than a thousand
entries, because it runs with the page-table lock held. The run is zeroed
after the lock is let go, a page at a time, as `alloc_page` zeroes a page:
under the lock, a run of 128 pages was half a megabyte of stores with
interrupts off and every other CPU's allocation waiting for it.

**`PageAllocatorImpl`** — eight `FixedPageAllocator` buckets, for 1, 2, 4 …
128 pages. The VA arena starts where `page_array_` ends and spans 70% of the
free page count; each bucket gets an eighth of it and tracks it with a
`VaAllocator` bitmap (`mm/va_allocator.h`), whose blocks are aligned to their
own size. The
`mm::` surface on top of it (`mm/new.h`):

| API | Allocates frames? | Tracked VA? | Frees frames on release? |
|---|---|---|---|
| `mm::alloc` / `mm::free` | yes | yes (via the pools) | yes |
| `mm::alloc_map_pages` / `unmap_free_pages` | yes, contiguous, returns the physical address too | yes | yes |
| `mm::map_pages` / `unmap_pages` | no — the caller supplies the physical addresses | yes | no |

**`AllocatorImpl` + `Pool`** — power-of-two size classes for small
allocations, one `Pool` per class, each carving 4 KiB pages into blocks with
a small per-block header (a list link and the caller's tag, which is what
makes a leak report name its owner). An 8-byte header goes on every request,
so the smallest class actually reached is 16 bytes and the largest is 2 KiB;
a request of 2040 bytes or more goes straight to the page allocator. `Free`
tells the two apart by alignment — a page-aligned pointer came from the page
allocator — and checks a magic word otherwise. A free block carries a poison
tag, swapped for the owner's and back by compare-and-swap, which is how a
double free is caught — on two CPUs at once too — and a block handed out
twice, or written into while free.

What the heap keeps between a free and the next allocation, so that neither
costs what the first one did (`heapbench`, below, measures each):

- **A cache per CPU in front of the pools.** A few free blocks of each size,
  under a lock only that CPU takes, traded with the pool half a cache at a
  time — so allocations of one size on several CPUs do not all queue on the
  pool's lock. Used once the CPU can say which it is (`hal::irq_chip_ready`).
- **An empty page per pool.** The largest class's block has a page to
  itself, so without it every allocation and free of 1 to 2 KiB was a page
  mapped and unmapped.
- **Freed blocks of the page allocator, still mapped** — a bounded list per
  size, a thousandth of RAM at most and never more than 512 KiB a size. A
  block that comes back costs no map and, on x86, no TLB shootdown of every
  other CPU. `mm::alloc` zeroes a kept block, as fresh pages always are;
  `mm::alloc_uninit`, what Rust's global allocator calls, does not —
  `GlobalAlloc::alloc` promises no contents, and `alloc_zeroed` zeroes on the
  Rust side. DMA buffers (`unmap_free_pages`) are never kept.

`AllocatorImpl::trim` gives all of it back; an allocation that finds no page
or no VA trims the page allocator's lists and tries once more before refusing.

### What Rust holds of it

Rust's global allocator -- every `Box`, `Vec`, `String` and `Arc` of the
image and of every module -- reaches the heap through `kernel_alloc` and
`kernel_free`, which count it (`mm/heap_usage.{h,cpp}`): the bytes asked
for; what they took once the heap rounded them up
(`AllocatorImpl::footprint`: a 16-byte `Box` takes a 32-byte block and the
pool's 24-byte header, a 300 KiB `Vec` a run of 512 KiB, every page of it
backed); the blocks; and the most of each there has been at once -- with the
allocations and frees since boot, and the allocations the heap had nothing
for, a `try_reserve` that failed and was handled among them. `memusage`
shows them, and `system`'s `memory` gives them to the host API
([Management](mgmt.md)); on a 1 GiB arm64 QEMU just booted:

```
free_pages_lowest: 248837
rust_heap: 16725 KiB taken, 8688 KiB asked, 4303 blocks
rust_heap_peak: 17395 KiB taken, 9342 KiB asked, 5565 blocks (to within 64 KiB and 256 blocks a cpu)
rust_heap_since_boot: 8956 allocations, 4653 frees, 0 refused
```

What is taken there is nearly twice what was asked for: the classes are
powers of two, and a run of pages one too. Not in them: the C++ objects
the bridge makes for Rust's handles (a `Mutex`, an `Event`, a `Task`), DMA
buffers and frames, which are not the global allocator's.

There is no lock in the counting. A change goes into this CPU's slot, a
cache line of its own, with interrupts off for the few loads and stores it
takes: nothing else writes the slot meanwhile -- not another CPU, not an
interrupt on this one -- so not even a locked instruction. The totals the
CPUs share are reached only when a slot has moved 64 KiB or 256 blocks either
way since it was last folded into them -- an atomic add, and a
compare-and-swap for a peak that rises -- and that is where the peaks are
kept, since the most the sum has held is no sum of what each slot held. A
peak is therefore the truth to within a slot's worth a CPU, either way; a
read sums the totals and every slot. A slot is 128 bytes, not 64: Apple's
cores have lines that long and x86's prefetcher takes lines in pairs.

What it costs, `heapbench` on the tree before and after (arm64, HVF on an
M-series Mac, the two booted in turn, the median of nine runs each): an
allocation and its free 6 to 9 ns more on one CPU and on four -- 6 to 8% on
the pools' sizes, 9 to 11% on runs of pages; a block of 64 KiB or more,
which is past a slot's worth and so reaches the totals at every allocation
and free, 24 ns more (27%). The first cut had 64-byte slots and took the
class by `log2`'s loop over the bits rather than the count of leading zeros
(`AllocatorImpl::footprint`, `stdlib::log2_clz`), and cost 9 to 15 ns on one
CPU and 20 to 30 on four.

`free_pages_lowest`, beside them, is the fewest pages there have been free
since the free list was built: how near the machine has come to none, with
guests' frames and the huge pool in it. It is lowered where the count is,
under the page table's lock.

`operator new` sits on top of `mm::alloc`, and its contract is unusual:
**plain `new T(...)` panics on OOM and never returns nullptr**. The
replaceable plain form is implicitly potentially-throwing, so the compiler
runs the constructor on the result unchecked and may delete a caller's null
test — a null check after plain `new` is dead code. An allocation that wants
to observe OOM must use `new (mm::NO_THROW) T(...)` or
`mm::talloc<T, Tag>()`, both of which are checked; from `stdlib::` code the
tag is spelled in full, `new (kernel::mm::NO_THROW) T[...]`, as in
`lib/vector.h`. The reasoning is in `mm/new.h`. There is no way to make the
plain form nullable instead: a `noexcept` redeclaration of it does not
compile, and the toolchain's clang parses `-fcheck-new` without implementing
it.

## MMIO, cacheability and W^X

`map_mmio_region(phys, size, policy)` builds 4 KiB leaf entries at
`phys + KERNEL_SPACE_BASE`, allocating table pages as it goes. Two policies:
`MmioUncached` for device registers (every load and store reaches the
device, in program order) and `MmioWriteCombining` for memory-like device
RAM with no read side effects — a framebuffer — where stores may be
buffered, merged and reordered, which is what makes drawing affordable. The
write-combining writer must call `hal::wc_flush()` when the pixels have to be
visible, and must never use it for registers; on a CPU without PAT the
policy silently degrades to uncached.

Every MMIO mapping is non-executable: nothing is ever fetched from a device,
and on real hardware a speculative fetch from device memory can trigger read
side effects.

These mappings live outside the `VaAllocator`, are permanent, and invalidate
only the local TLB — so **`map_mmio_region` callers must run on the BSP before
the APs start**. On arm64 a request that falls inside the boot device GiB is
answered from the premapped window instead (`hal::mmio_premapped_va`).

Never map DMA or device memory at `phys + KERNEL_SPACE_BASE` by hand: go
through `mm::map_pages` / `mm::alloc_map_pages` so the VA is tracked.

**W^X** has two halves. Every mapping the kernel makes at runtime is
non-executable from birth: `map_range_locked` sets NX on each leaf it writes,
so the heap, task stacks, DMA buffers, the temp-map window and `page_array_`
are RW+NX, and so is MMIO (`map_mmio_region`). Nothing walks that back except
the module loader, which asks for each segment's permissions by name once
the segment is written (`set_range_protection`), so a module's text is RX and
nothing is ever writable and executable at once. The tables above the leaves
stay permissive: on both architectures an NX bit on an L4/L3/L2 entry takes
execute permission away from the whole subtree and no leaf can give it back.

The other half is the kernel image, which is the one executable mapping the
page table is built with — the kernel runs out of it from the root switch
onwards. Late in boot `protect_range` walks its 4 KiB leaves and splits it
into text RX, rodata RO+NX, data RW+NX on both architectures.

So self-modifying code, executing from a heap buffer, or writing through a
pointer into `.rodata` faults instead of silently working. `wxprobe=text`
deliberately writes to `.text` and `wxprobe=heap` deliberately calls into a
page the allocator just handed out, to prove both halves are on;
`scripts/wx-test.sh` runs both boots and fails if either survives.

On x86-64 the NX bit means nothing until `EFER.NXE` is set, and bit 63 is a
*reserved* bit until it is — a PTE carrying it faults on the first touch. So
`hal::enable_wx_support()` runs before any page table is built on the BSP, and
on each AP before it loads the kernel's (`ap_main2`). arm64 needs no enable:
PXN/UXN are always honored.

## TLB shootdown, and the deadlock it can cause

Every mapping change invalidates only the issuing CPU's TLB.
`CpuTable::invalidate_tlb_all/Address/Range` fan the invalidation out to the
others as an IPI task and wait for every one to acknowledge — except on
arm64, where `TLBI` broadcasts in hardware and `hal::tlb_shootdown_needs_ipi()`
answers false.

The local flush and the mask of the CPUs still to ask are made on one CPU,
preemption off from the first to the second (`CpuTable::flush_local_tlb`):
the mask leaves out the CPU it is computed on, and that has to be the CPU
just flushed. It was not always: a task moved between the two left out the
CPU it moved to, whose TLB nobody flushed, and asked the one already
flushed. On x86 under load that CPU kept a translation of a freed heap
block, and once the block's VA was handed out again with a frame of its own
it read and wrote the old frame through it -- a page of somebody else's.
It showed as one 1 MiB `ipc ping` in some thousands coming back with a
4 KiB piece of another ping's data, between two hosts under KVM, and as
`take_kept`'s mark check firing within seconds once a yield was put
between the flush and the mask. `get_remote_cpu_mask` now refuses a caller
that could be moved (`BUG_ON(preempt_can_move())`), a panic at its first
call rather than at the move that one call in thousands meets. arm64 needs
no mask, but the same move can leave a `TLBI` incomplete -- it is complete
once the CPU that issued it passes a `DSB`, and a task interrupted between
the two runs its `DSB` on the CPU it is moved to -- so `switch_context`
starts with a `dsb ish`, as Linux's `__switch_to` does.

This is the origin of the single most important rule in the codebase:
**never allocate or free with a spinlock held.** `mm::free`/`mm::alloc` can
reach the page allocator, which shoots down every other CPU and waits for
all of them to answer. A CPU spinning on the lock you hold has interrupts
off and can never answer, so the two wait for each other forever and the
machine stops dead with no panic and nothing to reset it. Collect what needs
releasing into a local list under the lock, unlock, then release. Better
still, do not allocate on a datapath at all — network frames come from a
pool built once at boot for exactly this reason.

`send_tlb_ipi` gives up after ten seconds and panics naming the CPUs that never
answered, so a new instance of this bug is a report rather than a dead
machine.

## Looking at it from the shell

- `meminfo` — the firmware memory map, and how much of it the kernel
  actually uses (RAM reported vs. RAM reachable).
- `memusage` — free and total page counts and how many of the free are
  zeroed, and the huge pool's free and total blocks and how many of the free
  are known zero; the fewest pages free since boot; and what Rust's heap
  takes now and took at most ([What Rust holds of it](#what-rust-holds-of-it)).
- `meminfo` and the huge pool: with `hugepages=`, a line for the pool --
  2 MiB blocks reserved at boot right after the free list is built, kept for
  guests' memory and out of every other allocation ([The
  hypervisor](hypervisor.md#how-much-of-it-and-where)).
- `heapbench` (a module, `insmod /heapbench.ko`; [Modules](modules.md)) —
  what an allocation and a free cost, size by size, on one CPU and on all.
- `memcheck` — `check_free_list` walks every descriptor and reports any page
  that is on a free list but must not be: inside a reserved region, inside
  the kernel image, or outside usable RAM. That invariant is what the whole
  carve-out scheme rests on, and its violation is silent until something
  overwrites `page_array_` or an ACPI table.
