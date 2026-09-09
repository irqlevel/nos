#include "chacha20.h"

namespace Stdlib
{

namespace
{

const ulong StateWords = 16;
/* Twenty rounds, run as ten column/diagonal pairs. */
const ulong RoundPairs = 10;

inline u32 RotateLeft32(u32 value, u32 count)
{
    return (value << count) | (value >> (32 - count));
}

inline u32 LoadLe32(const u8* p)
{
    return (u32)p[0] | ((u32)p[1] << 8) | ((u32)p[2] << 16) | ((u32)p[3] << 24);
}

inline void StoreLe32(u8* p, u32 value)
{
    p[0] = (u8)(value & 0xFF);
    p[1] = (u8)((value >> 8) & 0xFF);
    p[2] = (u8)((value >> 16) & 0xFF);
    p[3] = (u8)((value >> 24) & 0xFF);
}

inline void QuarterRound(u32& a, u32& b, u32& c, u32& d)
{
    a = a + b; d = d ^ a; d = RotateLeft32(d, 16);
    c = c + d; b = b ^ c; b = RotateLeft32(b, 12);
    a = a + b; d = d ^ a; d = RotateLeft32(d, 8);
    c = c + d; b = b ^ c; b = RotateLeft32(b, 7);
}

}

void ChaCha20Block(const u8* key, u32 counter, const u8* nonce, u8* out)
{
    u32 state[StateWords];

    /* "expa" "nd 3" "2-by" "te k" as four little-endian words. Spelled as
       numbers because the freestanding build has no place for a string
       constant that is never printed. */
    state[0] = 0x61707865;
    state[1] = 0x3320646E;
    state[2] = 0x79622D32;
    state[3] = 0x6B206574;

    for (ulong i = 0; i < 8; i++)
        state[4 + i] = LoadLe32(&key[4 * i]);

    state[12] = counter;

    for (ulong i = 0; i < 3; i++)
        state[13 + i] = LoadLe32(&nonce[4 * i]);

    u32 x[StateWords];
    for (ulong i = 0; i < StateWords; i++)
        x[i] = state[i];

    for (ulong i = 0; i < RoundPairs; i++)
    {
        /* Columns */
        QuarterRound(x[0], x[4], x[8],  x[12]);
        QuarterRound(x[1], x[5], x[9],  x[13]);
        QuarterRound(x[2], x[6], x[10], x[14]);
        QuarterRound(x[3], x[7], x[11], x[15]);

        /* Diagonals */
        QuarterRound(x[0], x[5], x[10], x[15]);
        QuarterRound(x[1], x[6], x[11], x[12]);
        QuarterRound(x[2], x[7], x[8],  x[13]);
        QuarterRound(x[3], x[4], x[9],  x[14]);
    }

    /* The feed-forward addition is what makes the block function one-way:
       without it the twenty rounds are a permutation anyone could run
       backwards, and the CSPRNG's rekeying would be reversible. */
    for (ulong i = 0; i < StateWords; i++)
        StoreLe32(&out[4 * i], x[i] + state[i]);
}

}
