# Build

The build is parameterized by `ARCH` (default `x86_64`, or `aarch64`); objects go to `out/$(ARCH)/`.

## Native

Requires clang, nasm, ld, grub-mkrescue with `xorriso` + `mtools`, and a **nightly**
rustup toolchain with the `rust-src` component — the Rust staticlib uses `-Z build-std`
to rebuild `core`/`alloc` with `-Ccode-model=large`, and `#![feature(alloc_error_handler)]`
for its out-of-memory hook; `src/rust/rust-toolchain.toml` pins the exact nightly:

```sh
make
```

`make` runs `cppcheck` static analysis first (`make check`) and fails on any
finding; `make nocheck` skips it. `make smoke` builds in Docker and runs the
headless boot smoke test (`scripts/smoke-test.sh`).

## Four things the Makefile will not tell you

**Source lists are explicit, not globbed.** The Makefile has two
hand-written lists, `CXX_SRC_x86_64` and `CXX_SRC_aarch64` (plus `ASM_SRC_*`
for NASM and `ASM_S_SRC_*` for GNU-as). A new `.cpp` that is in neither is
silently not compiled, and portable code has to be added to **both**. Common
driver code that names an x86-only entry point gets an unreachable link stub
in `src/cpp/arch/arm64/x86_driver_stubs.cpp` rather than an `#ifdef`.

**Dependency files go stale.** After moving, renaming or deleting a header,
run a clean build, or delete the `.d` files in `out/` that still name the
old path. Make reports one as `No rule to make target`, not as an error in
any source file -- which is one more reason to gate on the exit code of a
build and not on what it printed.

**The link is two-pass.** Stack traces resolve symbols from a table baked
into the kernel: the build links `out/$(ARCH)/pass1.elf`, runs `nm` over it
to generate `out/$(ARCH)/symtab_data.cpp`, and then links the final ELF (the
`symtab_data` rules in the `Makefile`). The table of functions a loadable
module may call is made from the same first pass
([Loadable modules](modules.md#what-a-module-may-call)). Pass 1 links
against empty weak stand-ins for both tables, in `kernel/pass1_tables.cpp`,
a file that indexes neither. Anything that touches the link or symbol
resolution has to keep both passes working.

**The link refuses a static constructor.** This kernel runs no
`.init_array`: a global whose type has a non-`constexpr` constructor or a
non-trivial destructor would be left as zeroes, and was -- `Pci`'s config lock
and the arm64 console's were never registered with the watchdog their
constructors would have registered them with. Both linker scripts gather
`.init_array`, `.ctors` and their destructor counterparts into a section
nothing loads and fail the build if it is not empty, with `a static
constructor or destructor: this kernel runs none`. The way out is a
`constexpr` constructor and a trivial destructor, every member initialised,
or a function-local static, constructed on first use.

## No network during a build

The Rust dependencies (rustls and what it brings, see [HTTPS](tls.md)) are
vendored in `src/rust/vendor` and cargo is run with `--offline`, so a build
never reaches crates.io — a machine with no internet, or a CI runner with none,
builds the same as any other.

After adding, removing or bumping a dependency in `src/rust`, refresh that
directory and commit it together with `Cargo.lock`:

```sh
scripts/vendor.sh      # this step, and only this step, needs the network
```

A build that suddenly wants the network is a dependency that was never
vendored; `--offline` makes it say so instead of quietly fetching.

The dated toolchain pin is what holds this together. `-Z build-std` resolves
the compiler's own `library/` workspace alongside ours, so the vendored set
covers *std's* dependencies as well — and those move from one nightly to the
next. On a floating `nightly` the offline build eventually fails in resolution,
naming a crate the kernel does not compile and never will: `hermit-abi`, which
only std wants and only on `cfg(target_os = "hermit")`, going 0.5.2 → 0.5.3 is
enough to stop the build. `src/rust/rust-toolchain.toml` is where the version
lives; the Dockerfile and both CI workflows install whatever it names. Bumping
it and re-running `scripts/vendor.sh` is one commit, not two.

## Docker

Works on macOS / Apple Silicon, and packages the whole toolchain:

```sh
./scripts/build-iso-docker.sh
```

This produces `nos.iso` and `bin/kernel64.elf` (for GDB symbols).

## arm64

Requires `ld.lld`, `llvm-nm` and the same nightly Rust toolchain; on macOS build in Docker:

```sh
make nocheck ARCH=aarch64
```

This produces `kernel-arm64.elf` and `nos-arm64.img` (Linux `Image` format, bootable with QEMU `-kernel`).

## UBSan

`make nocheck UBSAN=1` (either `ARCH`) builds the kernel's C++ with clang's
undefined-behaviour sanitizer: a check in front of every operation the
language leaves undefined -- a shift past the width, signed overflow, an index
past a bound the compiler knows, a null or misaligned access, a `bool` or an
`enum` holding no valid value, falling off the end of a function -- which calls
the handlers in `kernel/ubsan.cpp` when it fails. The Rust and the assembly
are not instrumented. The images keep their names, and the build is a flavour
of its own: its objects are in `out/$(ARCH)-ubsan/`, and a switch of flavour
relinks the final images (a stamp in `out/` records which they are), so a
plain build never leaves an instrumented kernel behind, or the other way
round. `version` says `+ubsan`.

The first report is a panic, with the site:

    PANIC:Report():ubsan.cpp,267: UBSAN: shift exponent 64 is too large for 64-bit type 'unsigned long' at src/cpp/...

which makes every gate a UB check over the code it drives: build with
`UBSAN=1`, and run the gate on the images as they are (`--skip-build` where it
builds). CI boots it on both architectures, after everything else. To collect
every report of a boot instead, boot with `ubsan=warn`: each site reports
once, with a backtrace, and the kernel goes on. A report goes out through the
writers the panic path uses -- the serial port, polled, and the disk log --
and not through the kernel log or the netconsole, which take locks: a check
can fail inside an NMI, or on a CPU that holds one of them.

What its first boots found, all in code that had passed every gate:

- `CONTAINING_RECORD` and `OFFSET_OF` were `&((type*)0)->field`, a member
  access through a null pointer. They are `__builtin_offsetof` now.
- On x86 `Hal::RunOnStack` entered the boot and idle bodies with RSP 8 off the
  ABI's 16-byte boundary, and chasing that found the same in every exception
  stub under an error code, `#GP` and `#PF` among them. Nothing had faulted
  because the kernel uses no SSE. The stubs pad now, and NASM refuses to
  assemble them if the frame sizes stop adding up.
- The empty weak tables pass 1 links against were defined in the files that
  index them, which made every index out of bounds of a `T[0]`.
- On arm64 the BSP's boot stack ran off its end into the identity tables below
  it, which every AP walks as it turns its MMU on: each AP took a translation
  fault with no vector to go to, and was only ever "still not running". The
  plain build had 248 bytes of its 16 KiB to spare. It has 32 KiB now, as on
  x86, and a guard page between it and the tables, which `CpuTable::StartAll`
  checks before it starts an AP.

## Rust UB checks

`make nocheck [ARCH=aarch64] RUSTUB=1` is the other half of the same idea,
for the half of the kernel UBSan cannot reach: rustc has no undefined-
behaviour sanitizer (`-Zsanitizer=` has no `undefined`), but `core` checks
the preconditions of its own unsafe operations when `-Zub-checks` is on --
an unaligned or null pointer in `ptr::read`/`write`, an overlapping
`copy_nonoverlapping`, a `slice::from_raw_parts` whose pointer or length is
wrong, `get_unchecked` past the end, `NonNull::new_unchecked(null)`,
`unreachable_unchecked` -- and `-Coverflow-checks` catches arithmetic that
wraps where it was not meant to. The flag turns both on by building the
`release` profile with `debug-assertions` (which `-Zub-checks` follows) and
`overflow-checks`, through cargo's `--config`; it must go through the
profile and not `RUSTFLAGS`, which would replace the per-target rustflags in
`src/rust/.cargo/config.toml` and drop `-Ccode-model=large`, the frame
pointers and the crates' software-backend cfgs. `-Z build-std` puts the
checks inside `core` as well as this tree's crates: the staticlib grows from
about 12 MB to about 16 MB.

A failed check is a panic through the kernel's `#[panic_handler]`, naming
the precondition and the line:

    RUST PANIC: panicked at kernel/src/lib.rs:122:25:
    unsafe precondition(s) violated: slice::get_unchecked requires that the
    index is within the slice

which makes every gate a check over the Rust it drives, exactly as `UBSAN=1`
does over the C++; the two are independent and can be set together.
`version` says `+rustub`. What it is not: an analysis of *your* `unsafe`.
A raw pointer dereferenced past its object, two `&mut` to the same place,
a read of uninitialised memory, a data race -- none of those are checked
here. Miri is what checks them, and it cannot run a kernel: for the crates
with no kernel in them (`netwire`, `ssh`) it can run over their host tests.

The flavour is recorded like UBSan's, in `out/flavor-$(ARCH)`, and both the
Rust staticlib and every `.ko` depend on that stamp: unlike the C++ objects
they have one place to live, so a switch of either flag rebuilds them and
relinks the image rather than leaving yesterday's instrumented `libkernel.a`
in place.

The first full run over the network gates (2026-09-20, both architectures)
reported nothing: no precondition and no overflow in the block layer, the
network layer, the drivers or the modules. What it did find was in the
kernel's own C++: the panic it raised printed no reason, because a log line
longer than its buffer was dropped whole rather than truncated
([Debug](debug.md)).

## Disk image

Build a bootable qcow2 disk image (MBR, one ext2 partition labelled `nos`
that is both where GRUB finds `/boot/kernel64.elf` and the root filesystem
the kernel mounts read-write):

```sh
./scripts/build-disk.sh                      # nos.qcow2, 1 GB
SIZE_MB=4096 ROOTFS=my-root ./scripts/build-disk.sh   # bigger, with my-root/ copied in (e.g. lib/modules)
```

This produces `nos.qcow2` (MBR, virtio-blk compatible, suitable for KVM-based public clouds including Google Cloud Compute Engine). It runs entirely inside Docker and needs no `--privileged`: the filesystem is populated by `mke2fs -d` and GRUB's boot code is written with `dd`. See [Run](run.md#google-cloud) for deploying it and [Filesystems](filesystems.md) for the root filesystem itself.

## Firmware: BIOS and UEFI

Both firmware flavours are supported by the same `nos.iso`: `grub-mkrescue`
writes a hybrid image whose El Torito catalog carries an `i386-pc` boot image
*and* an EFI system partition (`/efi/boot/bootx64.efi`), and the kernel then
adapts to whichever firmware it woke up under.

| | Legacy BIOS | UEFI |
|---|---|---|
| GRUB platform | `i386-pc` El Torito image + MBR boot code | ESP with `bootx64.efi` |
| Console | EGA text at `0xB8000` (`drivers/vga.cpp`) | GOP pixel framebuffer, 8x16 font (`drivers/fb_console.cpp`) |
| Keyboard | 8042 PS/2 | USB HID over xHCI (`src/rust/drivers/usb`); real UEFI laptops often have no 8042 at all |

The multiboot2 header asks GRUB for a framebuffer but marks both the console and
framebuffer tags optional, so BIOS boots keep legacy text mode while UEFI boots
get a linear framebuffer; `drivers/screen.cpp` picks the console at runtime from
what actually arrived, and `insmod all_video` in `build/grub.cfg` is what lets
GRUB set the mode at all.

Two limits worth knowing:

- The UEFI half of the ISO only exists if `grub-mkrescue` finds the
  `x86_64-efi` modules (`grub-efi-amd64-bin`) plus `mtools` — the FAT ESP is
  built with them — on the build host. The Docker builder image and the CI
  runner both install them, so `scripts/build-iso-docker.sh`, a native `make`
  and the release artifacts all produce the hybrid ISO; a bare host missing
  those two packages silently gets a BIOS-only ISO instead.
- `nos.qcow2` from `scripts/build-disk.sh` is MBR with `i386-pc` GRUB in the
  boot code — BIOS-only by design, since that is how the KVM clouds boot it.
  There is no ESP on that image.

Booting the ISO under OVMF to exercise the UEFI path is described in
[Run](run.md#uefi-boot-ovmf).
