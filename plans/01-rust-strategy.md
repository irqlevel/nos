# Stage 1 — Rust strategy: new code in Rust, unsafe confined

**Goal:** All substantial new code — the hypervisor above all — is written in
Rust, with `unsafe` confined to a small, auditable arch crate. This is a
cross-cutting discipline applied to every stage after it, not a milestone with
its own demo.

**Why:** The worst bug class in a hypervisor is guest-controlled data corrupting
*host* memory. Rust designs that class out — *if* the boundary is drawn so the
compiler enforces it, rather than left to programmer discipline.

---

## The existing layering already proves this works

`nos` Rust is layered `ffi → kcore → drivers`, and the numbers show the pattern
holds:

- `kcore` concentrates the unsafe: ~130 `unsafe` mentions over ~1800 lines.
- Drivers on top are nearly clean: **NVMe ~4%**, **r8168 ~2%** of lines mention
  unsafe.

The hypervisor should replicate exactly this ratio.

## Crate layout for the hypervisor

Two new crates, with a compiler-enforced boundary:

- **`hv-arch`** — *all* hypervisor unsafe. VMX/SVM instruction wrappers, the
  naked vmexit entry/exit stub, CR/MSR access, raw physical-page access for
  VMXON/VMCS/EPT structures.
  - `#![deny(unsafe_op_in_unsafe_fn)]`
  - Every `unsafe` block carries a `// SAFETY:` invariant comment.
- **`hv`** — *all* hypervisor logic. VMCS/EPT typed builders, the vmexit
  dispatcher, exit-qualification decoding, device emulation (UART, LAPIC),
  `bzImage`/`boot_params` loader, VM lifecycle, and the control-plane glue.
  - `#![forbid(unsafe_code)]` at the crate root.

`#![forbid(unsafe_code)]` turns "please keep unsafe minimal" into a **compile
error**: any unsafe that leaks into the logic layer fails to build.

## The irreducible unsafe core is small (~300–500 lines)

There is a layer that can never be safe, but it is compact:

- VMX/SVM instructions (`vmxon`, `vmclear`, `vmptrld`, `vmread`/`vmwrite`,
  `vmlaunch`/`vmresume`, `invept`; SVM's `vmrun`/`clgi`/`stgi`) via
  `core::arch::asm!`. **No C++ FFI is needed for this** — Rust emits the inline
  asm directly.
- The vmexit entry/exit stub that saves/restores guest GPRs. rustc 1.91 supports
  this on stable: **naked functions (`#[unsafe(naked)]` + `naked_asm!`) have been
  stable since 1.88**, so a separate `.asm` file is not even required.
- CR/MSR access and raw EPT page manipulation.

Everything else — typed VMCS field access, the EPT builder, the vmexit
dispatcher, exit-qualification parsing, device emulation state machines, the
loader, VM lifecycle, and the HTTP API — is ordinary safe Rust. Crucially, **the
parsing of guest-controlled data (the main attack surface) lives entirely in the
safe layer.**

## The one place "safe" can become a lie: guest memory

The guest mutates its own RAM concurrently with the host. Holding a Rust
`&`/`&mut` into guest memory for even an instant is instant UB, and it voids
every guarantee of the safe layer.

**Mandatory abstraction:** a `GuestMemory` type with volatile, copying accessors
(`read_obj::<T>(gpa)` / `write_obj(gpa, &T)`), modeled on rust-vmm's
`vm-memory`. No long-lived references into guest RAM, ever. This is the single
most important design decision in the hypervisor: get it right on day one and
"minimum unsafe" genuinely means "minimum UB," not just cosmetics.

## What to reuse from `kcore`

`kcore` already exposes most of what the VM layer needs, via the existing
three-step FFI process (`rust_ffi.cpp` → `ffi/` → `kcore/`):

- `task.rs` — a vCPU is a kernel task running the resume/vmexit loop.
- `interrupt.rs`, `dma.rs` (physical pages for VMXON/VMCS/EPT), `pci.rs`,
  `time.rs`/`timer.rs`, `sync.rs`, `trace.rs`.

Likely small additions to expose: `VirtToPhys` for arbitrary VAs, and a per-CPU
context handle.

## What to study / vendor from

- **rust-vmm** (`vm-memory`, `linux-loader`) — reference implementations of
  `GuestMemory` and `bzImage`/`boot_params` loading. The crates themselves may
  not drop into a `no_std` kernel as-is, but the structures and approach port
  directly.
- **Firecracker / Cloud Hypervisor** — device-emulation structure and minimal
  VMM shape.
- **barevisor** and Satoshi Tanda's **"Hypervisor 101 in Rust"** — the closest
  precedents: bare-metal VMX/SVM hypervisors in Rust proving ~95% safe is
  achievable in exactly this setting.

## Honest limitation

Rust guarantees **host** memory safety, not virtualization *correctness*. A
mis-filled VMCS, a wrong EPT permission, or a bug in LAPIC emulation is still on
you and debugs as painfully as in C. The win is narrow but real: the worst bug
class disappears by construction, and the rest at least reproduces
deterministically.
