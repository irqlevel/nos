FROM --platform=linux/amd64 ubuntu:22.04

RUN apt-get update && apt-get install -y \
    build-essential \
    ca-certificates \
    git \
    iasl \
    nasm \
    python3 \
    python3-distutils \
    uuid-dev \
    zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
