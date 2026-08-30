#pragma once

#include <include/types.h>
#include <lib/stdlib.h>
#include <lib/printer.h>
#include <net/net.h>
#include <net/net_device.h>
#include <kernel/spin_lock.h>
#include <kernel/task.h>

namespace Kernel
{

/* Kernel log over UDP ("netconsole=ip:port").
 *
 * Every line the tracer produces is captured into a ring buffer the moment it
 * is produced -- from any context, including IRQ -- and a drain task ships it
 * to the configured collector. While the network is down (no device, no IP)
 * lines simply accumulate; the ring keeps the newest ones and counts what it
 * had to drop. As soon as the device has an IP the whole backlog goes out, and
 * after that every new line follows within a couple of milliseconds.
 *
 * Messages produced by the drain task itself are not captured: the TX path
 * traces (ARP failures, driver errors), and feeding those back into the ring
 * would make the drain loop generate its own work forever. They still reach
 * dmesg and the console.
 */
class Netconsole final
{
public:
    static Netconsole& GetInstance()
    {
        static Netconsole Instance;
        return Instance;
    }

    /* Arm capture from the netconsole= kernel parameter. Safe to call long
       before the network exists; primes the ring with what dmesg already has,
       so lines traced before this point are not lost. */
    bool Setup();

    /* Attach a device and start the drain task. */
    bool Start(NetDevice* dev);
    void Stop();

    /* Capture hook: called from Tracer::Output for every message, and from
       the panic printer. Must be safe at any IRQ level. */
    void Log(const char* s);

    /* Called once a panic has started, before anything is printed: remembers
       how much undrained backlog sits in front of the report so PanicFlush()
       can get past it. */
    void PanicMark();

    /* Best-effort synchronous drain from panic context: no locks, and only if
       the collector MAC is already in the ARP cache (a blocking ARP resolve
       would never complete with the other CPUs halted). */
    void PanicFlush();

    bool IsEnabled();

    void Dump(Stdlib::Printer& printer);

private:
    Netconsole();
    ~Netconsole();

    Netconsole(const Netconsole& other) = delete;
    Netconsole(Netconsole&& other) = delete;
    Netconsole& operator=(const Netconsole& other) = delete;
    Netconsole& operator=(Netconsole&& other) = delete;

    static void TaskFunc(void* ctx);
    void Run();

    /* Ring helpers: the caller holds Lock, or is the panic path, which runs
       with every other CPU halted. */
    void PushBytes(const u8* src, ulong len);
    void PopBytes(u8* dst, ulong len);
    bool PeekRecordLen(u16& len);
    void DropOldest();
    void Append(const char* s, ulong len);

    /* Fill buf with as many whole records as fit, without consuming them;
       returns bytes filled and, in consumed, how many ring bytes they occupy.
       The caller pops that many once the datagram is actually out. */
    ulong PeekBatch(u8* buf, ulong bufSize, ulong& consumed);

    bool SendBatch(const u8* buf, ulong len);

    /* A record is a 2-byte length prefix followed by that many payload bytes.
       Either may straddle the wrap, so both go through PushBytes/PopBytes. */
    static const ulong RecordHdrSize = 2;
    static const ulong MaxRecordLen = 512;

    /* Enough for a full boot log, so a collector started late still gets it. */
    static const ulong BufSize = 128 * 1024;

    /* One UDP datagram of log text; stays well under the 1500-byte MTU. */
    static const ulong PayloadMaxLen = 1400;

    /* Poll interval of the drain task while the ring is empty, and while the
       device has no IP yet. */
    static const ulong IdlePollMs = 2;
    static const ulong NoLinkPollMs = 200;

    /* Backoff after a datagram the device refused. The records stay in the
       ring, so this is how fast a wedged TX path is retried -- without it the
       drain loop spins a CPU and shreds the backlog. */
    static const ulong TxRetryMs = 20;

    u8 Buf[BufSize];
    ulong Head;      /* oldest byte */
    ulong Used;      /* bytes in the ring */

    SpinLock Lock;

    NetDevice* Dev;
    Task* TaskPtr;

    Net::IpAddress DstIp;
    u16 DstPort;
    u16 SrcPort;

    volatile bool Enabled;

    /* Stats, for the "netconsole" shell command. */
    ulong Dropped;     /* records evicted because the ring was full */
    ulong Sent;        /* datagrams sent */
    ulong TxFailed;    /* datagrams the device refused */

    /* Bytes of pre-panic backlog, sampled by PanicMark(). */
    ulong PanicBacklog;

    u8 PktBuf[PayloadMaxLen];

    static const ulong Tag = 'NetC';
};

}
