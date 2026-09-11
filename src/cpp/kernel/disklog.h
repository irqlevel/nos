#pragma once

#include <include/types.h>
#include <lib/stdlib.h>
#include <lib/printer.h>
#include "atomic.h"
#include "spin_lock.h"
#include "lockless_ring.h"

namespace Kernel
{

class BlockDevice;
class Task;

/* The kernel log, written to a raw disk area as each line is produced.
 *
 * This exists for one situation: a machine with no serial port, no working
 * network and therefore no netconsole, that stops somewhere in boot and says
 * nothing at all. The netconsole cannot help there -- it needs a NIC that
 * works and a link that is up, and by the time either exists the interesting
 * part is over.
 *
 * HOW A LINE GETS THERE. The tracer hands every line to Log() from whatever
 * context it runs in -- an interrupt handler, code under a spinlock, the
 * panic path -- so Log() takes no lock and never waits: the line goes into a
 * free slot, the slot onto a ready ring (kernel/lockless_ring.h), and that is
 * all. Until the area is known that ring is the boot log so far, and Setup()
 * writes all of it before it returns. From then on a task of its own does the
 * writing, woken by each line: a write waits for the device -- one line has
 * cost 25-33 ms on real hardware -- and whoever traced, the receive path or
 * a lock holder, is no place to wait that long. With no scheduler to hand a
 * line to, a caller that can wait writes it itself.
 *
 * The price of the task is the last moments before a hang: a line traced
 * just before a machine stops dead may still be in the ring. A panic pushes
 * out whatever is queued, the report with it.
 *
 * Every burst of lines is still a forced write, which is why the whole thing
 * is off unless `disklog=on` is given: a machine that merely has an area
 * prepared is not made to pay for it on every boot.
 *
 * WHERE IT WRITES, and why it will not eat a disk. The area is never guessed
 * and never searched for by "free space". A tool run under the host OS
 * (scripts/disklog.py) writes a header carrying a magic and a checksum to the
 * first sector of a partition set aside for this. At boot, given disklog=on,
 * the kernel reads the first sector of every block device it has and writes
 * only where that header is found intact. A disk that has not been prepared
 * is not written to, a partition holding anything else does not carry the
 * magic, and without disklog=on no disk is so much as read. */
class DiskLog final
{
public:
    static DiskLog& GetInstance()
    {
        static DiskLog instance;
        return instance;
    }

    /* Given disklog=on, look for a prepared area on every registered block
       device. Called once the block drivers are up. When one is found the
       boot so far is written before this returns, and the writer task takes
       over. Returns false -- and the log is off for the rest of the boot,
       nothing more queued -- when the parameter is not given or nothing is
       prepared, which is the normal case and not an error. */
    bool Setup();

    /* Append one line. Safe from any context, and never waits: the line is
       queued for the writer. Nothing is queued once the command line has
       been read without disklog=on, or once the log has been switched off. */
    void Log(const char* s);

    /* On the way down, before the soft IRQs stop: the writer finishes what is
       queued and exits, and the log switches off -- after SoftIrq::Stop() a
       write through a virtio disk, which completes by soft IRQ, would wait
       for ever. */
    void Stop();

    /* Push everything queued, from the panic path. Best effort by
       construction -- the machine is going down either way. */
    void PanicFlush();

    bool IsEnabled();
    void Dump(Stdlib::Printer& printer);

    /* On-disk header, first sector of the area. Little-endian, and the layout
       scripts/disklog.py writes and reads. */
    static const u64 Magic = 0x31474F4C534F4EULL; /* "NOSLOG1" */
    static const u32 Version = 1;

    static const ulong MaxSectorSize = 4096;

    /* One page per transfer. Not a tuning choice: a DMA buffer has to be
       physically contiguous, and one page is the largest block the allocator
       guarantees that for. The flush loop makes as many trips as it needs. */
    static const ulong IoBufSize = 4096;

    struct Header
    {
        u64 Magic;
        u32 Version;
        u32 SectorSize;
        u64 AreaSectors;  /* header sector included */
        u64 BootSeq;      /* bumped by the kernel on every boot */
        u64 LogBytes;     /* valid bytes of text following the header */
        u32 Crc;          /* over everything above */
        u32 Reserved;
    };

    /* Where Crc sits, so the checksum never covers itself. Spelled out
       rather than taken with offsetof, which would want a header the
       freestanding build does not otherwise need. */
    static const ulong CrcOffset = 40;

private:
    DiskLog();
    ~DiskLog();
    DiskLog(const DiskLog& other) = delete;
    DiskLog(DiskLog&& other) = delete;
    DiskLog& operator=(const DiskLog& other) = delete;
    DiskLog& operator=(DiskLog&& other) = delete;

    bool ReadHeader(BlockDevice* dev, Header& hdr);
    bool WriteHeader();
    bool Enqueue(const char* s);
    bool Drain();
    void WriteOut();
    void Flush();
    bool StartTask();
    void StopTask();
    void Run();
    static void TaskFunc(void* ctx);
    void SwitchOff();
    static u32 HeaderCrc(const Header& hdr);

    /* A line as the tracer makes it: Tracer::Output formats into 256 bytes,
       and anything longer is cut to fit. */
    static const ulong MsgSize = 256;

    /* Lines the writer has not taken yet. Before the area is found that is
       the whole boot log, which is the part that matters and the part no
       other channel can carry -- and a real machine prints far more of a
       boot than QEMU does. A slot per line, since the ring carries
       pointers. */
    static const ulong MsgCount = 2048;
    static_assert((MsgCount & (MsgCount - 1)) == 0,
        "LocklessRing wants a power of two");

    /* The writer's own staging: lines off the ring, and the tail of a sector
       written only in part. Room for a transfer and a line over. */
    static const ulong PendingSize = 2 * IoBufSize;

    static const ulong Tag = 'DLog';

    struct Msg
    {
        char Text[MsgSize];
    };

    /* Setup's and Dump's; neither the tracer nor the writer takes it. */
    SpinLock Lock;

    BlockDevice* Dev;
    ulong DevClaim = 0;    /* BlockDeviceTable::Claim's, on Dev */
    u64 AreaStartSector;   /* the header sector */
    u64 AreaSectors;
    u32 SectorSize;
    u64 BootSeq;

    volatile bool Enabled;

    /* Set once the log will not be written this boot: disklog=on not given,
       no prepared area, no writer task, or Stop(). Log() queues nothing
       after that. */
    volatile bool Off;

    /* One writer at a time: the task, Setup() catching up, Stop() finishing,
       or a caller with no scheduler to hand its line to. */
    Atomic InFlush;

    /* Published before the task first runs. Log() wakes it through this, and
       leaves out the lines the task itself produces: an error on the write
       path traces, and writing that line would fail and trace again. */
    Task* TaskPtr;

    Msg Msgs[MsgCount];
    LocklessRing::Cell FreeCells[MsgCount];
    LocklessRing::Cell ReadyCells[MsgCount];
    LocklessRing FreeRing;    /* empty slots */
    LocklessRing ReadyRing;   /* filled ones, in the order they were queued */

    u8 Pending[PendingSize];
    ulong PendingUsed;

    /* DMA targets, from the page allocator and not static arrays. A driver
       hands the buffer's physical address to the device, and only memory the
       allocator tracks has one it can find: a read into a .bss array comes
       back reporting success with the buffer untouched, which is a worse
       failure than an error would be and cost an afternoon to see. */
    u8* IoBuffer;
    u8* HdrBuffer;

    u64 Cursor;            /* bytes of text on disk; always a whole number of
                              sectors, so Pending starts exactly where the
                              next sector does */
    bool Full;

    /* Stats for the shell command. */
    u64 SectorWrites;
    u64 WriteFailures;
    u64 DroppedBytes;      /* the writer's: past the end of the area */
    Atomic DroppedLines;   /* the tracer's: no free slot */
};

}
