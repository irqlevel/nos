#include "net_frame.h"

#include <lib/stdlib.h>
#include <mm/new.h>
#include <mm/page_table.h>

namespace Kernel
{

void NetFrame::Init()
{
    Link.Init();
    Data = nullptr;
    DataPhys = 0;
    Length = 0;
    Refcount.Set(0);
    Direction = Tx;
    Release = nullptr;
    ReleaseCtx = nullptr;
}

void NetFrame::Get()
{
    Refcount.Inc();
}

void NetFrame::Put()
{
    if (Refcount.DecAndTest())
    {
        if (Release)
            Release(this, ReleaseCtx);
    }
}

NetFrame* NetFrame::AllocTx(ulong dataLen)
{
    ulong totalSize = sizeof(NetFrame) + dataLen;
    NetFrame* frame = (NetFrame*)Mm::Alloc(totalSize, 'NtFr');
    if (!frame)
        return nullptr;

    Stdlib::MemSet(frame, 0, sizeof(NetFrame));
    frame->Link.Init();
    frame->Data = (u8*)(frame + 1);
    frame->Length = 0;
    frame->Refcount.Set(1);
    frame->Direction = Tx;
    frame->Release = TxFrameRelease;
    frame->ReleaseCtx = nullptr;

    auto& pt = Mm::PageTable::GetInstance();
    frame->DataPhys = pt.VirtToPhys((ulong)frame->Data);
    if (frame->DataPhys == 0)
    {
        Mm::Free(frame);
        return nullptr;
    }

    return frame;
}

void NetFrame::TxFrameRelease(NetFrame* frame, void* ctx)
{
    (void)ctx;
    Mm::Free(frame);
}

}
