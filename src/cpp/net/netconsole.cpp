#include "netconsole.h"
#include "arp.h"

#include <kernel/dmesg.h>
#include <kernel/panic.h>
#include <kernel/parameters.h>
#include <kernel/sched.h>
#include <kernel/trace.h>
#include <mm/new.h>

namespace Kernel
{

/* Bound on how much the panic path tries to push out, so a wedged TX ring
   cannot turn a panic into an endless loop. */
static const ulong PanicMaxPackets = 64;

Netconsole::Netconsole()
    : Head(0)
    , Used(0)
    , Dev(nullptr)
    , TaskPtr(nullptr)
    , DstPort(0)
    , SrcPort(0)
    , Enabled(false)
    , Dropped(0)
    , Sent(0)
    , TxFailed(0)
{
}

Netconsole::~Netconsole()
{
}

bool Netconsole::IsEnabled()
{
    return Enabled;
}

bool Netconsole::Setup()
{
    auto& params = Parameters::GetInstance();

    if (!params.IsNetconsoleEnabled())
        return false;

    if (Enabled)
        return false;

    DstIp = params.GetNetconsoleIp();
    DstPort = params.GetNetconsolePort();
    SrcPort = DstPort;

    /* Everything traced before this point is still in dmesg -- replay it into
       the ring so the collector sees the whole boot, not just the tail. */
    for (DmesgMsg* msg = Dmesg::GetInstance().Next(nullptr); msg != nullptr;
         msg = Dmesg::GetInstance().Next(msg))
    {
        ulong len = Stdlib::StrLen(msg->Str);
        if (len == 0)
            continue;

        Stdlib::AutoLock lock(Lock);
        Append(msg->Str, len);
    }

    Enabled = true;

    Trace(0, "Netconsole: capturing for %u.%u.%u.%u:%u",
        (ulong)((DstIp.Addr4 >> 24) & 0xFF), (ulong)((DstIp.Addr4 >> 16) & 0xFF),
        (ulong)((DstIp.Addr4 >> 8) & 0xFF), (ulong)(DstIp.Addr4 & 0xFF),
        (ulong)DstPort);

    return true;
}

bool Netconsole::Start(NetDevice* dev)
{
    if (!Enabled || dev == nullptr || TaskPtr != nullptr)
        return false;

    Dev = dev;

    Task* task = Mm::TAlloc<Task, Tag>("netcon");
    if (task == nullptr)
    {
        Dev = nullptr;
        return false;
    }

    /* Publish the task before it runs: Log() uses the pointer to recognize
       (and skip) the messages the TX path itself produces. */
    TaskPtr = task;

    if (!task->Start(&Netconsole::TaskFunc, this))
    {
        TaskPtr = nullptr;
        task->Put();
        Dev = nullptr;
        return false;
    }

    Trace(0, "Netconsole: started on %s", dev->GetName());
    return true;
}

void Netconsole::Stop()
{
    if (TaskPtr != nullptr)
    {
        Task* task = TaskPtr;
        task->SetStopping();
        task->Wait();
        TaskPtr = nullptr;
        task->Put();
    }

    Dev = nullptr;
}

void Netconsole::Log(const char* s)
{
    if (!Enabled || s == nullptr)
        return;

    /* Skip whatever the drain task produces (including IRQs taken on top of
       it): the TX path traces, and capturing that would feed the loop back
       into itself. Those lines still reach dmesg and the console. */
    if (TaskPtr != nullptr && Task::TryGetCurrentTask() == TaskPtr)
        return;

    ulong len = Stdlib::StrLen(s);
    if (len == 0)
        return;

    if (len > MaxRecordLen)
        len = MaxRecordLen;

    /* A panic runs with interrupts off and the other CPUs on their way to a
       halt -- one of them may hold the lock and never release it, so the panic
       path writes to the ring unlocked rather than deadlocking on it. */
    if (Panicker::GetInstance().IsActive())
    {
        Append(s, len);
        return;
    }

    Stdlib::AutoLock lock(Lock);
    Append(s, len);
}

void Netconsole::PushBytes(const u8* src, ulong len)
{
    ulong tail = (Head + Used) % BufSize;

    ulong first = BufSize - tail;
    if (first > len)
        first = len;

    Stdlib::MemCpy(&Buf[tail], src, first);
    if (len > first)
        Stdlib::MemCpy(&Buf[0], src + first, len - first);

    Used += len;
}

void Netconsole::PopBytes(u8* dst, ulong len)
{
    if (len > Used)
        len = Used;

    ulong first = BufSize - Head;
    if (first > len)
        first = len;

    if (dst != nullptr)
    {
        Stdlib::MemCpy(dst, &Buf[Head], first);
        if (len > first)
            Stdlib::MemCpy(dst + first, &Buf[0], len - first);
    }

    Head = (Head + len) % BufSize;
    Used -= len;
}

bool Netconsole::PeekRecordLen(u16& len)
{
    if (Used < RecordHdrSize)
        return false;

    u8 lo = Buf[Head];
    u8 hi = Buf[(Head + 1) % BufSize];
    len = (u16)((ulong)lo | ((ulong)hi << 8));

    if ((ulong)len + RecordHdrSize > Used)
        return false;

    return true;
}

void Netconsole::DropOldest()
{
    u16 len;
    if (!PeekRecordLen(len))
    {
        /* Should not happen; resync rather than spin forever. */
        Head = 0;
        Used = 0;
        return;
    }

    PopBytes(nullptr, RecordHdrSize + (ulong)len);
    Dropped++;
}

void Netconsole::Append(const char* s, ulong len)
{
    ulong need = RecordHdrSize + len;

    if (need > BufSize)
        return;

    /* Full ring: the newest lines are the interesting ones, so evict the
       oldest records rather than dropping what is being logged now. */
    while (Used + need > BufSize)
        DropOldest();

    u8 hdr[RecordHdrSize];
    hdr[0] = (u8)(len & 0xFF);
    hdr[1] = (u8)((len >> 8) & 0xFF);

    PushBytes(hdr, RecordHdrSize);
    PushBytes((const u8*)s, len);
}

ulong Netconsole::PopBatch(u8* buf, ulong bufSize)
{
    ulong filled = 0;

    for (;;)
    {
        u16 len;
        if (!PeekRecordLen(len))
            break;

        if (filled + (ulong)len > bufSize)
            break;

        PopBytes(nullptr, RecordHdrSize);
        PopBytes(buf + filled, len);
        filled += len;
    }

    return filled;
}

bool Netconsole::SendBatch(const u8* buf, ulong len)
{
    if (Dev == nullptr || len == 0)
        return false;

    return Dev->SendUdp(DstIp, DstPort, Dev->GetIp(), SrcPort, buf, len);
}

void Netconsole::TaskFunc(void* ctx)
{
    Netconsole* nc = static_cast<Netconsole*>(ctx);
    nc->Run();
}

void Netconsole::Run()
{
    Task* task = Task::GetCurrentTask();

    while (!task->IsStopping())
    {
        /* No address yet (DHCP still running, or a static IP not set): keep
           buffering, the backlog goes out as soon as there is one. */
        if (Dev == nullptr || Dev->GetIp().IsZero())
        {
            Sleep(NoLinkPollMs * Const::NanoSecsInMs);
            continue;
        }

        ulong len;
        {
            Stdlib::AutoLock lock(Lock);
            len = PopBatch(PktBuf, sizeof(PktBuf));
        }

        if (len == 0)
        {
            Sleep(IdlePollMs * Const::NanoSecsInMs);
            continue;
        }

        if (SendBatch(PktBuf, len))
            Sent++;
        else
            TxFailed++;
    }
}

void Netconsole::PanicFlush()
{
    if (!Enabled || Dev == nullptr || Dev->GetIp().IsZero())
        return;

    /* Only if the collector (or the gateway to it) is already in the ARP
       cache: with the other CPUs halted and interrupts off nothing would ever
       deliver an ARP reply, so a resolve would just burn the panic. */
    Net::MacAddress mac;
    if (!ArpTable::GetInstance().Lookup(Dev->RouteIp(DstIp), mac))
        return;

    for (ulong i = 0; i < PanicMaxPackets; i++)
    {
        /* Unlocked on purpose -- see the comment in Log(). */
        ulong len = PopBatch(PktBuf, sizeof(PktBuf));
        if (len == 0)
            break;

        if (SendBatch(PktBuf, len))
            Sent++;
        else
            TxFailed++;
    }
}

void Netconsole::Dump(Stdlib::Printer& printer)
{
    if (!Enabled)
    {
        printer.Printf("netconsole: disabled (boot with netconsole=ip:port)\n");
        return;
    }

    printer.Printf("netconsole: ");
    DstIp.Print(printer);
    printer.Printf(":%u src port %u dev %s\n", (ulong)DstPort, (ulong)SrcPort,
        (Dev != nullptr) ? Dev->GetName() : "none");

    ulong used, dropped, sent, txFailed;
    {
        Stdlib::AutoLock lock(Lock);
        used = Used;
        dropped = Dropped;
        sent = Sent;
        txFailed = TxFailed;
    }

    printer.Printf("  buffered %u/%u bytes, dropped %u msgs, sent %u pkts, tx failed %u\n",
        used, (ulong)BufSize, dropped, sent, txFailed);
}

}
