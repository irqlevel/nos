# CRYPTOGAMS' ChaCha20 and Poly1305

The assembly the crypto crate runs (`../src/asm/`) is made from these files
by `scripts/cryptogams.sh`. They are Andy Polyakov's, from his own
distribution of the assembly he wrote for OpenSSL, taken as they are:

- from https://github.com/dot-asm/cryptogams
- at commit `4acc4a308eab6049d1cf1c7ba61b353a36ce0e2d` (2026-10-20), which
  has the fix of a missed carry in arm64's NEON Poly1305 (its issue #21)
- every commit to them in that repository's history his own

| File | SHA-256 |
|---|---|
| `LICENSE` | `c2e940a0b67954b8e9284d34adb38ae55ffa38219bc879f0d8dd96cd06318526` |
| `x86_64/chacha-x86_64.pl` | `3f8cd93e3ff26b6fae9d9090713118263bc24c507ac72516143c1de82478fa75` |
| `x86_64/poly1305-x86_64.pl` | `ea968cd5d4bde5b81da669c9571b3ae6827445f5e959b1b14f461ae67be44d68` |
| `x86_64/x86_64-xlate.pl` | `ae43d9ec5c89021f65a33f69bf1a3fda864ef9f13036e643edc8b0bf62a7aae1` |
| `arm/chacha-armv8.pl` | `a1e6d1fc0d6077b00ca27f07be0ea1803bcbb419c36d3738af2fdec40de5c7aa` |
| `arm/poly1305-armv8.pl` | `1c81406342f9195de6125139e7f3cf0ba9ef6b15ef5ba37733899f9f0be7d37e` |
| `arm/arm-xlate.pl` | `710fcf124cab4cdd6fb05a4cba3a3f5e4ceb9273a22885b8d954bc07ca8874fe` |

Of the repository, nothing else: not `arm/arm_arch.h`, which the kernel
mode the files are built in does not include, and no other algorithm.

## The license

`LICENSE` is the repository's: a BSD-style license, or the GPL instead,
at the taker's choice. This tree takes the BSD-style one, which an MIT
kernel can carry, and keeps its three conditions:

- **a copy of the source keeps the notice**: `LICENSE` here, and the
  whole of it at the top of every file `scripts/cryptogams.sh` makes;
- **a binary keeps it too**, in what is handed out with it: the system
  image carries it as `/lib/licenses/CRYPTOGAMS` (the Makefile's `image`);
- **nobody's name endorses anything**: nothing here says CRYPTOGAMS or
  its author stands behind nos.

Why these and not another copy of the same code: Linux's
(`arch/*/crypto/*-cryptogams.pl`) are GPL-2.0, which an MIT tree does not
take; OpenSSL's are Apache-2.0, which it could, but are the same work under
the project's license rather than the author's own, without the kernel mode
these have.

## The kernel mode

Built for the kernel, neither file reads OpenSSL's capability words
(`OPENSSL_ia32cap_P`, `OPENSSL_armcap_P`): each code path is an entry
point of its own, and the crypto crate picks one by what the kernel's SIMD
section offers (`kcore::simd`). Their integer-only entry points --
`ChaCha20_ctr32`, `poly1305_init`, `poly1305_blocks`, `poly1305_emit` --
use no vector register, and run where no section can be had.

## Taking a newer commit

Copy the files from the new commit over these, put its hash and the files'
SHA-256 above, run `scripts/cryptogams.sh` and commit what it writes with
them; then the crypto crate's gates (`docs/tls.md`, "The assembly").
`make check` runs `scripts/cryptogams.sh --check`, which fails while the
two disagree.
