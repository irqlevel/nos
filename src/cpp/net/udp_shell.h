#pragma once

#include <include/types.h>
#include <lib/printer.h>
#include <lib/stdlib.h>
#include <net/net_device.h>
#include <net/net.h>
#include <kernel/spin_lock.h>
#include <kernel/task.h>

namespace Kernel
{

class UdpPrinter : public Stdlib::Printer
{
public:
    UdpPrinter();

    virtual void Printf(const char *fmt, ...) override;
    virtual void VPrintf(const char *fmt, va_list args) override;
    virtual void PrintString(const char *s) override;
    virtual void Backspace() override;

    /* Stamp the truncation marker if anything was dropped. Call once, after
       the command has finished printing and before the reply goes out: a
       silently short reply reads as a command that produced nothing more,
       which is how a truncated dmesg passes for a complete one. */
    void Finish();

    const u8* GetData() const { return Buf; }
    ulong GetLen() const { return Pos; }
    void Reset() { Pos = 0; Truncated = false; }

    static const ulong BufSize = 4096;

private:
    u8 Buf[BufSize];
    ulong Pos;
    bool Truncated;
};

struct UdpShellHdr
{
    u32 Magic;
    u32 SeqNo;
    u16 ChunkIdx;
    u16 Flags;
    u16 PayloadLen;
    u16 Reserved;
} __attribute__((packed));

static_assert(sizeof(UdpShellHdr) == 16, "Invalid size");

static const u32 UdpShellMagic = 0x4E4F5348; /* "NOSH" */
static const u16 UdpShellFlagLast = 0x0001;

class UdpShell
{
public:
    UdpShell();
    ~UdpShell();

    bool Start(NetDevice* dev, u16 port);
    void Stop();

private:
    UdpShell(const UdpShell& other) = delete;
    UdpShell(UdpShell&& other) = delete;
    UdpShell& operator=(const UdpShell& other) = delete;
    UdpShell& operator=(UdpShell&& other) = delete;

    static void TaskFunc(void* ctx);
    void Run();

    static void RxCallbackFn(const u8* frame, ulong len, void* ctx);

    NetDevice* Dev;
    Task* TaskPtr;
    u16 Port;

    /* RX buffer for incoming commands */
    static const ulong RxBufMaxLen = 1500;
    u8 RxBuf[RxBufMaxLen];
    ulong RxBufLen;
    bool RxBufReady;
    SpinLock RxLock;

    /* Sender info for reply */
    Net::MacAddress SenderMac;
    Net::IpAddress SenderIp;
    u16 SenderPort;
    u32 RxSeqNo;

    static const ulong Tag = 'UdpS';
};

}
