#pragma once

#include <include/types.h>
#include <lib/stdlib.h>
#include <lib/printer.h>
#include "atomic.h"
#include "spin_lock.h"

namespace Kernel
{

class BlockDevice;

/* The kernel log, written to a raw disk area as each line is produced.
 *
 * This exists for one situation: a machine with no serial port, no working
 * network and therefore no netconsole, that stops somewhere in boot and says
 * nothing at all. The netconsole cannot help there -- it needs a NIC that
 * works and a link that is up, and by the time either exists the interesting
 * part is over.
 *
 * The writes are synchronous, one per line, straight from the tracer. That is
 * deliberate and it is the whole point: a drain task cannot help either,
 * because the scheduler does not exist until late in boot -- Rust driver
 * bring-up, where a hang is most likely, runs long before preemption is on.
 * Anything buffered for a task to write later is exactly what a hang loses.
 *
 * The cost is a sector write per traced line, which is why it is off unless
 * `disklog=on` is given. On NVMe a few thousand of them is a few tens of
 * milliseconds over a whole boot.
 *
 * WHERE IT WRITES, and why it will not eat a disk. The area is never guessed
 * and never searched for by "free space". A tool run under the host OS
 * (scripts/disklog.py) writes a header carrying a magic and a checksum to the
 * first sector of a partition set aside for this. At boot the kernel reads
 * the first sector of every block device it has and writes only where that
 * header is found intact. A disk that has not been prepared is not written
 * to, and a partition holding anything else does not carry the magic. */
class DiskLog final
{
public:
    static DiskLog& GetInstance()
    {
        static DiskLog instance;
        return instance;
    }

    /* Look for a prepared area on every registered block device. Called once
       the block drivers are up. Returns false when nothing is prepared, which
       is the normal case and not an error. */
    bool Setup();

    /* Append one line. Safe from any context: what cannot be written now is
       held until a call that can write flushes it. */
    void Log(const char* s);

    /* Push everything held, from the panic path. Best effort by
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
    void Stage(const char* s, ulong len);
    void Flush();
    static u32 HeaderCrc(const Header& hdr);

    /* Everything not yet on disk. Before the area is found that is the whole
       boot log, which is the part that matters and the part no other channel
       can carry; after, it is the handful of bytes since the last write.
       Sized like the netconsole backlog and for the same reason -- a real
       machine prints far more of a boot than QEMU does. */
    static const ulong PendingSize = 256 * 1024;

    static const ulong Tag = 'DLog';



    SpinLock Lock;

    BlockDevice* Dev;
    u64 AreaStartSector;   /* the header sector */
    u64 AreaSectors;
    u32 SectorSize;
    u64 BootSeq;

    volatile bool Enabled;

    /* Guards against a write path that traces: without it the first error
       inside WriteSectors would recurse until the stack ran out. */
    Atomic InFlush;

    u8 Pending[PendingSize];
    ulong PendingUsed;
    bool PendingOverflowed;

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
    u64 DroppedBytes;
};

}
