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
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
CMD ["make", "nocheck"]
