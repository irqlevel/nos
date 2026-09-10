#pragma once

#include <include/types.h>

/* SHA-256 from the Rust side (src/rust/kernel/src/sha256.rs): RustCrypto's
   sha2, in the tree already for TLS. A context is heap-allocated by new and
   released by finish or free. */
extern "C" void* sha256_new();
extern "C" void sha256_update(void* ctx, const u8* data, ulong len);
extern "C" void sha256_finish(void* ctx, u8* out);
extern "C" void sha256_free(void* ctx);

namespace Kernel
{

/* A SHA-256 in progress. Feed it with Update, take the digest with Finish
   once; a hash abandoned before Finish is released by the destructor. */
class Sha256Hash final
{
public:
    static const ulong DigestSize = 32;

    Sha256Hash()
        : Ctx(sha256_new())
    {
    }

    ~Sha256Hash()
    {
        sha256_free(Ctx);
    }

    void Update(const void* data, ulong len)
    {
        sha256_update(Ctx, static_cast<const u8*>(data), len);
    }

    /* out is DigestSize bytes. False when the digest was already taken. */
    bool Finish(u8* out)
    {
        if (Ctx == nullptr)
            return false;
        sha256_finish(Ctx, out);
        Ctx = nullptr;
        return true;
    }

private:
    Sha256Hash(const Sha256Hash& other) = delete;
    Sha256Hash(Sha256Hash&& other) = delete;
    Sha256Hash& operator=(const Sha256Hash& other) = delete;
    Sha256Hash& operator=(Sha256Hash&& other) = delete;

    void* Ctx;
};

}
