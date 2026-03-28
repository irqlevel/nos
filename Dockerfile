# Build nos x86_64 kernel ISO. Use on Mac: ./build-iso-docker.sh
# Platform amd64 so the toolchain produces x86_64 ELF (runs via emulation on Apple Silicon).
FROM --platform=linux/amd64 ubuntu:22.04

RUN apt-get update && apt-get install -y \
    build-essential \
    clang \
    nasm \
    grub2-common \
    grub-pc-bin \
    xorriso \
    cppcheck \
    parted \
    e2fsprogs \
    qemu-utils \
    python3 \
    curl \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    --default-toolchain nightly --component rust-src
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /src
CMD ["make", "nocheck"]
