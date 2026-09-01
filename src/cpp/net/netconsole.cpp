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

/* Consecutive refusals after which the panic path gives up on the device.
   A TX ring that is merely full drains on its own while we retry the same
   datagram; one that is wedged never will. */
static const ulong PanicMaxTxRetries = 8;

/* How much of the undrained backlog the panic path keeps in front of the
   report, for context. The rest is dropped -- see PanicFlush(). */
static const ulong PanicBacklogKeep = 8 * 1024;

Netconsole::Netconsole()
    : Head(0)
    , Used(0)
    , Dev(nullptr)
    , TaskPtr(nullptr)
    , DstPort(0)
    , SrcPort(0)
    , Enabled(false)
    , TailKeepBytes(0)
    , BacklogTrimmed(false)
    , Dropped(0)
    , Sent(0)
    , TxFailed(0)
    , Seq(0)
    , PanicBacklog(0)
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

    /* nctail=N caps what the link-up drain carries; a bound at or above the
       ring is the same as no bound at all. */
    ulong tailKb = params.GetNetconsoleTailKb();
    if (tailKb != 0)
    {
        ulong bytes = tailKb * Const::KB;
        TailKeepBytes = (bytes < BufSize) ? bytes : 0;
    }

    /* Everything traced before this point is still in dmesg -- replay it into
       the ring so the collector sees the whole boot, not just the tail. */
    DmesgMsg* msg = Dmesg::GetInstance().Next(nullptr);
    for (ulong replayed = 0; msg != nullptr; replayed++)
    {
        ulong len = Stdlib::StrLen(msg->Str);
        if (len != 0)
        {
            Stdlib::AutoLock lock(Lock);
            Append(msg->Str, len);
        }

        /* Never outlast the log itself: a walk past its capacity is chasing a
           tail someone else is still extending. */
        if (replayed + 1 == Dmesg::MaxMsgs)
        {
            Dmesg::GetInstance().Release(msg);
            break;
        }

        msg = Dmesg::GetInstance().Next(msg);
    }

    Enabled = true;

    Trace(0, "Netconsole: capturing for %u.%u.%u.%u:%u, backlog cap %u bytes",
        (ulong)((DstIp.Addr4 >> 24) & 0xFF), (ulong)((DstIp.Addr4 >> 16) & 0xFF),
        (ulong)((DstIp.Addr4 >> 8) & 0xFF), (ulong)(DstIp.Addr4 & 0xFF),
        (ulong)DstPort, TailKeepBytes);

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

void Netconsole::TrimHead(ulong region, ulong keepBytes)
{
    while (region > keepBytes)
    {
        u16 len;
        if (!PeekRecordLen(len))
            break;

        ulong record = RecordHdrSize + (ulong)len;
        if (record > region)
            break;

        PopBytes(nullptr, record);
        Dropped++;
        region = region - record;
    }
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

ulong Netconsole::PeekBatch(u8* buf, ulong bufSize, ulong& consumed)
{
    ulong filled = 0;
    ulong pos = Head;   /* read cursor, left where it is until the send works */
    ulong left = Used;

    consumed = 0;

    for (;;)
    {
        if (left < RecordHdrSize)
            break;

        ulong lo = Buf[pos];
        ulong hi = Buf[(pos + 1) % BufSize];
        ulong len = lo | (hi << 8);

        if (len + RecordHdrSize > left)
            break;

        if (filled + len > bufSize)
            break;

        pos = (pos + RecordHdrSize) % BufSize;
        left = left - RecordHdrSize;

        ulong first = BufSize - pos;
        if (first > len)
            first = len;

        Stdlib::MemCpy(buf + filled, &Buf[pos], first);
        if (len > first)
            Stdlib::MemCpy(buf + filled + first, &Buf[0], len - first);

        pos = (pos + len) % BufSize;
        left = left - len;
        filled += len;
        consumed += RecordHdrSize + len;
    }

    return filled;
}

bool Netconsole::SendBatch(ulong textLen)
{
    if (Dev == nullptr || textLen == 0)
        return false;

    PktBuf[0] = DgramMagic0;
    PktBuf[1] = DgramMagic1;
    PktBuf[2] = DgramMagic2;
    PktBuf[3] = DgramMagic3;
    PktBuf[4] = (u8)(Seq & 0xFF);
    PktBuf[5] = (u8)((Seq >> 8) & 0xFF);
    PktBuf[6] = (u8)((Seq >> 16) & 0xFF);
    PktBuf[7] = (u8)((Seq >> 24) & 0xFF);

    if (!Dev->SendUdp(DstIp, DstPort, Dev->GetIp(), SrcPort,
            PktBuf, DgramHdrSize + textLen))
        return false;

    /* Only a datagram the device actually took gets a number, so a gap in
       the sequence at the collector means the network lost it. */
    Seq++;
    return true;
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

        /* First moment there is anywhere to send to. With nctail=N the boot
           log queued behind this point is cut down to the newest N KiB: on a
           machine that wedges seconds later, the whole network it will ever
           get goes on the lines nearest the wedge instead of on the head of
           the log, which is the part already understood. */
        if (!BacklogTrimmed)
        {
            ulong trimmed;
            {
                Stdlib::AutoLock lock(Lock);
                trimmed = Dropped;
                if (TailKeepBytes != 0)
                    TrimHead(Used, TailKeepBytes);
                trimmed = Dropped - trimmed;
                BacklogTrimmed = true;
            }

            if (trimmed != 0)
                Trace(0, "Netconsole: link up, dropped %u backlog msgs over the %u byte cap",
                    trimmed, TailKeepBytes);
        }

        ulong len;
        ulong consumed;
        ulong dropped;
        {
            Stdlib::AutoLock lock(Lock);
            len = PeekBatch(PktBuf + DgramHdrSize, sizeof(PktBuf) - DgramHdrSize, consumed);
            dropped = Dropped;
        }

        if (len == 0)
        {
            Sleep(IdlePollMs * Const::NanoSecsInMs);
            continue;
        }

        if (!SendBatch(len))
        {
            /* The records are still in the ring: on a machine whose only
               console is this one, consuming them first turned any TX hiccup
               into a silent blackout -- the loop has no sleep while the ring
               is non-empty, so it shredded the whole backlog at full speed and
               every line traced afterwards with it. Back off and retry. */
            TxFailed++;
            Sleep(TxRetryMs * Const::NanoSecsInMs);
            continue;
        }

        Sent++;

        {
            Stdlib::AutoLock lock(Lock);

            /* Append() may have evicted from the head while the lock was down,
               in which case what was just sent is already gone and popping
               again would eat live records. Dropped is the only thing that
               moves Head besides this task. */
            if (Dropped == dropped)
                PopBytes(nullptr, consumed);
        }

        /* Pace the next one. Sending a backlog as fast as the NIC takes it
           put tens of datagrams on the wire in a couple of milliseconds, and
           everything past the first few died in the narrowest queue on the
           way to the collector. */
        Sleep(SendPaceMs * Const::NanoSecsInMs);
    }
}

void Netconsole::PanicMark()
{
    if (!Enabled)
        return;

    /* Unlocked on purpose -- see the comment in Log(). */
    PanicBacklog = Used;
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

    /* The report is at the tail, behind whatever the drain task had not
       shipped yet. A machine that dies just after DHCP has the entire boot log
       in front of it -- far more than PanicMaxPackets carries -- so the one
       thing worth reading would never leave. Drop the old end of that backlog,
       keep a little for context. */
    ulong backlog = (PanicBacklog < Used) ? PanicBacklog : Used;
    TrimHead(backlog, PanicBacklogKeep);

    ulong failures = 0;
    for (ulong i = 0; i < PanicMaxPackets; i++)
    {
        /* Unlocked on purpose -- see the comment in Log(). */
        ulong consumed;
        ulong len = PeekBatch(PktBuf + DgramHdrSize, sizeof(PktBuf) - DgramHdrSize,
            consumed);
        if (len == 0)
            break;

        if (SendBatch(len))
        {
            Sent++;
            PopBytes(nullptr, consumed);
            failures = 0;
            continue;
        }

        /* Retry the same datagram: a TX ring that is only full drains while
           we spin here. Give up before the report is spent on a dead one. */
        TxFailed++;
        failures++;
        if (failures >= PanicMaxTxRetries)
            break;
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

    ulong used, dropped, sent, txFailed, seq;
    {
        Stdlib::AutoLock lock(Lock);
        used = Used;
        dropped = Dropped;
        sent = Sent;
        txFailed = TxFailed;
        seq = Seq;
    }

    printer.Printf("  buffered %u/%u bytes, dropped %u msgs, sent %u pkts, tx failed %u\n",
        used, (ulong)BufSize, dropped, sent, txFailed);
    printer.Printf("  next seq %u, backlog cap %u bytes%s\n",
        seq, TailKeepBytes, BacklogTrimmed ? ", applied" : "");
}

}
