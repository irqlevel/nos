# More than 64 CPUs, and x2APIC

**Planned, not started** (2026-11-19). Met while bringing nos up on Newton,
a two-socket Supermicro X10DRi ([NUMA](../docs/numa.md),
[Real hardware](../docs/real-hardware.md#newton-a-supermicro-x10dri-of-two-sockets)):
its 48 threads fit under the limit -- their APIC ids end at 59 -- and the
next size of two-socket server does not.

## Where it stands

- **`MAX_CPUS` is 64** (`kernel/cpu.h`), "a ceiling rather than a tuning
  knob": a set of CPUs is a `ulong` everywhere it travels -- the running
  CPUs, a task's affinity, the soft IRQs' raise mask, a TLB shootdown's
  targets, IRQ balancing, a NUMA node's CPUs, the shell's `sched affinity`
  -- and in Rust a `u64`/`usize` across the C ABI (`kernel_cpu_online_mask`,
  `kernel_numa_node_cpus`, `kernel_softirq_raise_mask`, a task's affinity at
  its spawn) into `kcore`, hv's placement and the modules that pin tasks.
- **A CPU's index is its APIC id.** The MADT's local APIC entries (type 0)
  are the only CPUs read: one whose id is 64 or more is dropped, said in
  the log (`Acpi: MADT lapic apic_id N ignored (max 64)`), and the machine
  boots without it. The GS slot (`arch/x86_64/percpu.h`) holds the id.
- **xAPIC only.** A firmware that hands over in x2APIC mode is switched back
  (`Lapic::enable`, through the disabled state as the SDM says), which works
  while every id is under 255. The MADT's local x2APIC entries (type 9) are
  not read; the SRAT's (type 2) are.
- The IO-APIC's physical destination is four bits: line interrupts reach
  APIC ids 0-15 alone (`IoApic::MAX_PHYSICAL_DEST`). MSI's is eight.

What that leaves out: a CPU's APIC id is its package's number above its
core's and thread's bits, so a socket of more than 16 cores starts its
package at id 64 -- **a two-socket Intel server of 17 cores a socket or
more loses its whole second socket**. A part of more than 32 cores loses
the cores past them. APIC ids of 255 and up -- a two-socket EPYC of 64
cores, four sockets -- need x2APIC, and a firmware that turned it on for
such ids is turned back today into a machine that does not work.

## Part 1: CPU sets past a word, `MAX_CPUS` 256

The wide part, and mechanical. A survey of the tree on 2026-11-19:
`MAX_CPUS` in 214 places in 34 C++ files and 64 in 16 Rust files (most of
them array bounds); a CPU set as a word in about 145 places in 20 C++ files
and 70 in 23 Rust files.

- **A `CpuSet` type**, a fixed bitmap of `MAX_CPUS` bits, in C++ (a
  `constexpr` constructor and a trivial destructor, as a static's type must
  have) and in `kcore` (`kcore::cpu::CpuSet`): set, clear, test, any, count,
  first, the next after, and, or, a walk. Every `1UL << cpu` and every
  `ulong` mask becomes one; `select_next_task_queue`'s loop, the TLB
  shootdown's `send_tlb_ipi`, `SoftIrq`, IRQ balancing, `Numa`, the shell's
  parse and print of a list of CPUs (`8,9,24-25` stays, the hex form grows).
- **The C ABI** passes a set by pointer to its words, a `#[repr(C)]` struct
  `check-ffi.py` holds to the C++ one field by field; the ffi digest
  changes, so every module is rebuilt with the kernel, as always. In Rust:
  `kcore::cpu`, `kcore::numa`, `kcore::task`'s spawn on CPUs, hv's
  `place.rs`/`cores.rs` (`load: [u32; MAX_CPUS]`), hvarch's per-CPU statics
  (`msrs.rs`, `fp.rs`), and the modules that pin their tasks (netload,
  blkload, heapbench, numabench, nbs's).
- **Per-CPU static storage** grows with the bound: a CPU's boot stack
  (32 KiB, `kernel/main.cpp`), its double-fault stack (16 KiB, `gdt.h`) and
  its park stack (16 KiB, `cpu.cpp`) are 4 MiB of BSS at 64 and would be
  16 MiB at 256; arm64's AP boot stacks the same. Better: allocate them at
  boot for the CPUs the MADT names -- the heap is up before the first AP
  starts -- and keep static only what the BSP needs before it.
- Under xAPIC an APIC id is eight bits, 255 the broadcast: with
  `MAX_CPUS` 256 the index can stay the APIC id, and nothing that programs
  an interrupt's destination changes.

Gates: a smoke mode of many CPUs whose second package starts past id 64
(`-smp 96,sockets=2,cores=24,threads=2`; TCG boots it slowly, KVM where
there is one), beside the plain smoke boots, both arches; `cpp-fuzz.py`
(its stand-in `CpuTable`, `eevdf`, `pagetable`), `hv-fuzz.py`'s `place`
target with more than 64 host CPUs, `check-ffi`, `hv-test.py`; on the
metal, Newton and the others as they are, for what must not change.

## Part 2: x2APIC

Needed only for APIC ids past 254, or a firmware that leaves x2APIC on
with them; smaller than part 1, and riskier -- interrupt routing and the
APs' start.

- **The mode.** CPUID.1:ECX[21]; `IA32_APIC_BASE.EXTD` set on the BSP and on
  each AP before it touches its local APIC. Every access already goes
  through `Lapic::read_reg`/`write_reg` with the register's offset over 16,
  which is the x2APIC MSR's number less 0x800: the two become `rdmsr`/
  `wrmsr` there. The ICR is one 64-bit write (MSR 0x830, the destination in
  bits 63:32) with no delivery-status wait; the DFR is not written (it
  faults), the LDR is read-only (cluster mode); the ID is MSR 0x802, 32
  bits. A parameter, `x2apic=auto|on|off`: auto keeps xAPIC unless an id is
  past 254 or the firmware left x2APIC on, and honours the DMAR's x2APIC
  opt-out otherwise.
- **The MADT's local x2APIC entries** (type 9: the id, the flags, the ACPI
  processor UID) and their NMI entries (type 10); a CPU named by both kinds
  is one CPU.
- **A CPU's index apart from its APIC id.** Ids are 32 bits and sparse: the
  table keeps both, the GS slot the index (percpu.h's comment, which rules
  x2APIC out, changes), and every site that hands the hardware an id takes
  it from the table -- about 150 places in 29 files: the IPIs and the APs'
  INIT/SIPI (`lapic.cpp`, `cpu_start.cpp`), the IO-APIC's redirection
  entries, MSI and MSI-X addresses, VT-d's IRTE destinations (`vtd.cpp`),
  the PMU's NMI, IRQ balancing, and hv's posted-interrupt descriptor, whose
  NDST holds an xAPIC id in bits 15:8 and an x2APIC one in all 32.
- **Device interrupts to a CPU past id 255**: MSI's eight bits and the
  IO-APIC's four do not reach it. Only VT-d's interrupt remapping in x2APIC
  mode does (IRTA's EIME, the IRTE's 32-bit destination): nos remaps with
  `iommu=on` (`arch/x86_64/vtd.cpp`), in xAPIC's format. Without it, IRQ
  balancing keeps a device's interrupts on the CPUs it can address, and
  says so once.

Gates: QEMU's x2APIC (TCG's from 8.1) under `x2apic=on` on the smoke boots;
ids past 254 under `-machine q35,kernel-irqchip=split -device
intel-iommu,intremap=on,eim=on` with `iommu=on`, `iommu-test.py` under it;
`hv-test.py` (posted interrupts: nested VT-x, and the EX44's silicon); on the
metal, Newton under `x2apic=on` -- its Haswell-EPs have the mode, and its
firmware opts out, so it tests the mode, not ids past 254. No machine of
ours has those.

## Order and size

Part 1 first: it is the limit a common two-socket server meets. Part 2
when a machine with APIC ids past 254 is to run nos. Together about one and
a half to two times the NUMA work of 2026-11-19 (topology, allocator by
node, the scheduler's preference, hv's placement, numabench, the gates):
part 1 the larger in lines, part 2 the larger in risk.
