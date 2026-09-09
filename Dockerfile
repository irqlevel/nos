# Build nos kernels (x86_64 ISO and arm64 image). Use on Mac: ./build-iso-docker.sh
# Platform amd64 so the toolchain produces x86_64 ELF (runs via emulation on Apple Silicon).
# clang is multi-target: the same image cross-compiles aarch64 (linked with ld.lld).
FROM --platform=linux/amd64 ubuntu:22.04

RUN apt-get update && apt-get install -y \
    build-essential \
    clang \
    lld \
    llvm \
    nasm \
    grub2-common \
    grub-pc-bin \
    grub-efi-amd64-bin \
    xorriso \
    mtools \
    cppcheck \
    parted \
    e2fsprogs \
    qemu-utils \
    qemu-system-x86 \
    qemu-system-arm \
    python3 \
    curl \
    && rm -rf /var/lib/apt/lists/*

# src/rust/rust-toolchain.toml names the exact nightly, and the exactness is
# the point: src/rust/vendor holds the dependency sources that this one
# nightly's own library workspace resolves to, and no other (see the comment
# there). Installing it here, rather than "nightly", keeps the image
# self-contained -- no build has to fetch a toolchain at run time.
COPY src/rust/rust-toolchain.toml /tmp/rust-toolchain.toml
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    --component rust-src --default-toolchain \
    "$(sed -n 's/^channel *= *"\(.*\)"/\1/p' /tmp/rust-toolchain.toml)"
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /src
CMD ["make", "nocheck"]
