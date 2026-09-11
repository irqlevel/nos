#include "disklog.h"
#include "trace.h"
#include "panic.h"
#include "preempt.h"
#include "parameters.h"
#include "task.h"
#include "sched.h"
#include <hal/cpu.h>
#include <block/block_device.h>
#include <lib/checksum.h>
#include <mm/new.h>

namespace Kernel
{

/* The one rule this file exists to obey: nobody who traces ever waits for the
 * device. A line is queued without a lock -- a free slot, a copy, the ready
 * ring -- and the writes happen on the writer's side alone: the task, or
 * Setup() and Stop() in their own context, one at a time by InFlush. The
 * spinlock is Setup's and Dump's, and no I/O is done under it either:
 * AutoLock takes its interrupt-saving form, and a block write waits for a
 * completion interrupt from the device. */

DiskLog::DiskLog()
    : Dev(nullptr)
    , AreaStartSector(0)
    , AreaSectors(0)
    , SectorSize(0)
    , BootSeq(0)
    , Enabled(false)
    , Off(false)
    , TaskPtr(nullptr)
    , PendingUsed(0)
    , IoBuffer(nullptr)
    , HdrBuffer(nullptr)
    , Cursor(0)
    , Full(false)
    , SectorWrites(0)
    , WriteFailures(0)
    , DroppedBytes(0)
{
    InFlush.Set(0);
    DroppedLines.Set(0);

    /* Every slot starts free. Setup cannot fail on a power-of-two count, and
       Enqueue cannot on a ring sized to hold every slot there is. */
    if (!FreeRing.Setup(FreeCells, MsgCount) ||
        !ReadyRing.Setup(ReadyCells, MsgCount))
    {
        Off = true;
        return;
    }

    for (ulong i = 0; i < MsgCount; i++)
        FreeRing.Enqueue(&Msgs[i]);
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

/* The log will not be written this boot: queue nothing more for it. */
void DiskLog::SwitchOff()
{
    Stdlib::AutoLock lock(Lock);
    Off = true;
}

bool DiskLog::Setup()
{
    /* Asked for, or nothing at all -- not a disk read, not a buffer. An area
       outlives the debugging session it was prepared for, and finding one is
       no reason for every later boot to pay a forced write per line. */
    if (!Parameters::GetInstance().IsDiskLogOn())
    {
        SwitchOff();
        return false;
    }

    if (IoBuffer == nullptr)
    {
        IoBuffer = (u8*)Mm::Alloc(IoBufSize, Tag);
        HdrBuffer = (u8*)Mm::Alloc(MaxSectorSize, Tag);
        if (IoBuffer == nullptr || HdrBuffer == nullptr)
        {
            Trace(0, "DiskLog: no memory for the transfer buffers");
            SwitchOff();
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
           the boot task with interrupts on. */
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
            Off = true;
            Dev = nullptr;
            return false;
        }

        Trace(0, "DiskLog: %s, boot %u, %u sectors of %u bytes",
            dev->GetName(), (ulong)BootSeq, (ulong)AreaSectors,
            (ulong)SectorSize);

        /* The boot so far -- the whole ring, the line above with it -- goes
           down here, in this context and before Setup returns: a machine
           that stops right after still leaves all of it. */
        Flush();

        if (!StartTask())
        {
            Trace(0, "DiskLog: no writer task, the log on disk stops here");
            Flush();
            SwitchOff();
            return false;
        }

        return true;
    }

    /* Asked for and not found: worth a line, since whoever asked is about
       to go looking for a log that was never written. */
    SwitchOff();
    Trace(0, "DiskLog: disklog=on, but no prepared area on any disk");
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

/* A line into a free slot, and the slot onto the ready ring. No lock: this
   is the tracer's side, and it runs wherever a trace does -- interrupt
   handlers, code under a spinlock, the panic path. Preemption is held off
   across it so that a task is never switched away between claiming a ready
   cell and publishing it: the writer takes cells in order, and would wait
   behind that one for as long as the task stayed away. */
bool DiskLog::Enqueue(const char* s)
{
    Task* task = PreemptDisableTask();

    void* slot = nullptr;
    bool queued = FreeRing.Dequeue(slot);
    if (queued)
    {
        Msg* msg = static_cast<Msg*>(slot);
        Stdlib::StrnCpy(msg->Text, s, sizeof(msg->Text));

        /* Cannot fail: the ready ring holds every slot there is. */
        queued = ReadyRing.Enqueue(msg);
        if (!queued)
            FreeRing.Enqueue(msg);
    }

    if (!queued)
        DroppedLines.Inc();

    PreemptEnableTask(task);
    return queued;
}

/* Lines off the ready ring into Pending, as many as there is room for; true
   if any came. The writer's side only. */
bool DiskLog::Drain()
{
    bool drained = false;

    while (PendingSize - PendingUsed >= MsgSize)
    {
        void* slot = nullptr;
        if (!ReadyRing.Dequeue(slot))
            break;

        Msg* msg = static_cast<Msg*>(slot);
        ulong len = Stdlib::StrLen(msg->Text);
        Stdlib::MemCpy(&Pending[PendingUsed], msg->Text, len);
        PendingUsed += len;
        drained = true;

        /* Cannot fail either: the free ring holds every slot there is. */
        FreeRing.Enqueue(msg);
    }

    return drained;
}

/* Everything queued, onto the disk. The writer's side only: Flush() under
   InFlush, or PanicFlush() with the rest of the machine stopped. */
void DiskLog::WriteOut()
{
    for (;;)
    {
        bool drained = Drain();

        if (Full)
        {
            /* The area is used up. What is queued still has to leave the
               ring, or its slots would never come back. */
            DroppedBytes += PendingUsed;
            PendingUsed = 0;
            if (!drained)
                break;
            continue;
        }

        ulong n = PendingUsed;
        if (n > IoBufSize)
            n = IoBufSize;
        if (n == 0)
            break;

        ulong sectors = (n + SectorSize - 1) / SectorSize;
        u64 firstSector = AreaStartSector + 1 + Cursor / SectorSize;
        if (firstSector + sectors > AreaStartSector + AreaSectors)
        {
            Full = true;
            continue;
        }

        Stdlib::MemCpy(IoBuffer, Pending, n);
        Stdlib::MemSet(&IoBuffer[n], 0, sectors * SectorSize - n);

        /* Forced to media: the point of this is to survive a machine that
           stops immediately afterwards, and a write sitting in a cache does
           not. */
        if (!Dev->WriteSectors(firstSector, IoBuffer, (u32)sectors, true))
        {
            WriteFailures++;
            break;
        }

        SectorWrites += sectors;

        /* Only whole sectors are retired. The tail of a partial one stays
           staged and is written again next time, which is what makes the last
           few lines before a hang appear on disk at all -- and a round that
           retired nothing would only write the same partial sector again. */
        ulong retire = (n / SectorSize) * SectorSize;
        if (retire == 0)
            break;

        Cursor += retire;
        PendingUsed -= retire;
        if (PendingUsed != 0)
            Stdlib::MemCpy(Pending, &Pending[retire], PendingUsed);

        /* Keep the length on disk in step with what is there. The reader can
           find the end without it -- the area is zeroed and the text is not --
           but a header that agrees is the difference between a tool that has
           to guess and one that knows. */
        WriteHeader();
    }
}

/* Push what is queued to the device, from the writer's side. */
void DiskLog::Flush()
{
    if (!Enabled || Dev == nullptr)
        return;

    /* One writer at a time. Losing the race costs nothing: the winner drains
       the ring, and a line queued behind its last look is the next wake's. */
    if (InFlush.Cmpxchg(1, 0) != 0)
        return;

    WriteOut();

    InFlush.Set(0);
}

bool DiskLog::StartTask()
{
    Task* task = Mm::TAlloc<Task, Tag>("disklog");
    if (task == nullptr)
        return false;

    /* Published before it runs (see TaskPtr). */
    TaskPtr = task;

    if (!task->Start(&DiskLog::TaskFunc, this))
    {
        TaskPtr = nullptr;
        task->Put();
        return false;
    }

    return true;
}

void DiskLog::StopTask()
{
    Task* task = TaskPtr;
    if (task == nullptr)
        return;

    /* Unpublished first, so no new line reaches for a task on its way out. */
    TaskPtr = nullptr;

    task->SetStopping();
    task->Unblock();
    task->Wait();
    task->Put();
}

void DiskLog::TaskFunc(void* ctx)
{
    static_cast<DiskLog*>(ctx)->Run();
}

void DiskLog::Run()
{
    Task* task = Task::GetCurrentTask();

    while (!task->IsStopping())
    {
        Flush();

        /* Nothing queued: out of the scheduler's way until Log() has a line.
           The handshake is SoftIrq::Run's: the flag goes up first and the
           ring is looked at only after, while Log() queues first and clears
           the flag only after -- a line landing in between is either seen
           here or has cleared the flag before Schedule() can act on it.
           Count() is a snapshot, and that is all the handshake needs: a cell
           claimed and not yet published counts as work, and the wake that
           follows its publishing is the one that matters. */
        task->Block();
        if (ReadyRing.Count() == 0 && !task->IsStopping())
            Schedule();
        task->Unblock();
    }

    /* What arrived while it was being stopped. */
    Flush();
}

void DiskLog::Log(const char* s)
{
    /* Switched off, or the area used up: nothing to queue for. Read without
       the lock -- each is set once, and a line that slips past the flip is
       only queued for a writer that drops it. */
    if (Off || Full || s == nullptr || s[0] == '\0')
        return;

    /* Once the command line has been read, only for disklog=on. Before it
       every line is kept: nobody knows yet whether it is wanted, and the
       first lines are part of the boot the area is meant to hold. */
    auto& params = Parameters::GetInstance();
    if (params.IsParsed() && !params.IsDiskLogOn())
        return;

    Task* writer = TaskPtr;
    if (writer != nullptr && Task::TryGetCurrentTask() == writer)
        return;

    if (!Enqueue(s))
        return;

    if (!Enabled || Panicker::GetInstance().IsActive())
        return;

    /* Queued; now to whoever writes it. With the scheduler running that is
       the task -- a single atomic bit, safe from any context, and the task
       runs at its CPU's next scheduling point. Before there is a task,
       Setup() is about to write the ring itself. */
    if (PreemptIsOn())
    {
        if (writer != nullptr)
            writer->Unblock();
        return;
    }

    /* No scheduler, so no task will ever run: a caller that may wait writes
       the line itself, and one that may not -- interrupts off -- leaves it
       queued for the next that can. */
    if (PreemptCanBlock())
        Flush();
}

void DiskLog::Stop()
{
    if (!Enabled || Off)
        return;

    /* The task writes what is queued on its way out, and what arrives after
       is written here. Then off: nothing may wait on a block write once the
       soft IRQs are gone. */
    StopTask();
    Flush();
    SwitchOff();
}

void DiskLog::PanicFlush()
{
    if (!Enabled || Dev == nullptr || Off)
        return;

    /* No InFlush: every other CPU has been sent the halting IPI by now, and a
       flag one of them died holding -- the task stopped mid-write, say --
       must not keep the report off the disk. Interrupts are off here, so the
       device write may not complete -- best effort, and the console already
       has the report. A line whose producer was stopped between claiming its
       cell and publishing it holds up the ring behind it; what came before
       still goes. */
    WriteOut();
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
        if (!Parameters::GetInstance().IsDiskLogOn())
        {
            printer.Printf("disklog: off -- boot with disklog=on to write the "
                "log to a prepared area\n");
            return;
        }

        printer.Printf("disklog: no prepared area found\n");
        printer.Printf("  %u lines queued, %u dropped\n",
            ReadyRing.Count(), (ulong)DroppedLines.Get());
        return;
    }

    /* The writer's counters are read as they stand, without its InFlush:
       for a report, a value a moment old is as good as any. */
    printer.Printf("disklog: %s, boot %u, %u sectors of %u bytes\n",
        Dev->GetName(), (ulong)BootSeq, (ulong)AreaSectors, (ulong)SectorSize);
    printer.Printf("  on disk %u bytes, staged %u, queued %u, sector writes %u\n",
        (ulong)Cursor, (ulong)PendingUsed, ReadyRing.Count(),
        (ulong)SectorWrites);
    printer.Printf("  failures %u, dropped %u lines and %u bytes, full %u, "
        "off %u\n", (ulong)WriteFailures, (ulong)DroppedLines.Get(),
        (ulong)DroppedBytes, (ulong)(Full ? 1 : 0), (ulong)(Off ? 1 : 0));
}

}
