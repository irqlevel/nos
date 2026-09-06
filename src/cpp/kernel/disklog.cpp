#include "disklog.h"
#include "trace.h"
#include "panic.h"
#include <hal/cpu.h>
#include <block/block_device.h>
#include <lib/checksum.h>
#include <mm/new.h>

namespace Kernel
{

/* The one rule this file exists to obey: no device I/O while the lock is
 * held. AutoLock takes the interrupt-saving form of SpinLock, so holding it
 * means interrupts are off -- and a block write waits for a completion
 * interrupt from the device. Doing both at once is a machine that stops with
 * no output, which is precisely the failure this code was written to
 * diagnose. The lock covers the staging buffer and nothing else; the writes
 * happen outside it, serialised by InFlush. */

DiskLog::DiskLog()
    : Dev(nullptr)
    , AreaStartSector(0)
    , AreaSectors(0)
    , SectorSize(0)
    , BootSeq(0)
    , Enabled(false)
    , PendingUsed(0)
    , PendingOverflowed(false)
    , IoBuffer(nullptr)
    , HdrBuffer(nullptr)
    , Cursor(0)
    , Full(false)
    , SectorWrites(0)
    , WriteFailures(0)
    , DroppedBytes(0)
{
    InFlush.Set(0);
}

DiskLog::~DiskLog()
{
}

u32 DiskLog::HeaderCrc(const Header& hdr)
{
    static_assert(sizeof(Header) == 48, "DiskLog::Header layout changed");
    return Stdlib::Crc32(&hdr, CrcOffset);
}

bool DiskLog::ReadHeader(BlockDevice* dev, Header& hdr)
{
    u32 sectorSize = (u32)dev->GetSectorSize();
    if (sectorSize < sizeof(Header) || sectorSize > MaxSectorSize)
        return false;

    if (!dev->ReadSectors(0, IoBuffer, 1))
        return false;

    Stdlib::MemCpy(&hdr, IoBuffer, sizeof(hdr));

    if (hdr.Magic != Magic || hdr.Version != Version)
        return false;

    if (hdr.SectorSize != sectorSize)
        return false;

    /* The area must fit the device and hold more than its own header. */
    if (hdr.AreaSectors < 2 || hdr.AreaSectors > dev->GetCapacity())
        return false;

    return hdr.Crc == HeaderCrc(hdr);
}

bool DiskLog::Setup()
{
    if (IoBuffer == nullptr)
    {
        IoBuffer = (u8*)Mm::Alloc(IoBufSize, Tag);
        HdrBuffer = (u8*)Mm::Alloc(MaxSectorSize, Tag);
        if (IoBuffer == nullptr || HdrBuffer == nullptr)
        {
            Trace(0, "DiskLog: no memory for the transfer buffers");
            return false;
        }
    }

    auto& table = BlockDeviceTable::GetInstance();

    for (ulong i = 0; i < table.GetCount(); i++)
    {
        BlockDevice* dev = table.GetDevice(i);
        if (dev == nullptr)
            continue;

        /* Reading here is safe and not under the lock: this runs once, in
           task-less early boot with interrupts on. */
        Header hdr;
        if (!ReadHeader(dev, hdr))
            continue;

        {
            Stdlib::AutoLock lock(Lock);
            Dev = dev;
            AreaStartSector = 0;
            AreaSectors = hdr.AreaSectors;
            SectorSize = hdr.SectorSize;
            BootSeq = hdr.BootSeq + 1;
            Cursor = 0;
            Full = false;
            Enabled = true;
        }

        /* The header goes down before any text, so a machine that stops on
           the very next line still leaves a readable area rather than the
           previous boot's text under a stale length. */
        if (!WriteHeader())
        {
            Stdlib::AutoLock lock(Lock);
            Enabled = false;
            Dev = nullptr;
            return false;
        }

        /* This traces, which stages a line, which is then flushed along with
           everything held from before the area was known. */
        Trace(0, "DiskLog: %s, boot %u, %u sectors of %u bytes",
            dev->GetName(), (ulong)BootSeq, (ulong)AreaSectors,
            (ulong)SectorSize);

        Flush();
        return true;
    }

    return false;
}

bool DiskLog::WriteHeader()
{
    if (Dev == nullptr || SectorSize == 0)
        return false;

    Header hdr;
    Stdlib::MemSet(&hdr, 0, sizeof(hdr));
    hdr.Magic = Magic;
    hdr.Version = Version;
    hdr.SectorSize = SectorSize;
    hdr.AreaSectors = AreaSectors;
    hdr.BootSeq = BootSeq;
    hdr.LogBytes = Cursor;
    hdr.Crc = HeaderCrc(hdr);

    Stdlib::MemSet(HdrBuffer, 0, SectorSize);
    Stdlib::MemCpy(HdrBuffer, &hdr, sizeof(hdr));

    if (!Dev->WriteSectors(AreaStartSector, HdrBuffer, 1, true))
    {
        WriteFailures++;
        return false;
    }
    return true;
}

/* Add bytes to the staging buffer. Called with the lock held; does no I/O. */
void DiskLog::Stage(const char* s, ulong len)
{
    if (Full)
    {
        DroppedBytes += len;
        return;
    }

    if (PendingUsed + len > PendingSize)
    {
        /* Before the area is known this means a boot that printed more than
           the buffer holds; afterwards it means the device is not keeping up.
           Either way the newest lines are the ones worth having, but dropping
           from the front would tear a line in half -- so the oldest go and the
           loss is counted. */
        PendingOverflowed = true;
        DroppedBytes += len;
        return;
    }

    Stdlib::MemCpy(&Pending[PendingUsed], s, len);
    PendingUsed += len;
}

/* Push staged bytes to the device. Takes no lock across I/O. */
void DiskLog::Flush()
{
    if (!Enabled || Dev == nullptr || Full)
        return;

    /* One writer at a time. A line that arrives while this runs is staged by
       its own caller and picked up here or by the next flush -- nothing is
       lost by losing the race. */
    if (InFlush.Cmpxchg(1, 0) != 0)
        return;

    for (;;)
    {
        ulong n;
        u64 base;

        {
            Stdlib::AutoLock lock(Lock);
            n = PendingUsed;
            if (n > IoBufSize)
                n = IoBufSize;
            if (n == 0)
                break;
            base = Cursor;
            Stdlib::MemCpy(IoBuffer, Pending, n);
        }

        ulong sectors = (n + SectorSize - 1) / SectorSize;
        ulong padded = sectors * SectorSize;
        Stdlib::MemSet(&IoBuffer[n], 0, padded - n);

        u64 firstSector = AreaStartSector + 1 + base / SectorSize;
        if (firstSector + sectors > AreaStartSector + AreaSectors)
        {
            Stdlib::AutoLock lock(Lock);
            Full = true;
            break;
        }

        /* Forced to media: the point of this is to survive a machine that
           stops immediately afterwards, and a write sitting in a cache does
           not. No lock is held here, which is the whole design. */
        if (!Dev->WriteSectors(firstSector, IoBuffer, (u32)sectors, true))
        {
            Stdlib::AutoLock lock(Lock);
            WriteFailures++;
            break;
        }

        SectorWrites += sectors;

        /* Only whole sectors are retired. The tail of a partial one stays
           staged and is written again next time, which is what makes the last
           few lines before a hang appear on disk at all. */
        ulong retire = (n / SectorSize) * SectorSize;

        {
            Stdlib::AutoLock lock(Lock);
            if (retire != 0)
            {
                Cursor += retire;
                PendingUsed -= retire;
                if (PendingUsed != 0)
                    Stdlib::MemCpy(Pending, &Pending[retire], PendingUsed);
            }
            /* Nothing whole was retired, so another round would write the
               same partial sector again. */
            if (retire == 0)
                break;
        }

        /* Keep the length on disk in step with what is there. The reader can
           find the end without it -- the area is zeroed and the text is not --
           but a header that agrees is the difference between a tool that has
           to guess and one that knows. */
        WriteHeader();
    }

    InFlush.Set(0);
}

void DiskLog::Log(const char* s)
{
    if (s == nullptr)
        return;

    ulong len = Stdlib::StrLen(s);
    if (len == 0)
        return;

    {
        Stdlib::AutoLock lock(Lock);
        Stage(s, len);
    }

    if (!Enabled)
        return;

    /* A write waits for a completion interrupt, so a caller that already has
       them off cannot wait for one. The line is staged either way and the
       next caller that can write pushes it out -- which is why the partial
       sector is rewritten rather than held back. */
    if (!Hal::IsInterruptEnabled())
        return;

    if (Panicker::GetInstance().IsActive())
        return;

    Flush();
}

void DiskLog::PanicFlush()
{
    if (!Enabled || Dev == nullptr)
        return;

    /* No lock and no InFlush: every other CPU has been sent the halting IPI
       by now, and a flag one of them died holding must not stop the report
       reaching the disk. Interrupts are off here, so the device write may
       not complete -- best effort, and the console already has the report. */
    ulong n = PendingUsed;
    if (n > IoBufSize)
        n = IoBufSize;

    if (n != 0)
    {
        Stdlib::MemCpy(IoBuffer, Pending, n);
        ulong sectors = (n + SectorSize - 1) / SectorSize;
        Stdlib::MemSet(&IoBuffer[n], 0, sectors * SectorSize - n);

        u64 firstSector = AreaStartSector + 1 + Cursor / SectorSize;
        if (firstSector + sectors <= AreaStartSector + AreaSectors)
        {
            if (Dev->WriteSectors(firstSector, IoBuffer, (u32)sectors, true))
                Cursor += (n / SectorSize) * SectorSize;
        }
    }

    WriteHeader();
}

bool DiskLog::IsEnabled()
{
    return Enabled;
}

void DiskLog::Dump(Stdlib::Printer& printer)
{
    Stdlib::AutoLock lock(Lock);

    if (!Enabled)
    {
        printer.Printf("disklog: no prepared area found\n");
        printer.Printf("  %u bytes staged, overflowed %u\n",
            (ulong)PendingUsed, (ulong)(PendingOverflowed ? 1 : 0));
        return;
    }

    printer.Printf("disklog: %s, boot %u, %u sectors of %u bytes\n",
        Dev->GetName(), (ulong)BootSeq, (ulong)AreaSectors, (ulong)SectorSize);
    printer.Printf("  on disk %u bytes, staged %u, sector writes %u\n",
        (ulong)Cursor, (ulong)PendingUsed, (ulong)SectorWrites);
    printer.Printf("  failures %u, dropped %u, full %u\n",
        (ulong)WriteFailures, (ulong)DroppedBytes, (ulong)(Full ? 1 : 0));
}

}
