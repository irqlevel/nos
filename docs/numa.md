# NUMA

A machine of two or more sockets is not one memory: each socket has its own
controllers and its own DIMMs, and a CPU reaches the other socket's through
the link between them, slower and narrower. The firmware says which CPUs and
which memory are near each other -- a NUMA node -- and how far the rest is,
and the kernel keeps a task's memory, the pages it zeroes and a guest's RAM
on the node of the CPUs that use them.

What it is worth, on Newton, a Supermicro X10DRi of two Xeon E5-2670 v3
and 256 GiB ([Real hardware](real-hardware.md#newton-a-supermicro-x10dri-of-two-sockets)),
measured by `numabench` ([Loadable modules](modules.md#numabench)) --
each node's gigabyte, past either socket's 30 MiB of last-level cache, as
each node's CPUs reach it:

    numabench: 1024 MiB a node, 8000000 loads a pair, 2 nodes
      CPUs of    memory of       load ns    read GB/s    its pages
      node 0     node 0            137.6         8.16         100%
      node 0     node 1            193.2         6.37         100%
      node 1     node 0            193.3         6.81         100%
      node 1     node 1            137.9         8.67         100%

A line of the other socket's memory takes 55 ns more -- 193 against 138,
the TLB's misses in both -- and one CPU reads it at four fifths of the speed;
256 MiB a node says the same (130 and 182 ns). Before the kernel knew of
nodes every free page was on one list, in the order of their addresses,
node 0's first: a task on the second socket was given the first socket's
memory until it ran out, and paid the second and third rows for all of it.
The last column is the allocator checked: every page of each node's memory
its own node's, by its physical address.

## Where the topology comes from

`kernel/numa.{h,cpp}`, `kernel::Numa`. On x86-64 the ACPI parse
(`drivers/acpi.cpp`) reads two tables after the MADT:

- the **SRAT** (ACPI 6.5, 5.2.16): each CPU's proximity domain, by its
  local APIC id (16-byte entries; the domain's high three bytes from the
  table's revision 2 on) or its x2APIC id (24), and each range of memory's
  (40), enabled by bit 0 of their flags;
- the **SLIT** (5.2.17): a byte a pair of domains, the distance from one to
  the other -- 10 a domain's to itself.

`main2` seals what was said right after the parse, before the heap and
before a second CPU runs (`Numa::seal`); from then on it is read without a
lock, and nothing changes it. The domains are the nodes, in their order.
The machine is **one node** -- what the kernel was before it knew of nodes,
so that a wrong table costs speed and never correctness -- when the tables
name one domain or none, and when they do not hold together: a structure cut
short or of no length, an entry shorter than its fields, a CPU in two
domains, two domains' memory overlapping, more than `MAX_NODES` (16) domains
or `MAX_RANGES` (64) ranges. A SLIT whose diagonal is not 10, or with a
distance under 10, is set aside for 10 and 20. A CPU the SRAT does not place
is node 0's, and so is memory no range of it covers. arm64 boots from a
device tree, which says nothing of nodes here: one node.

`numa` in the shell says what was made of it -- Newton's, booted with
`hugepages=16G`, half a minute after `boot: complete`, its free pages still
being zeroed:

    node 0 (domain 0): CPUs 0-11,16-27 (24)
      memory 0x0-0x80000000, 2048 MiB
      memory 0x100000000-0x2080000000, 129024 MiB
      free 120731 MiB, 8068 MiB of it zeroed; huge pool 8122 MiB, 8122 MiB of it free
    node 1 (domain 1): CPUs 32-43,48-59 (24)
      memory 0x2080000000-0x4080000000, 131072 MiB
      free 122808 MiB, 24259 MiB of it zeroed; huge pool 8262 MiB, 8262 MiB of it free
    distances:
            0    1
       0   10   21
       1   21   10

A CPU's number is its APIC id, and Haswell-EP numbers a socket's twelve
cores 0-5 and 8-13: hence the gaps. The boot says the same in its log
(`numa: node 0 (domain 0): CPUs 0-11,16-27 (24), 131072 MiB the firmware
placed in 2 ranges`), and `mm: node N: ... MiB free` once the pages are
spread.

## The page allocator

`mm/page_table.{h,cpp}`. Every free page is on its own node's two lists --
the pages as their last owner left them, and those known zero -- and nobody
else's (`PageTable::NodeFree`); the machine's counts are their sums.

- **Allocation** takes from the node of the CPU it runs on
  (`Numa::get_current_node`), then from each other node in the order of
  its distances, nearest first: it fails only where every node has none.
  `alloc_page_keeping_on` and `alloc_huge_frame_on` take a node -- a
  guest's memory, from the node its CPUs are placed on.
- **The free list is built before the tables are read**, so every page is
  node 0's until `seal`; `spread_free_pages` then moves each node's pages to
  its lists, a run at a time (`ListEntry::move_run_tail`) -- the list is in
  the order of the pages' addresses, and a node's pages are a few long runs.
- **The zeroing** (`PageZeroer`, [Paging](paging.md)) is a task a node, two
  on a machine of one, each on its node's CPUs zeroing its node's pages:
  their stores stay off the link between the sockets. The huge pool's
  blocks are zeroed by any of them, whatever node a block is on, the first
  task taking them first.
- **The huge pool** (`hugepages=`) is reserved after the spread, no longer
  with the free list: each node gives its share of the blocks -- as much as
  it has of the free pages (Newton's 16 GiB: 8122 MiB and 8262) -- and keeps
  its share of the pages kept free, its highest blocks first. A
  block is the node's its first page is; one no range covers is found past
  them all.
- `memusage` and `meminfo` end with a `node N:` line a node -- its free
  and zeroed pages, its huge pool, and in `meminfo` the RAM the firmware's
  map gives it -- and `memcheck` says each node's free and zeroed pages and
  checks that they add up to the machine's. On a machine of one node none
  of the three says anything of nodes.

What is not by node yet: `page_array_`, the descriptors, all on node 0; the
heap's shared lists of blocks kept for reuse (a few hundred KiB a size), and
a block a task freed after it moved; the IO-APIC's and MSI-X's interrupts,
which go where IRQ balancing puts them; the huge pool's zeroing; the page
tables, each taken where its mapping was made.

## The scheduler

`Task::select_next_task_queue` (`kernel/task.cpp`, [Scheduler](scheduler.md)):
a task moves to a lighter queue of its own node -- its queue's CPU's -- for a
difference of `MIGRATE_THRESHOLD` (2), and to another node's for one more
(`NUMA_MIGRATE_EXTRA`): there it leaves its memory behind. A task placed for
the first time goes to its creator's node's lightest queue unless another
node has a lighter one. On a machine of one node every queue is the node's,
and this is the rule it always was.

## Guests

`hv` ([The hypervisor](hypervisor.md)) places a guest's vCPUs as before --
whole cores, sharing the first's last-level cache where they can, which on
a part without cluster-on-die is its socket -- and takes its memory from the
first vCPU's node: `Spec::placed` sets `node`, `LinuxGuest::new_on` hands it
to `GuestMemory::place_on`, and `Run::new_on` / `Run::new_huge_on`
(`kcore::frame`) take every frame or block of the pool from it, the nearest
node's when it has none. `hv start`'s line ends with which node, and how
much of the memory was found there by the pages' physical addresses
(`GuestMemory::pages_on`): `..., its memory node 1's (100%)`. A machine of
one node builds guests as it did, and its line says nothing of nodes.

## From Rust

`kcore::numa`: `node_count`, `cpu_node`, `node_cpus`, `distance`,
`phys_node`, `node_free_pages`; `kcore::frame::{Frame, HugeFrame}::new_on`
and `Run::{new_on, new_huge_on}`. Values only across the C ABI
(`ffi::numa`, `kernel_frame_alloc_on`, `kernel_frame_alloc_huge_on`): a CPU
or a node past the machine's is answered for, and a node to allocate on that
is not there is the calling CPU's.

## Tests

- every boot: `test_numa` -- the nodes' counts add up, a page the kernel takes
  is its CPU's node's while that node has one free, a page taken for a node
  is that node's, and each goes back to its own lists;
- `SMOKE_NUMA=1 tests/smoke-test.sh` (in CI): QEMU's two nodes, 2 CPUs and
  512 MiB each, 21 apart -- the tables read, the pages spread, `test_numa`;
- `cpp-fuzz.py --target acpi`: SRATs and SLITs, random, damaged and the
  X10DRi's, against a judge of the target's own for what `Numa` makes of
  them; `--target pagetable`: machines of two or three domains -- in no
  order, far apart, one of CPUs alone, a range of none, a SLIT that holds or
  does not -- every free page on its own node's lists, an allocation the
  nearest node's head, the pool each node's top first and
  `alloc_huge_frame` the nearest node's first choice;
- on the metal: Newton, the X10DRi -- `numa`, `memcheck` and `numabench`
  ([Real hardware](real-hardware.md#newton-a-supermicro-x10dri-of-two-sockets)).
