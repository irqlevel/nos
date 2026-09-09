# Build

The build is parameterized by `ARCH` (default `x86_64`, or `aarch64`); objects go to `out/$(ARCH)/`.

## Native

Requires clang, nasm, ld, grub-mkrescue with `xorriso` + `mtools`, and a **nightly**
rustup toolchain with the `rust-src` component — the Rust staticlib uses `-Z build-std`
to rebuild `core`/`alloc` with `-Ccode-model=large`; `src/rust/rust-toolchain.toml` pins it:

```sh
make
```

`make` runs `cppcheck` static analysis first (`make check`) and fails on any
finding; `make nocheck` skips it. `make smoke` builds in Docker and runs the
headless boot smoke test (`scripts/smoke-test.sh`).

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
| Keyboard | 8042 PS/2 | USB HID over xHCI (`drivers/usb/`); real UEFI laptops often have no 8042 at all |

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
