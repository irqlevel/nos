#pragma once

#include <include/types.h>

namespace Stdlib
{

/* The ChaCha20 block function of RFC 8439: twenty rounds of the quarter-round
   over a 64-byte state built from a 256-bit key, a 32-bit block counter and a
   96-bit nonce, added back to the state it started from.

   Only the block function is here, not a stream cipher: the kernel's use for
   ChaCha20 is the CSPRNG (kernel/random.cpp), which wants raw keystream. The
   TLS client's ChaCha20 is a different implementation entirely -- RustCrypto's,
   in the Rust tree -- and neither has to know about the other. */

const ulong ChaCha20KeySize = 32;
const ulong ChaCha20NonceSize = 12;
const ulong ChaCha20BlockSize = 64;

/* key is ChaCha20KeySize bytes, nonce is ChaCha20NonceSize, out is
   ChaCha20BlockSize. */
void ChaCha20Block(const u8* key, u32 counter, const u8* nonce, u8* out);

}
