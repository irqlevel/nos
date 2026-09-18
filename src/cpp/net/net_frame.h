#pragma once

#include <include/types.h>
#include <lib/list_entry.h>
#include <kernel/atomic.h>

namespace Kernel
{

struct NetFrame
{
    Stdlib::ListEntry Link;
    u8* Data;
    ulong DataPhys;
    ulong Length;
    Atomic Refcount;

    enum : u8 { Tx = 0, Rx = 1 };
    u8 Direction;

    typedef void (*ReleaseFn)(NetFrame* frame, void* ctx);
    ReleaseFn Release;
    void* ReleaseCtx;

    void Init();
    void Get();
    void Put();

    static NetFrame* AllocTx(ulong dataLen);

private:
    static void TxFrameRelease(NetFrame* frame, void* ctx);
};

/* The frame pool is Rust (src/rust/net/src/frame.rs) and builds these, so
   the two sides must lay them out the same way. crate::frame::NetFrame
   asserts the same number: a disagreement would be no compile error
   anywhere -- it would be a driver reading a length at the wrong offset. */
static_assert(sizeof(NetFrame) == 72, "src/rust/net/src/frame.rs mirrors this");

}
