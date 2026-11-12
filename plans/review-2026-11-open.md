# The 2026-11 review: what is left

The review of 2026-11-12 read the Rust ffi, the base crates (`kcore`, `kmod`,
`events`, `random`), the hypervisor (`hvarch`, `hv`, `hvboot`) and the `hv`
module at `e5b8fb80`: 4 high findings, some 36 medium and 137 low. All but
what is below were fixed and are on master (`5c28a3bf`, `3850ee73`,
`7d24a6d5`); the reports themselves are not in the tree. What each item
here is, where, why it waits, and what would gate it.

## Findings not fixed

### A12 -- two RDMSRs on every AMD-V entry

`hvarch/src/x86/svm.rs:904`: before every `vmrun` the entry checks
`!enabled() || host_area() != expected` -- a read of EFER (`enabled`, `:218`)
and of VM_HSAVE_PA (`host_area`, `:226`). Both are written only by
`enable`/`disable`, on that CPU, with interrupts off, so a per-CPU word that
those two set would answer the same; under a nested hypervisor each read of
VM_HSAVE_PA is an exit of its own. Waits for a measurement on AMD silicon:
the cost of the two reads against an entry's (`hv bench` on the AX41's Zen 2,
before and after), which needs nos booted there rather than Ubuntu and its
VMs -- the EX44 is Intel and cannot say. Under KVM the reads are exits to L0
and say nothing of the silicon.

### L9 (b) -- a reboot gives its guest's memory back before taking it again

`modules/hypervisor/hv/src/vms.rs:874` (`reboot`): the guest is dropped --
its memory back to the huge pool -- and then `guest::build` takes as much
again (`:886`). A start that lands between the two, even one that would be
refused a moment later, can take that memory, and the rebooting VM is left
stopped ("restart failed -- no guest"): on a pool sized for its guests, a
running VM lost to another's start. Part (a) of L9, the disks opened and
claimed before the memory is taken, is fixed. The fix: keep the rebooting
VM's memory across its reboot, zeroed again, or hold its share of the pool
(a reservation the pool honours) from the drop to the build. Gate: a test
that starts a second guest on a pool with room for exactly one while the
first reboots in a loop -- `hv-test.py`'s restart guests, with `hugepages=`
sized to fit.

### T10 -- `hv forward`: no half-close, and an idle relay polls

`modules/hypervisor/hv/src/forward.rs` (`relay_task`, `:243`) over
`relay.rs`: a relay ends when either side's stream ends, closing both, so a
client that shuts its sending side and then waits for the answer (`nc -N`,
a shell pipeline, some health checks) gets nothing; and an idle relay looks
at each side every `RELAY_WAIT_MS` (5 ms, `:89`) -- 200 waits a second a
connection for as long as it lives -- rather than sleeping until either has
something. The connect to the guest moved onto the relay's task (the rest of
T10). Both halves want the TCP layer first: a shutdown of one direction (a
FIN with the stream still readable) and a wait on two connections' events at
once (`kcore::tcp::TcpStream::waker` and `events::SharedEvent` are most of
the way there). Gate: `hv-fuzz.py`'s `relay` target taught half-close, then
`hv-linux-test.py --net` with an `nc -N` through a forward.

### T13 (the rest) -- a forward of a VM with no folder, through the API

`modules/hypervisor/hv/src/provider.rs`: a VM started from a kernel path
rather than a folder has `name: null` in the `vms` resource and `vm: null` in
its forwards, and `forward_del` -- like every action of the resource -- takes
a folder's name: such a forward cannot be taken away through the API (`hv
forward del` in the shell can). The provider would need to name such VMs by
number too, and the descriptor is near its limit: `vm.descriptor.json` is
16,213 bytes of the 16,384 the bus takes (`web-test.py` catches an
overflow). The rest of T13 is fixed.

### P6 (4) -- the host's CPUID and topology asked again at every start

`hypervisor/hv/src/policy.rs`: every guest CPUID answer asks the host's
several times (the range check and the leaf in `cpu::cpuid_count`, more in
`structured_last`, `topoext`, `vendor`, up to a dozen in `last_level` for
leaves 4 and 0x8000_001D), and `topology::host()` (`topology.rs:57`, an IPI
to every CPU on a hybrid part) runs at every `Cores::take`. None of it changes
after `hv on`, but a cache would need an entry per core type on a hybrid CPU
and a rule for microcode loaded late, and the cost is a guest's start alone:
measure a start under nested KVM, where each host CPUID is an exit, before
building one. Parts 1-3 of P6 (one bzImage crate, `common/boot/bzimage`; the
fields nothing read; SMBIOS 2.1's 32-bit entry point) are done.

## Side notes of the reports, still open

What the reports noted outside their own slices was mostly another slice's
finding, since fixed. These were not, as far as re-read against master:

- `hypervisor/hv/src/policy.rs:1149` over `hvarch/src/x86/cpu.rs:115`
  (`microcode_revision`): every guest read or write of MSR 0x8B (the
  microcode revision) runs the host's own sequence -- a `wrmsr` of
  IA32_BIOS_SIGN_ID, a CPUID, an `rdmsr` -- at the guest's pace. Harmless as
  read (the vCPU's task is bound to its CPU), but a host MSR written when a
  guest asks; the revision could be read once at `hv on`.
- `base/kcore/src/cmd.rs:213`: a command line that is not UTF-8 reaches its
  handler as the empty string, silently. It should be refused, said.
- `hypervisor/hv/src/svm.rs:650` (`skip`) and the fast path's step in
  `hvarch/src/x86/svm.rs`: stepping past an emulated instruction moves RIP and
  clears the interrupt shadow but leaves RFLAGS.RF as it was, where a CPU
  clears it when an instruction completes. Only a guest's debugger sees it.

## Gates that fail on every tree

- `test_sleep_blocks` (`src/base/kernel/test.cpp`): two tasks that sleep on
  one CPU must run under 10% of what they slept. Under heavy host load -- a
  Mac running several TCG QEMUs and Docker at once, load average over 30 --
  their runtime takes in the emulator's own preemption (a vCPU thread taken
  off mid-run), and the boot fails "Multitasking test failed" (two sleepers
  ran 40-60 ms of 200). Alone, it passes; nothing in the scheduler changed.
  The test could hold its sleepers to their wake-ups rather than to their
  runtime, or say what the host's load was.
- `hv-linux-test.py --ioapic`, "the timer's interrupts came through the
  IO-APIC's pin 2": a Linux 6.18 offered the TSC-deadline timer and ARAT never
  starts the PIT, so IRQ 0 has no handler and no line in /proc/interrupts.
  The rest of the IO-APIC (the serial port's pin 4, a level SCI, every IPI and
  INIT) goes through. The check should hold a guest to pin 2 only where its
  kernel uses the PIT for its timer.

## hvboot against GRUB

hvboot chooses the entry GRUB 2.06 would, by grub.cfg read as GRUB reads it
(checked by booting GRUB on 14 menus), but:

- it does not run Ubuntu's `initrdfail` function -- the `save_env` that,
  after an entry booted without an initrd failed, makes the next boot take
  the initrd. Current Ubuntu cloud images (24.04, 22.04) boot their default
  entry with its initrd and never reach that branch; an image made with
  `GRUB_FORCE_PARTUUID` would boot without one, and on failure loop rather
  than fall back;
- GRUB reads ext4 with `meta_bg` and `encrypt`, which hvboot refuses (by
  name), and refuses `inline_data`, which hvboot reads.

## Elsewhere

- nos's own TCP over a WAN: `wget` of 730 MB from cloud-images.ubuntu.com on
  the EX44 runs at 9.2 MiB/s, where a guest's Linux through nos's NAT gets 76
  MB/s on the same link: the level nos's sender reaches over a long path
  today, loss recovery (SACK, RACK-TLP, PRR) and all. Where the rest goes --
  the window, pacing, the path -- is not measured here.
- CLAUDE.md lists the `common/` crates by area and has a gates table; neither
  names `common/boot/bzimage` yet -- a change there is gated by `hv-fuzz.py`'s
  `linux` target and `hvboot-test.py`.
- `scp` and `sftp` to nos: today `modules/net/sshd` refuses the `sftp`
  subsystem (modern `scp` speaks SFTP). The shape it would take: SFTP v3 in
  the sshd module over `kcore::fs` (open, read and write at an offset, stat,
  list, mkdir, remove, rename; attributes nos does not keep answered with
  defaults), its codec a `common/net/sftp` crate with a `net-fuzz.py` target
  of its own, and sshd on a bulk listener -- the 8 KiB windows of a plain one
  make about 130 KB/s at the 62 ms from a Mac to the EX44.

Related: [`hypervisor.md`](../docs/hypervisor.md) has E8's measurement on the
EX44 (a `KVM_GUEST` kernel turns x2APIC on by itself, so the xAPIC start costs
it nothing). The VT-x fixes were run on the EX44's silicon too: `hv run all`
27 of 27 guests on a P-core and an E-core, `kgs` and `cache` among them.
