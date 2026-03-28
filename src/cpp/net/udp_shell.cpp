#include "udp_shell.h"
#include "net.h"

#include <kernel/trace.h>
#include <kernel/cmd.h>
#include <kernel/sched.h>
#include <mm/new.h>
#include <lib/stdlib.h>

namespace Kernel
{

using Net::EthHdr;
using Net::IpHdr;
using Net::UdpHdr;
using Net::Htons;
using Net::Htonl;
using Net::Ntohs;
using Net::Ntohl;

/* --- UdpPrinter --- */

UdpPrinter::UdpPrinter()
    : Pos(0)
{
    Stdlib::MemSet(Buf, 0, sizeof(Buf));
}

void UdpPrinter::Printf(const char *fmt, ...)
{
    va_list args;
    va_start(args, fmt);
    VPrintf(fmt, args);
    va_end(args);
}

void UdpPrinter::VPrintf(const char *fmt, va_list args)
{
    if (Pos >= BufSize)
        return;

    char tmp[256];
    int n = Stdlib::VsnPrintf(tmp, sizeof(tmp), fmt, args);
    if (n < 0)
        return;

    ulong len = (ulong)n;
    if (Pos + len > BufSize)
        len = BufSize - Pos;

    Stdlib::MemCpy(Buf + Pos, tmp, len);
    Pos += len;
}

void UdpPrinter::PrintString(const char *s)
{
    if (s == nullptr || Pos >= BufSize)
        return;

    ulong len = Stdlib::StrLen(s);
    if (Pos + len > BufSize)
        len = BufSize - Pos;

    Stdlib::MemCpy(Buf + Pos, s, len);
    Pos += len;
}

void UdpPrinter::Backspace()
{
    /* no-op for UDP */
}

/* --- UdpShell --- */

UdpShell::UdpShell()
    : Dev(nullptr)
    , TaskPtr(nullptr)
    , Port(0)
    , RxBufLen(0)
    , RxBufReady(false)
    , SenderPort(0)
    , RxSeqNo(0)
{
}

UdpShell::~UdpShell()
{
    Stop();
}

bool UdpShell::Start(NetDevice* dev, u16 port)
{
    if (!dev || TaskPtr || port == 0)
        return false;

    Dev = dev;
    Port = port;
    RxBufReady = false;

    TaskPtr = Mm::TAlloc<Task, Tag>("udpsh");
    if (!TaskPtr)
        return false;

    if (!TaskPtr->Start(&UdpShell::TaskFunc, this))
    {
        TaskPtr->Put();
        TaskPtr = nullptr;
        return false;
    }

    Dev->RegisterUdpListener(Port, RxCallbackFn, this);

    Trace(0, "UdpShell: started on port %u", (ulong)Port);
    return true;
}

void UdpShell::Stop()
{
    if (Dev && Port)
    {
        Dev->UnregisterUdpListener(Port);
    }
    if (TaskPtr)
    {
        TaskPtr->SetStopping();
        TaskPtr->Wait();
        TaskPtr->Put();
        TaskPtr = nullptr;
    }
    Dev = nullptr;
    Port = 0;
}

void UdpShell::TaskFunc(void* ctx)
{
    UdpShell* shell = static_cast<UdpShell*>(ctx);
    shell->Run();
}

void UdpShell::Run()
{
    auto* task = Task::GetCurrentTask();

    while (!task->IsStopping())
    {
        bool ready = false;
        {
            Stdlib::AutoLock lock(RxLock);
            ready = RxBufReady;
        }

        if (!ready)
        {
            Sleep(10 * Const::NanoSecsInMs);
            continue;
        }

        /* Extract command and sender info under lock */
        char cmd[256];
        Net::IpAddress senderIp;
        u16 senderPort;
        u32 seqNo;

        {
            Stdlib::AutoLock lock(RxLock);
            ulong len = RxBufLen;
            if (len >= sizeof(cmd))
                len = sizeof(cmd) - 1;
            Stdlib::MemCpy(cmd, RxBuf, len);
            cmd[len] = '\0';
            senderIp = SenderIp;
            senderPort = SenderPort;
            seqNo = RxSeqNo;
            RxBufReady = false;
        }

        /* Strip trailing newline/CR */
        ulong cmdLen = Stdlib::StrLen(cmd);
        while (cmdLen > 0 && (cmd[cmdLen - 1] == '\n' || cmd[cmdLen - 1] == '\r'))
        {
            cmd[cmdLen - 1] = '\0';
            cmdLen--;
        }

        if (cmdLen == 0)
            continue;

        Trace(0, "UdpShell: cmd '%s' from %p:%u",
            cmd, (ulong)Ntohl(senderIp.ToNetwork()), (ulong)senderPort);

        /* Execute command */
        UdpPrinter printer;
        Cmd::Dispatch(cmd, printer);

        /* Send reply in chunks with protocol header */
        const u8* data = printer.GetData();
        ulong remaining = printer.GetLen();
        ulong offset = 0;
        static const ulong ChunkSize = 1384;
        u16 chunkIdx = 0;

        if (remaining == 0)
        {
            /* Empty output: send header-only packet with LAST flag */
            UdpShellHdr hdr;
            hdr.Magic = Net::Htonl(UdpShellMagic);
            hdr.SeqNo = seqNo; /* already in network byte order */
            hdr.ChunkIdx = Net::Htons(0);
            hdr.Flags = Net::Htons(UdpShellFlagLast);
            hdr.PayloadLen = 0;
            hdr.Reserved = 0;

            Dev->SendUdp(senderIp, senderPort,
                         Dev->GetIp(), Port,
                         &hdr, sizeof(hdr));
        }

        while (remaining > 0)
        {
            ulong chunk = remaining;
            if (chunk > ChunkSize)
                chunk = ChunkSize;

            bool last = (chunk == remaining);

            u8 sendBuf[sizeof(UdpShellHdr) + ChunkSize];
            UdpShellHdr* hdr = (UdpShellHdr*)sendBuf;
            hdr->Magic = Net::Htonl(UdpShellMagic);
            hdr->SeqNo = seqNo; /* already in network byte order */
            hdr->ChunkIdx = Net::Htons(chunkIdx);
            hdr->Flags = last ? Net::Htons(UdpShellFlagLast) : 0;
            hdr->PayloadLen = Net::Htons((u16)chunk);
            hdr->Reserved = 0;

            Stdlib::MemCpy(sendBuf + sizeof(UdpShellHdr), data + offset, chunk);

            Dev->SendUdp(senderIp, senderPort,
                         Dev->GetIp(), Port,
                         sendBuf, sizeof(UdpShellHdr) + chunk);

            offset += chunk;
            remaining -= chunk;
            chunkIdx++;
        }
    }
}

void UdpShell::RxCallbackFn(const u8* frame, ulong len, void* ctx)
{
    UdpShell* shell = static_cast<UdpShell*>(ctx);

    if (len < sizeof(EthHdr) + sizeof(IpHdr) + sizeof(UdpHdr))
        return;

    const EthHdr* eth = (const EthHdr*)frame;
    const IpHdr* ip = (const IpHdr*)(frame + sizeof(EthHdr));
    const UdpHdr* udp = (const UdpHdr*)(frame + sizeof(EthHdr) + sizeof(IpHdr));

    ulong hdrLen = sizeof(EthHdr) + sizeof(IpHdr) + sizeof(UdpHdr);
    if (len <= hdrLen)
        return;

    const u8* payload = frame + hdrLen;
    ulong payloadLen = len - hdrLen;

    /* Validate UdpShell protocol header */
    if (payloadLen < sizeof(UdpShellHdr))
        return;

    const UdpShellHdr* shHdr = (const UdpShellHdr*)payload;

    if (Ntohl(shHdr->Magic) != UdpShellMagic)
        return;

    u16 declaredLen = Ntohs(shHdr->PayloadLen);
    if (declaredLen > payloadLen - sizeof(UdpShellHdr))
        return;

    Stdlib::AutoLock lock(shell->RxLock);

    if (shell->RxBufReady)
        return; /* drop if previous command still pending */

    ulong cmdLen = declaredLen;
    if (cmdLen > UdpShell::RxBufMaxLen)
        cmdLen = UdpShell::RxBufMaxLen;

    Stdlib::MemCpy(shell->RxBuf, payload + sizeof(UdpShellHdr), cmdLen);
    shell->RxBufLen = cmdLen;
    shell->RxSeqNo = shHdr->SeqNo; /* keep in network byte order, echo back as-is */
    shell->SenderMac = Net::MacAddress(eth->SrcMac);
    shell->SenderIp = Net::IpAddress::FromNetwork(ip->SrcAddr);
    shell->SenderPort = Ntohs(udp->SrcPort);
    shell->RxBufReady = true;
}

}
