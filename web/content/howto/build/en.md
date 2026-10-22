---
title: "Build nos"
description: "Build the kernel, its modules and the boot images for x86-64 and arm64: in Docker on macOS, or natively on Linux with the pinned Rust nightly."
order: 1
updated: 2026-10-22
---

By the end of this guide you will have built nos from source: the x86-64 ISO and kernel, the arm64 kernel image, the loadable modules, and, if you want them, a disk image for QEMU and the KVM clouds.

You need:

- a clone of the repository;
- either Docker, which is how nos is built on macOS (Apple Silicon included), or a Linux machine with the toolchain installed. The reference system is Ubuntu 22.04: the builder image and CI both run on it;
- disk space: the builder image takes about 4.7 GB, and the build outputs of both architectures need a couple of gigabytes more under `out/` and `src/rust/target/`;
- network access to install the toolchain. The build itself never touches the network.

```sh
git clone https://github.com/irqlevel/nos.git
cd nos
```

## Build in Docker

```sh
./scripts/build-iso-docker.sh
```

The script builds the `nos-builder` image from the [Dockerfile](https://github.com/irqlevel/nos/blob/master/Dockerfile). The image is Ubuntu 22.04 for `linux/amd64` with clang, lld, nasm, GRUB for both BIOS and UEFI, xorriso, mtools, cppcheck, e2fsprogs, squashfs-tools, QEMU and the pinned Rust nightly. The script then runs `make clean && make all` in a container, with your checkout mounted at `/src`. The image holds only the toolchain. Your sources are mounted when the container starts, so you never rebuild the image after editing them. When the build finishes, the script prints:

```text
Built nos.iso and kernel64.elf (use kernel64.elf in GDB for symbols)
```

`make all` runs cppcheck first, then builds the x86-64 ISO and every module. The script starts with `make clean`, so every run is a full rebuild. On an Apple Silicon Mac the `linux/amd64` image runs under emulation, so the build takes a while.

### Rebuild only what changed

Once the image exists, you can run `make` in it directly. This is the same build the smoke test runs:

```sh
docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder \
    bash -c 'make rust && make nocheck'
```

For arm64:

```sh
docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder \
    bash -c 'make nocheck ARCH=aarch64'
```

> The image has no git. The build scripts work out the release name and the commit on the host and pass them in as `VERSION` and `GIT_DESC`. If you run `docker run` yourself without them, the kernel's `version` command prints `nos dev (unknown)`. To record the commit, add `-e GIT_DESC="$(git rev-parse --short HEAD)"`, as the release workflow does.

### Build a disk image

```sh
./scripts/build-disk.sh
```

This builds the kernel in the container (`make clean && make nocheck`) and then writes `nos.qcow2`. It's an MBR disk with GRUB in the boot sector and one ext2 partition labelled `nos`. That partition holds `/boot/kernel64.elf`, and nos mounts it read-write as the root filesystem. The script doesn't need `--privileged`. Two variables change what it builds:

```sh
SIZE_MB=4096 ROOTFS=my-root ./scripts/build-disk.sh
```

`SIZE_MB` sets the size in MB (default 1024). `ROOTFS` names a directory to copy into the root filesystem next to `/boot`, for example modules under `lib/modules`, an `/etc/rc` or SSH keys. The disk boots under BIOS only, the way the KVM clouds boot it (see [Run: Google Cloud](/docs/run#google-cloud)). Because the script builds the kernel too, the same run also produces `nos.iso` and `bin/kernel64.elf`.

### Build the system image

```sh
./scripts/build-image.sh
./scripts/build-image.sh --clean
```

Either command produces `nos-x86_64.squashfs`, which holds the kernel, every module and the GRUB entries that a server is set up from (see [Install nos on a dedicated server](/howto/install-on-a-server)). With no argument, the script rebuilds what changed since the last build. With `--clean` it builds everything from scratch, the way a release is built.

## Build natively on Linux

On Ubuntu 22.04, install the same packages the builder image has:

```sh
sudo apt-get update
sudo apt-get install -y build-essential clang lld llvm nasm \
    grub2-common grub-pc-bin grub-efi-amd64-bin xorriso mtools cppcheck \
    parted e2fsprogs squashfs-tools qemu-utils qemu-system-x86 qemu-system-arm \
    python3 curl
```

The Rust toolchain must be a dated nightly with the `rust-src` component. Run the following from the repository root. It's the command CI uses, and it installs the exact nightly named in [`src/rust/rust-toolchain.toml`](https://github.com/irqlevel/nos/blob/master/src/rust/rust-toolchain.toml):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --component rust-src --default-toolchain \
    "$(sed -n 's/^channel *= *"\(.*\)"/\1/p' src/rust/rust-toolchain.toml)"
export PATH="$HOME/.cargo/bin:$PATH"
```

Then build:

```sh
make
```

| Command | What it does |
|---|---|
| `make` | runs `make check`, then builds `nos.iso` and the modules |
| `make nocheck` | the same without cppcheck |
| `make check` | runs only cppcheck over `src/cpp`; any finding fails it |
| `make rust` | builds only the Rust staticlib, `libkernel.a` |
| `make modules` | builds only the modules |
| `make nocheck ARCH=aarch64` | builds the arm64 kernel, its image and its modules |
| `make image` | builds `nos-x86_64.squashfs` (x86-64 only; needs `mksquashfs`) |
| `make nocheck UBSAN=1` | builds the C++ under clang's UB sanitizer (see below) |
| `make nocheck RUSTUB=1` | builds the Rust with `core`'s unsafe-precondition and overflow checks (see below) |
| `make smoke` | runs `scripts/smoke-test.sh`, which builds in Docker and boots headless |
| `make clean` | removes `out/`, the kernels, `nos-arm64.img`, the ISO and the system image, and runs `cargo clean` |

`ARCH` selects the architecture: `x86_64` (the default) or `aarch64`. Objects go to `out/$(ARCH)/`. For an arm64-only build you need only make, clang, lld, llvm and the nightly; that's all the release workflow installs for it. `make smoke` builds in Docker. With a native toolchain, build with `make nocheck` and then run `./scripts/smoke-test.sh --skip-build`.

## What you get

| File | What it is |
|---|---|
| `nos.iso` | the x86-64 boot ISO with GRUB and the kernel; it boots under both BIOS and UEFI |
| `kernel64.elf`, `bin/kernel64.elf` | the x86-64 kernel; the GDB scripts load the copy in `bin/` |
| `nos-arm64.img` | the arm64 kernel in Linux `Image` format, for QEMU `-kernel` |
| `kernel-arm64.elf`, `bin/kernel-arm64.elf` | the arm64 kernel ELF, with symbols |
| `out/<arch>/modules/*.ko` | the modules: `hello`, `modtest`, `slowexit`, `blkload`, `netblk`, `netload`, `sshd`, `hv`, `heapbench` |
| `nos.qcow2` | the disk image, from `scripts/build-disk.sh` |
| `nos-x86_64.squashfs` | the system image, from `make image` or `scripts/build-image.sh` |

A module loads only into a kernel built from the same tree. The loader checks a digest of the kernel interface and refuses a module built against a different one (see [What a module may call](/docs/modules#what-a-module-may-call)). Tagged releases carry the kernels, `nos.iso`, `nos.qcow2`, `nos-arm64.img`, the system image and every module, prebuilt ([`release.yml`](https://github.com/irqlevel/nos/blob/master/.github/workflows/release.yml)).

## How the toolchain is pinned and dependencies are vendored

- `src/rust/rust-toolchain.toml` pins a dated nightly (`nightly-2026-08-30` at the time of writing) with `rust-src`. The kernel's Rust needs nightly for two reasons: `-Z build-std`, which rebuilds `core` and `alloc` with the large code model, and the `alloc_error_handler` feature.
- The external crates are vendored in `src/rust/vendor`, and the Makefile runs cargo with `--offline`. These include rustls and the RustCrypto crates behind TLS and SSH, and the crates that std itself depends on. No build ever reaches crates.io. If a build asks for the network, a dependency was never vendored, and cargo fails with an error instead of downloading it.
- After you add, remove or bump a dependency in `src/rust`, run `scripts/vendor.sh`, which is the only step that needs the network. Commit the updated `src/rust/vendor` together with `Cargo.lock`. If you bump the toolchain pin, re-run `scripts/vendor.sh` in the same commit: the vendored set includes std's own dependencies, and those change from one nightly to the next.
- The Dockerfile and both CI workflows read the pin from that one file, so it's the only place the version lives.

## Build with undefined-behaviour checks

```sh
make nocheck UBSAN=1
make nocheck RUSTUB=1
make nocheck ARCH=aarch64 UBSAN=1 RUSTUB=1
```

`UBSAN=1` builds the C++ with clang's undefined-behaviour sanitizer. `RUSTUB=1` turns on `core`'s checks of its own unsafe preconditions and arithmetic overflow checks in the Rust. You can use the flags separately or together. The first failed check panics and names its site:

```text
PANIC:Report():ubsan.cpp,267: UBSAN: shift exponent 64 is too large for 64-bit type 'unsigned long' at src/cpp/...
```

The output files keep their usual names. The instrumented C++ objects go to `out/<arch>-ubsan/`, and a stamp in `out/` relinks the images whenever you switch flavour, so a plain build never ships an instrumented kernel. `version` adds `+ubsan` or `+rustub` to its output. To collect every C++ report during a boot instead of panicking on the first, boot with `ubsan=warn`. For details, see [UBSan](/docs/build#ubsan) and [Rust UB checks](/docs/build#rust-ub-checks).

## Keep both architectures green

For both `x86_64` and `aarch64`, CI ([`ci.yml`](https://github.com/irqlevel/nos/blob/master/.github/workflows/ci.yml)) runs `make check`, `make nocheck ARCH=...` and a smoke boot under TCG. Its last step builds with `UBSAN=1 RUSTUB=1` and smoke-boots that build too. A change to common code that builds only on x86 breaks CI.

Two things to watch for:

- **Source lists are explicit.** The Makefile lists every C++ file in `CXX_SRC_x86_64` and `CXX_SRC_aarch64`, and the assembly in `ASM_SRC_*` and `ASM_S_SRC_*`. A new `.cpp` file that isn't in a list is silently not compiled, so add portable code to both. If common code calls an x86-only entry point, add an unreachable link stub to [`x86_driver_stubs.cpp`](https://github.com/irqlevel/nos/blob/master/src/cpp/arch/arm64/x86_driver_stubs.cpp) rather than an `#ifdef`.
- **Judge a run by its exit code.** Before you send a change, build both architectures and boot both:

```sh
make nocheck && ./scripts/smoke-test.sh --skip-build
make nocheck ARCH=aarch64 && ./scripts/smoke-arm64.sh --skip-build
```

On macOS, run `./scripts/smoke-test.sh` and `./scripts/smoke-arm64.sh` without `--skip-build`, and each script builds incrementally in the `nos-builder` container first. A smoke script exits with 0 when the boot printed all its markers, and with 1 on a panic, a timeout or a failed build. For what each check covers, see [Run the gates](/howto/run-the-gates).

## Troubleshooting

- **`No rule to make target` after you move, rename or delete a header.** A stale dependency (`.d`) file in `out/` still names the old path. Run `make clean`, or delete the `.d` files that mention the old path. The message doesn't contain the word "error", so check `make`'s exit code instead of searching its output. Don't pipe the build into `tail` either: the pipeline then returns `tail`'s exit status.
- **cppcheck fails `make` or `build-iso-docker.sh`.** `make check` stops on any finding. Use `make nocheck` to skip it while you iterate, but CI runs `make check`.
- **The ISO boots under BIOS but not under UEFI.** `grub-mkrescue` didn't find the `x86_64-efi` modules (`grub-efi-amd64-bin`) or `mtools`, and silently built a BIOS-only ISO. The builder image and CI have both packages.
- **A build tries to reach the network.** A dependency was never vendored. Run `scripts/vendor.sh`.
- **The container downloads a Rust toolchain, or reports that it isn't installed.** Your `nos-builder` image is older than the current toolchain pin. `build-iso-docker.sh` and `build-disk.sh` rebuild the image on every run. `build-image.sh` builds it only when there is none, and the smoke scripts (and `mkrootfs.sh`, when it falls back to Docker) use whichever image exists. Rebuild it:

  ```sh
  docker build --platform linux/amd64 -t nos-builder .
  ```

- **The link fails with `a static constructor or destructor: this kernel runs none`.** The kernel runs no `.init_array`, so the linker rejects any global whose type has a non-`constexpr` constructor or a non-trivial destructor. Give the type a `constexpr` constructor, or make the object a function-local static.
- **The link fails because the functions aren't where the symbol table says.** The three-pass link checks that the symbol table used for backtraces matches the final image. See [Build](/docs/build#four-things-the-makefile-will-not-tell-you).

## Next steps

- [Run nos in QEMU](/howto/run-in-qemu)
- [Run the gates](/howto/run-the-gates)
- [Debug nos with GDB](/howto/debug-with-gdb)
- [Write a loadable kernel module in Rust](/howto/write-a-module)
- The full reference: [Build](/docs/build)
