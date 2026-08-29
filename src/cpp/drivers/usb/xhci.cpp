#include "xhci.h"

#include <drivers/mmio.h>
#include <hal/barrier.h>
#include <kernel/input.h>
#include <kernel/sched.h>
#include <kernel/task.h>
#include <kernel/time.h>
#include <kernel/trace.h>
#include <lib/stdlib.h>
#include <mm/new.h>
#include <mm/page_table.h>

namespace Kernel
{
namespace Usb
{

/* ------------------------------------------------------------------ */
/* Register map (xHCI 1.2, chapter 5)                                  */
/* ------------------------------------------------------------------ */

/* Capability registers, at BAR0 + 0 */
static const ulong CapCapLength = 0x00;   /* u8 CAPLENGTH, u16 HCIVERSION at +2 */
static const ulong CapHcsParams1 = 0x04;
static const ulong CapHcsParams2 = 0x08;
static const ulong CapHccParams1 = 0x10;
static const ulong CapDbOff = 0x14;
static const ulong CapRtsOff = 0x18;

/* Operational registers, at BAR0 + CAPLENGTH */
static const ulong OpUsbCmd = 0x00;
static const ulong OpUsbSts = 0x04;
static const ulong OpPageSize = 0x08;
static const ulong OpDnCtrl = 0x14;
static const ulong OpCrcr = 0x18;
static const ulong OpDcbaap = 0x30;
static const ulong OpConfig = 0x38;
static const ulong OpPortScBase = 0x400;
static const ulong OpPortRegSize = 0x10;

static const u32 UsbCmdRun = 1u << 0;
static const u32 UsbCmdReset = 1u << 1;
static const u32 UsbCmdIntEnable = 1u << 2;
static const u32 UsbCmdHsErrEnable = 1u << 3;

static const u32 UsbStsHalted = 1u << 0;
static const u32 UsbStsHostSystemError = 1u << 2;
static const u32 UsbStsEventInt = 1u << 3;
static const u32 UsbStsPortChange = 1u << 4;
static const u32 UsbStsControllerNotReady = 1u << 11;
static const u32 UsbStsHostControllerError = 1u << 12;

/* Runtime registers, at BAR0 + RTSOFF. Interrupter 0 starts at +0x20. */
static const ulong RtInterrupter0 = 0x20;
static const ulong IrIman = 0x00;
static const ulong IrImod = 0x04;
static const ulong IrErstSz = 0x08;
static const ulong IrErstBa = 0x10;
static const ulong IrErdp = 0x18;

static const u32 ImanInterruptPending = 1u << 0;
static const u32 ImanInterruptEnable = 1u << 1;
static const u64 ErdpEventHandlerBusy = 1u << 3;

/* PORTSC bits */
static const u32 PortScConnected = 1u << 0;
static const u32 PortScEnabled = 1u << 1;
static const u32 PortScReset = 1u << 4;
static const u32 PortScPower = 1u << 9;
static const u32 PortScLinkWriteStrobe = 1u << 16;
static const u32 PortScResetChange = 1u << 21;
static const u32 PortScWarmReset = 1u << 31;
/* CSC, PEC, WRC, OCC, PRC, PLC, CEC -- all write-1-to-clear */
static const u32 PortScChangeMask = 0x00FE0000u;

static const u32 PortScSpeedShift = 10;
static const u32 PortScSpeedMask = 0xFu;

/* CRCR bits */
static const u64 CrcrRingCycleState = 1u << 0;

/* Extended capability IDs */
static const u32 ExtCapLegacySupport = 1;

static const u32 LegacyBiosOwned = 1u << 16;
static const u32 LegacyOsOwned = 1u << 24;

/* USBLEGCTLSTS. Masking down to LegacyCtlKeepMask -- (0x7 << 1) | (0xff << 5)
   | (0x7 << 17), the same set Linux keeps -- clears every SMI enable bit in
   one write; LegacyCtlAckSmiMask then acknowledges the RW1C SMI status bits
   so firmware stops re-entering SMM on our register accesses. */
static const u32 LegacyCtlKeepMask = 0x000E1FEEu;
static const u32 LegacyCtlAckSmiMask = 0xE0000000u;

/* Endpoint types, xHCI 1.2 table 6-9 */
static const u32 EpTypeControl = 4;
static const u32 EpTypeInterruptIn = 7;

/* Setup Stage Transfer Type */
static const u32 TrtNoData = 0;
static const u32 TrtOutData = 2;
static const u32 TrtInData = 3;

/* Timeouts. Real controllers answer commands in microseconds; these bounds
   exist only so a wedged or absent device cannot hang the USB task. */
static const ulong CommandTimeoutMs = 1000;
static const ulong TransferTimeoutMs = 1000;
static const ulong ResetTimeoutMs = 1000;
static const ulong PortResetTimeoutMs = 750;
static const ulong BiosHandoffTimeoutMs = 1000;

static const ulong PollPeriodMs = 4;
static const ulong PortRescanPeriodMs = 1000;

static void SleepMs(ulong ms)
{
    Sleep(ms * Const::NanoSecsInMs);
}

static ulong NowMs()
{
    Stdlib::Time now = GetBootTime();
    return now.GetValue() / Const::NanoSecsInMs;
}

const char* SpeedToStr(u8 speed)
{
    switch (speed)
    {
    case SpeedFull: return "full";
    case SpeedLow: return "low";
    case SpeedHigh: return "high";
    case SpeedSuper: return "super";
    case SpeedSuperPlus: return "super+";
    default: return "unknown";
    }
}

u16 DefaultMaxPacket0(u8 speed)
{
    switch (speed)
    {
    case SpeedSuper:
    case SpeedSuperPlus:
        return 512;
    case SpeedHigh:
        return 64;
    default:
        /* Low and Full speed must start at 8; the real value comes from the
           first eight bytes of the device descriptor. */
        return 8;
    }
}

/* ------------------------------------------------------------------ */
/* Ring                                                                */
/* ------------------------------------------------------------------ */

Ring::Ring()
    : Trbs(nullptr)
    , Phys(0)
    , Enqueue(0)
    , Cycle(1)
{
}

Ring::~Ring()
{
    Deinit();
}

bool Ring::Init()
{
    if (Trbs != nullptr)
        return true;

    ulong phys = 0;
    void* va = Mm::AllocMapPages(1, &phys);
    if (va == nullptr)
        return false;

    Stdlib::MemSet(va, 0, Const::PageSize);

    Trbs = (Trb*)va;
    Phys = phys;
    Enqueue = 0;
    Cycle = 1;

    /* Trailing Link TRB points back at the head and toggles the producer
       cycle state. Its own cycle bit stays clear until the producer first
       wraps onto it, so the controller stops at the head instead. */
    Trbs[NumTrbs - 1].Param = Phys;
    Trbs[NumTrbs - 1].Status = 0;
    Trbs[NumTrbs - 1].Control = TrbTypeField(TrbLink) | TrbToggleCycle;

    return true;
}

void Ring::Deinit()
{
    if (Trbs != nullptr)
    {
        Mm::UnmapFreePages(Trbs);
        Trbs = nullptr;
    }
    Phys = 0;
    Enqueue = 0;
    Cycle = 1;
}

ulong Ring::Push(u64 param, u32 status, u32 control)
{
    if (Trbs == nullptr)
        return 0;

    /* No producer/consumer distance check: this driver keeps at most a
       handful of TRBs in flight against a 255-entry ring. */
    Trb* trb = &Trbs[Enqueue];
    ulong slotPhys = Phys + (ulong)Enqueue * sizeof(Trb);

    trb->Param = param;
    trb->Status = status;

    control = control & ~TrbCycle;
    if (Cycle)
        control = control | TrbCycle;

    /* Publish the payload before the cycle bit hands the TRB to the HC */
    Hal::DmaWmb();
    trb->Control = control;

    Enqueue++;
    if (Enqueue == NumTrbs - 1)
    {
        u32 link = TrbTypeField(TrbLink) | TrbToggleCycle;
        if (Cycle)
            link = link | TrbCycle;

        Hal::DmaWmb();
        Trbs[NumTrbs - 1].Control = link;

        Cycle = Cycle ^ 1;
        Enqueue = 0;
    }

    return slotPhys;
}

/* ------------------------------------------------------------------ */
/* EventRing                                                           */
/* ------------------------------------------------------------------ */

EventRing::EventRing()
    : Trbs(nullptr)
    , Phys(0)
    , Erst(nullptr)
    , ErstPhys(0)
    , Dequeue(0)
    , Cycle(1)
{
}

EventRing::~EventRing()
{
    Deinit();
}

bool EventRing::Init()
{
    if (Trbs != nullptr)
        return true;

    ulong segPhys = 0;
    void* seg = Mm::AllocMapPages(1, &segPhys);
    if (seg == nullptr)
        return false;

    ulong erstPhys = 0;
    void* erst = Mm::AllocMapPages(1, &erstPhys);
    if (erst == nullptr)
    {
        Mm::UnmapFreePages(seg);
        return false;
    }

    Stdlib::MemSet(seg, 0, Const::PageSize);
    Stdlib::MemSet(erst, 0, Const::PageSize);

    Trbs = (Trb*)seg;
    Phys = segPhys;
    Erst = (ErstEntry*)erst;
    ErstPhys = erstPhys;
    Dequeue = 0;
    Cycle = 1;

    Erst[0].SegmentBase = segPhys;
    Erst[0].SegmentSize = NumTrbs;
    Erst[0].Reserved = 0;

    return true;
}

void EventRing::Deinit()
{
    if (Trbs != nullptr)
    {
        Mm::UnmapFreePages(Trbs);
        Trbs = nullptr;
    }
    if (Erst != nullptr)
    {
        Mm::UnmapFreePages(Erst);
        Erst = nullptr;
    }
    Phys = 0;
    ErstPhys = 0;
    Dequeue = 0;
    Cycle = 1;
}

bool EventRing::Pop(Trb* out)
{
    if (Trbs == nullptr)
        return false;

    const volatile Trb* trb = &Trbs[Dequeue];

    u32 control = trb->Control;
    if ((control & TrbCycle) != (u32)Cycle)
        return false;

    /* The cycle bit is the ownership handshake; the payload reads below must
       not be hoisted above it. A control dependency is not ordering. */
    Hal::DmaRmb();

    out->Param = trb->Param;
    out->Status = trb->Status;
    out->Control = control;

    Dequeue++;
    if (Dequeue == NumTrbs)
    {
        Dequeue = 0;
        Cycle = Cycle ^ 1;
    }

    return true;
}

/* ------------------------------------------------------------------ */
/* Endpoint / Device                                                   */
/* ------------------------------------------------------------------ */

Endpoint::Endpoint()
    : Dci(0)
    , Address(0)
    , MaxPacket(0)
    , Interval(0)
    , InFlight(false)
    , Done(false)
    , Completion(CompInvalid)
    , Residual(0)
    , PendingTrb(0)
    , DataTrb(0)
{
}

Device::Device()
    : InUse(false)
    , SlotId(0)
    , RootPort(0)
    , Speed(SpeedInvalid)
    , VendorId(0)
    , ProductId(0)
    , DeviceClass(0)
    , InterfaceNum(0)
    , IsKeyboard(false)
    , Reports(0)
    , Errors(0)
    , Route(0)
    , Tier(0)
    , TtSlot(0)
    , TtPort(0)
    , Mtt(false)
    , ParentSlot(0)
    , ParentPort(0)
    , IsHub(false)
    , HubPorts(0)
    , HubTtt(0)
    , HubPowerOnDelayMs(0)
    , HubEnumeratedMask(0)
    , InputCtx(nullptr)
    , InputCtxPhys(0)
    , DevCtx(nullptr)
    , DevCtxPhys(0)
    , Buf(nullptr)
    , BufPhys(0)
    , ReportBuf(nullptr)
    , ReportBufPhys(0)
{
}

Device::~Device()
{
    FreeDma();
}

bool Device::AllocDma()
{
    /* One page each: the input context (33 * 64 bytes worst case), the
       device context (32 * 64), the control payload staging buffer and the
       interrupt-IN landing zone. Page granularity satisfies the 64-byte
       alignment and no-page-crossing rules for free. */
    InputCtx = (u8*)Mm::AllocMapPages(1, &InputCtxPhys);
    if (InputCtx == nullptr)
        return false;

    DevCtx = (u8*)Mm::AllocMapPages(1, &DevCtxPhys);
    if (DevCtx == nullptr)
    {
        FreeDma();
        return false;
    }

    Buf = (u8*)Mm::AllocMapPages(1, &BufPhys);
    if (Buf == nullptr)
    {
        FreeDma();
        return false;
    }

    ReportBuf = (u8*)Mm::AllocMapPages(1, &ReportBufPhys);
    if (ReportBuf == nullptr)
    {
        FreeDma();
        return false;
    }

    Stdlib::MemSet(InputCtx, 0, Const::PageSize);
    Stdlib::MemSet(DevCtx, 0, Const::PageSize);
    Stdlib::MemSet(Buf, 0, Const::PageSize);
    Stdlib::MemSet(ReportBuf, 0, Const::PageSize);

    if (!Ep0.TrRing.Init())
    {
        FreeDma();
        return false;
    }

    if (!IntrIn.TrRing.Init())
    {
        FreeDma();
        return false;
    }

    return true;
}

void Device::FreeDma()
{
    Ep0.TrRing.Deinit();
    IntrIn.TrRing.Deinit();

    if (InputCtx != nullptr)
    {
        Mm::UnmapFreePages(InputCtx);
        InputCtx = nullptr;
        InputCtxPhys = 0;
    }
    if (DevCtx != nullptr)
    {
        Mm::UnmapFreePages(DevCtx);
        DevCtx = nullptr;
        DevCtxPhys = 0;
    }
    if (Buf != nullptr)
    {
        Mm::UnmapFreePages(Buf);
        Buf = nullptr;
        BufPhys = 0;
    }
    if (ReportBuf != nullptr)
    {
        Mm::UnmapFreePages(ReportBuf);
        ReportBuf = nullptr;
        ReportBufPhys = 0;
    }
}

void Device::Reset()
{
    FreeDma();

    InUse = false;
    SlotId = 0;
    RootPort = 0;
    Speed = SpeedInvalid;
    VendorId = 0;
    ProductId = 0;
    DeviceClass = 0;
    InterfaceNum = 0;
    IsKeyboard = false;
    Reports = 0;
    Errors = 0;
    Route = 0;
    Tier = 0;
    TtSlot = 0;
    TtPort = 0;
    Mtt = false;
    ParentSlot = 0;
    ParentPort = 0;
    IsHub = false;
    HubPorts = 0;
    HubTtt = 0;
    HubPowerOnDelayMs = 0;
    HubEnumeratedMask = 0;

    Ep0.Dci = 0;
    Ep0.Address = 0;
    Ep0.MaxPacket = 0;
    Ep0.Interval = 0;
    Ep0.InFlight = false;
    Ep0.Done = false;
    Ep0.Completion = CompInvalid;
    Ep0.Residual = 0;
    Ep0.PendingTrb = 0;
    Ep0.DataTrb = 0;

    IntrIn.Dci = 0;
    IntrIn.Address = 0;
    IntrIn.MaxPacket = 0;
    IntrIn.Interval = 0;
    IntrIn.InFlight = false;
    IntrIn.Done = false;
    IntrIn.Completion = CompInvalid;
    IntrIn.Residual = 0;
    IntrIn.PendingTrb = 0;
    IntrIn.DataTrb = 0;

    Kbd.Reset();
}

/* ------------------------------------------------------------------ */
/* Controller: register windows                                        */
/* ------------------------------------------------------------------ */

Controller* Controller::Instances[Controller::MaxControllers];
ulong Controller::InstanceCount;

Controller::Controller()
    : Ready(false)
    , PciDev(nullptr)
    , CapBase(nullptr)
    , OpBase(nullptr)
    , RtBase(nullptr)
    , DbBase(nullptr)
    , CapLength(0)
    , HciVersion(0)
    , MaxSlots(0)
    , NumPorts(0)
    , MaxInterrupters(0)
    , ContextSize(32)
    , PageSizeBytes(Const::PageSize)
    , Ac64(false)
    , ExtCapOffset(0)
    , Dcbaa(nullptr)
    , DcbaaPhys(0)
    , ScratchpadArray(nullptr)
    , ScratchpadArrayPhys(0)
    , ScratchpadPages(nullptr)
    , ScratchpadCount(0)
    , PendingCmdTrb(0)
    , CmdDone(false)
    , CmdCompletion(CompInvalid)
    , CmdSlotId(0)
    , PortChangePending(false)
    , LastScanMs(0)
{
    for (ulong i = 0; i < MaxPorts; i++)
    {
        Ports[i].Connected = false;
        Ports[i].Enumerated = false;
        Ports[i].Speed = SpeedInvalid;
        Ports[i].SlotId = 0;
        Ports[i].VendorId = 0;
        Ports[i].ProductId = 0;
        Ports[i].DeviceClass = 0;
        Ports[i].Keyboard = false;
    }
}

Controller::~Controller()
{
    if (ScratchpadPages != nullptr)
    {
        for (u32 i = 0; i < ScratchpadCount; i++)
        {
            if (ScratchpadPages[i] != nullptr)
                Mm::UnmapFreePages(ScratchpadPages[i]);
        }
        Mm::Free(ScratchpadPages);
        ScratchpadPages = nullptr;
    }

    if (ScratchpadArray != nullptr)
    {
        Mm::UnmapFreePages(ScratchpadArray);
        ScratchpadArray = nullptr;
    }

    if (Dcbaa != nullptr)
    {
        Mm::UnmapFreePages(Dcbaa);
        Dcbaa = nullptr;
    }
}

u32 Controller::CapRead32(ulong off) const
{
    return MmioRead32(CapBase + off);
}

u32 Controller::OpRead32(ulong off) const
{
    return MmioRead32(OpBase + off);
}

void Controller::OpWrite32(ulong off, u32 val)
{
    MmioWrite32(OpBase + off, val);
}

void Controller::OpWrite64(ulong off, u64 val)
{
    /* Split into two 32-bit stores: several controllers (and QEMU's own
       device model) reject a single 64-bit access to these registers. */
    MmioWrite32(OpBase + off, (u32)(val & 0xFFFFFFFFu));
    MmioWrite32(OpBase + off + 4, (u32)(val >> 32));
}

u32 Controller::PortRead32(u8 port, ulong off) const
{
    return MmioRead32(OpBase + OpPortScBase + (ulong)(port - 1) * OpPortRegSize + off);
}

void Controller::PortWrite32(u8 port, ulong off, u32 val)
{
    MmioWrite32(OpBase + OpPortScBase + (ulong)(port - 1) * OpPortRegSize + off, val);
}

u32 Controller::RtRead32(ulong off) const
{
    return MmioRead32(RtBase + off);
}

void Controller::RtWrite32(ulong off, u32 val)
{
    MmioWrite32(RtBase + off, val);
}

void Controller::RtWrite64(ulong off, u64 val)
{
    MmioWrite32(RtBase + off, (u32)(val & 0xFFFFFFFFu));
    MmioWrite32(RtBase + off + 4, (u32)(val >> 32));
}

void Controller::Doorbell(u8 slot, u32 value)
{
    /* Every ring update must be visible before the doorbell rings */
    Hal::DmaWmb();
    MmioWrite32(DbBase + (ulong)slot * 4, value);
}

u8* Controller::SlotContext(u8* base) const
{
    return base;
}

u8* Controller::EndpointContext(u8* base, u8 dci) const
{
    return base + (ulong)dci * ContextSize;
}

/* PORTSC is a minefield of write-1-to-clear and write-to-act bits: build
   every write from the live value with those bits forced off, then OR in
   only the action wanted. */
static u32 PortScBase(u32 v)
{
    return v & ~(PortScChangeMask | PortScEnabled | PortScReset |
                 PortScWarmReset | PortScLinkWriteStrobe);
}

/* ------------------------------------------------------------------ */
/* Controller: bring-up                                                */
/* ------------------------------------------------------------------ */

bool Controller::MapBar(Pci::DeviceInfo* dev)
{
    auto& pci = Pci::GetInstance();

    u32 barLow = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, 0);
    if (barLow & 1)
    {
        Trace(0, "Xhci: BAR0 is I/O space, not MMIO");
        return false;
    }

    ulong physAddr = barLow & ~0xFUL;
    bool is64 = ((barLow & 0x6) == 0x4);
    u32 barHigh = 0;
    if (is64)
    {
        barHigh = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, 1);
        physAddr |= ((ulong)barHigh << 32);
    }

    if (physAddr == 0)
    {
        Trace(0, "Xhci: BAR0 not assigned");
        return false;
    }

    /* Size-probe both halves; probing only the low half computes a bogus
       size for a BAR placed above 4 GB. */
    pci.WriteDword(dev->Bus, dev->Slot, dev->Func, 0x10, 0xFFFFFFFF);
    u32 maskLow = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, 0);
    u32 maskHigh = 0xFFFFFFFF;
    if (is64)
    {
        pci.WriteDword(dev->Bus, dev->Slot, dev->Func, 0x14, 0xFFFFFFFF);
        maskHigh = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, 1);
        pci.WriteDword(dev->Bus, dev->Slot, dev->Func, 0x14, barHigh);
    }
    pci.WriteDword(dev->Bus, dev->Slot, dev->Func, 0x10, barLow);

    ulong sizeMask = ((ulong)maskHigh << 32) | (ulong)(maskLow & ~0xFU);
    ulong barSize = ~sizeMask + 1;
    if (barSize == 0 || barSize > 16 * Const::MB)
        barSize = 64 * Const::KB;

    auto& pt = Mm::PageTable::GetInstance();
    ulong va = pt.MapMmioRegion(physAddr, barSize);
    if (va == 0)
    {
        Trace(0, "Xhci: failed to map BAR0 phys 0x%p size 0x%p", physAddr, barSize);
        return false;
    }

    Trace(0, "Xhci: BAR0 phys 0x%p size 0x%p va 0x%p", physAddr, barSize, va);

    CapBase = (volatile u8*)va;
    return true;
}

void Controller::TakeOwnershipFromBios()
{
    if (ExtCapOffset == 0)
        return;

    /* Extended capability list: dword-indexed from the capability base, a
       zero Next terminating the walk. Bound the loop so a corrupt list
       cannot spin forever. */
    static const ulong MaxExtCaps = 64;
    ulong off = (ulong)ExtCapOffset * 4;

    for (ulong i = 0; i < MaxExtCaps && off != 0; i++)
    {
        u32 cap = MmioRead32(CapBase + off);
        u32 id = cap & 0xFF;
        u32 next = (cap >> 8) & 0xFF;

        if (id == ExtCapLegacySupport)
        {
            if ((cap & LegacyBiosOwned) == 0 && (cap & LegacyOsOwned) != 0)
            {
                Trace(0, "Xhci: already OS-owned");
            }
            else
            {
                Trace(0, "Xhci: requesting ownership from firmware (usblegsup 0x%p)",
                    (ulong)cap);

                MmioWrite32(CapBase + off, cap | LegacyOsOwned);

                ulong deadline = NowMs() + BiosHandoffTimeoutMs;
                for (;;)
                {
                    u32 cur = MmioRead32(CapBase + off);
                    if ((cur & LegacyBiosOwned) == 0)
                        break;

                    if (NowMs() >= deadline)
                    {
                        /* Some firmware never releases the semaphore. Clear
                           its bit by hand: the controller is about to be
                           reset anyway, and leaving SMIs armed is worse. */
                        Trace(0, "Xhci: BIOS handoff timed out, forcing ownership");
                        MmioWrite32(CapBase + off,
                            (MmioRead32(CapBase + off) & ~LegacyBiosOwned) | LegacyOsOwned);
                        break;
                    }
                    SleepMs(10);
                }
            }

            /* Silence every legacy SMI source and acknowledge stale status,
               otherwise firmware keeps trapping our register accesses. */
            u32 ctl = MmioRead32(CapBase + off + 4);
            ctl = (ctl & LegacyCtlKeepMask) | LegacyCtlAckSmiMask;
            MmioWrite32(CapBase + off + 4, ctl);
        }

        if (next == 0)
            break;

        off = off + next * 4;
    }
}

/* Intel 7/8-series and Cherry Trail PCHs boot with their USB2 ports wired to
   the companion EHCI controller and their SuperSpeed pins disabled; the
   ports only appear on the xHCI after this routing switch. Harmless on
   parts that do not implement the registers, but only applied to the models
   Intel documents it for. */
void Controller::ApplyIntelPortSwitch(Pci::DeviceInfo* dev)
{
    static const u16 IntelPantherPointXhci = 0x1E31;
    static const u16 IntelLynxPointXhci = 0x8C31;
    static const u16 IntelLynxPointLpXhci = 0x9C31;
    static const u16 IntelCherryviewXhci = 0x22B5;

    if (dev->Vendor != Pci::VendorIntel)
        return;

    if (dev->Device != IntelPantherPointXhci &&
        dev->Device != IntelLynxPointXhci &&
        dev->Device != IntelLynxPointLpXhci &&
        dev->Device != IntelCherryviewXhci)
        return;

    static const u16 RegUsb3Prm = 0xDC;   /* SuperSpeed-capable port mask */
    static const u16 RegUsb3Pssen = 0xD8; /* SuperSpeed enable            */
    static const u16 RegXusb2Prm = 0xD4;  /* USB2 routable port mask      */
    static const u16 RegXusb2Pr = 0xD0;   /* USB2 port routing            */

    auto& pci = Pci::GetInstance();

    u32 ssMask = pci.ReadDword(dev->Bus, dev->Slot, dev->Func, RegUsb3Prm);
    pci.WriteDword(dev->Bus, dev->Slot, dev->Func, RegUsb3Pssen, ssMask);

    u32 hsMask = pci.ReadDword(dev->Bus, dev->Slot, dev->Func, RegXusb2Prm);
    pci.WriteDword(dev->Bus, dev->Slot, dev->Func, RegXusb2Pr, hsMask);

    Trace(0, "Xhci: Intel port switch: usb3 0x%p usb2 0x%p",
        (ulong)ssMask, (ulong)hsMask);
}

bool Controller::ResetController()
{
    /* The controller may still be publishing CNR after a warm reset */
    ulong deadline = NowMs() + ResetTimeoutMs;
    while ((OpRead32(OpUsbSts) & UsbStsControllerNotReady) != 0)
    {
        if (NowMs() >= deadline)
        {
            Trace(0, "Xhci: controller not ready before reset");
            return false;
        }
        SleepMs(1);
    }

    /* Stop it first: HCRST on a running controller is undefined */
    u32 cmd = OpRead32(OpUsbCmd);
    if ((cmd & UsbCmdRun) != 0)
    {
        OpWrite32(OpUsbCmd, cmd & ~UsbCmdRun);

        deadline = NowMs() + ResetTimeoutMs;
        while ((OpRead32(OpUsbSts) & UsbStsHalted) == 0)
        {
            if (NowMs() >= deadline)
            {
                Trace(0, "Xhci: controller did not halt");
                return false;
            }
            SleepMs(1);
        }
    }

    OpWrite32(OpUsbCmd, OpRead32(OpUsbCmd) | UsbCmdReset);

    deadline = NowMs() + ResetTimeoutMs;
    for (;;)
    {
        u32 c = OpRead32(OpUsbCmd);
        u32 s = OpRead32(OpUsbSts);

        /* A controller that has fallen off the bus reads back all-ones */
        if (c == 0xFFFFFFFFu || s == 0xFFFFFFFFu)
        {
            Trace(0, "Xhci: controller vanished during reset");
            return false;
        }

        if ((c & UsbCmdReset) == 0 && (s & UsbStsControllerNotReady) == 0)
            break;

        if (NowMs() >= deadline)
        {
            Trace(0, "Xhci: reset timed out (usbcmd 0x%p usbsts 0x%p)",
                (ulong)c, (ulong)s);
            return false;
        }
        SleepMs(1);
    }

    return true;
}

bool Controller::SetupMemory()
{
    /* Device Context Base Address Array: index 0 holds the scratchpad
       array pointer, 1..MaxSlots the per-slot device contexts. */
    Dcbaa = (u64*)Mm::AllocMapPages(1, &DcbaaPhys);
    if (Dcbaa == nullptr)
        return false;
    Stdlib::MemSet(Dcbaa, 0, Const::PageSize);

    u32 hcs2 = CapRead32(CapHcsParams2);
    ScratchpadCount = (((hcs2 >> 21) & 0x1F) << 5) | ((hcs2 >> 27) & 0x1F);

    static const u32 MaxScratchpads = 512;
    if (ScratchpadCount > MaxScratchpads)
    {
        Trace(0, "Xhci: %u scratchpad buffers requested, capping at %u",
            (ulong)ScratchpadCount, (ulong)MaxScratchpads);
        ScratchpadCount = MaxScratchpads;
    }

    if (ScratchpadCount > 0)
    {
        ScratchpadArray = (u64*)Mm::AllocMapPages(1, &ScratchpadArrayPhys);
        if (ScratchpadArray == nullptr)
            return false;
        Stdlib::MemSet(ScratchpadArray, 0, Const::PageSize);

        ScratchpadPages = (void**)Mm::Alloc(ScratchpadCount * sizeof(void*), 'UsbS');
        if (ScratchpadPages == nullptr)
            return false;
        Stdlib::MemSet(ScratchpadPages, 0, ScratchpadCount * sizeof(void*));

        size_t pagesEach = PageSizeBytes / Const::PageSize;
        if (pagesEach == 0)
            pagesEach = 1;

        for (u32 i = 0; i < ScratchpadCount; i++)
        {
            ulong phys = 0;
            void* p = Mm::AllocMapPages(pagesEach, &phys);
            if (p == nullptr)
            {
                Trace(0, "Xhci: out of memory for scratchpad %u", (ulong)i);
                return false;
            }
            Stdlib::MemSet(p, 0, pagesEach * Const::PageSize);
            ScratchpadPages[i] = p;
            ScratchpadArray[i] = phys;
        }

        Dcbaa[0] = ScratchpadArrayPhys;
    }

    Trace(0, "Xhci: %u slots, %u ports, %u scratchpads, ctx %u bytes",
        (ulong)MaxSlots, (ulong)NumPorts, (ulong)ScratchpadCount, (ulong)ContextSize);

    OpWrite32(OpConfig, (OpRead32(OpConfig) & ~0xFFu) | MaxSlots);
    OpWrite64(OpDcbaap, DcbaaPhys);
    OpWrite32(OpDnCtrl, 0);

    if (!CmdRing.Init())
        return false;
    OpWrite64(OpCrcr, CmdRing.GetPhys() | CrcrRingCycleState);

    if (!EvtRing.Init())
        return false;

    /* ERSTBA must be written last: that store arms the event ring */
    RtWrite32(RtInterrupter0 + IrErstSz, 1);
    RtWrite64(RtInterrupter0 + IrErdp, EvtRing.GetDequeuePhys() | ErdpEventHandlerBusy);
    RtWrite64(RtInterrupter0 + IrErstBa, EvtRing.GetErstPhys());

    /* Polling driver: leave the interrupter disabled but keep its pending
       bit clear so the event-ring state machine keeps advancing. */
    RtWrite32(RtInterrupter0 + IrImod, 0);
    RtWrite32(RtInterrupter0 + IrIman,
        (RtRead32(RtInterrupter0 + IrIman) & ~ImanInterruptEnable) | ImanInterruptPending);

    return true;
}

bool Controller::StartController()
{
    u32 cmd = OpRead32(OpUsbCmd);
    cmd = (cmd | UsbCmdRun | UsbCmdHsErrEnable) & ~UsbCmdIntEnable;
    OpWrite32(OpUsbCmd, cmd);

    ulong deadline = NowMs() + ResetTimeoutMs;
    while ((OpRead32(OpUsbSts) & UsbStsHalted) != 0)
    {
        if (NowMs() >= deadline)
        {
            Trace(0, "Xhci: controller did not start");
            return false;
        }
        SleepMs(1);
    }

    return true;
}

void Controller::PowerPorts()
{
    /* Debounce interval a device attached before boot still needs before
       PORTSC.CCS can be trusted (USB 2.0 TATTDB). */
    static const ulong PortDebounceMs = 100;
    bool powered = false;

    for (u8 port = 1; port <= NumPorts && port < MaxPorts; port++)
    {
        u32 v = PortRead32(port, 0);
        if ((v & PortScPower) == 0)
        {
            PortWrite32(port, 0, PortScBase(v) | PortScPower);
            powered = true;
        }
    }

    /* USB 2.0 requires 100ms of bPwrOn2PwrGood after switching port power,
       and the same wait doubles as the attach debounce when firmware had
       already powered everything. */
    (void)powered;
    SleepMs(PortDebounceMs);
}

bool Controller::Init(Pci::DeviceInfo* dev)
{
    PciDev = dev;

    auto& pci = Pci::GetInstance();

    /* Memory space + bus mastering. Firmware normally leaves both on, but a
       controller handed over cold has neither. */
    u16 command = pci.ReadWord(dev->Bus, dev->Slot, dev->Func, 0x04);
    command = (u16)(command | (1 << 1) | (1 << 2));
    pci.WriteWord(dev->Bus, dev->Slot, dev->Func, 0x04, command);

    if (!MapBar(dev))
        return false;

    u32 capDw0 = CapRead32(CapCapLength);
    CapLength = (u8)(capDw0 & 0xFF);
    HciVersion = (u16)((capDw0 >> 16) & 0xFFFF);

    if (CapLength == 0 || CapLength == 0xFF)
    {
        Trace(0, "Xhci: implausible CAPLENGTH %u", (ulong)CapLength);
        return false;
    }

    OpBase = CapBase + CapLength;

    u32 hcs1 = CapRead32(CapHcsParams1);
    MaxSlots = (u8)(hcs1 & 0xFF);
    MaxInterrupters = (u16)((hcs1 >> 8) & 0x7FF);
    NumPorts = (u8)((hcs1 >> 24) & 0xFF);

    u32 hcc1 = CapRead32(CapHccParams1);
    Ac64 = (hcc1 & 1) != 0;
    ContextSize = ((hcc1 >> 2) & 1) ? 64 : 32;
    ExtCapOffset = (u16)((hcc1 >> 16) & 0xFFFF);

    u32 dbOff = CapRead32(CapDbOff) & ~0x3u;
    u32 rtsOff = CapRead32(CapRtsOff) & ~0x1Fu;
    DbBase = CapBase + dbOff;
    RtBase = CapBase + rtsOff;

    Trace(0, "Xhci: %04x:%04x hci %x.%x caplen %u slots %u ports %u",
        (ulong)dev->Vendor, (ulong)dev->Device,
        (ulong)(HciVersion >> 8), (ulong)(HciVersion & 0xFF),
        (ulong)CapLength, (ulong)MaxSlots, (ulong)NumPorts);

    if (MaxSlots == 0 || NumPorts == 0)
    {
        Trace(0, "Xhci: no slots or ports");
        return false;
    }

    if (!Ac64)
    {
        /* Every DMA structure this driver allocates could sit above 4 GB;
           refusing beats silently truncating a pointer. */
        Trace(0, "Xhci: 32-bit-only controller not supported");
        return false;
    }

    ApplyIntelPortSwitch(dev);
    TakeOwnershipFromBios();

    if (!ResetController())
        return false;

    /* PAGESIZE is a bitmap: bit n set means the controller supports a
       2^(n+12) byte page. Take the smallest one it offers. */
    u32 pageSizeBitmap = OpRead32(OpPageSize) & 0xFFFF;
    PageSizeBytes = Const::PageSize;
    for (u32 bit = 0; bit < 16; bit++)
    {
        if (pageSizeBitmap & (1u << bit))
        {
            PageSizeBytes = (u32)1 << (bit + 12);
            break;
        }
    }
    if (PageSizeBytes < Const::PageSize)
        PageSizeBytes = Const::PageSize;

    if (!SetupMemory())
        return false;

    if (!StartController())
        return false;

    Ready = true;

    PowerPorts();
    LastScanMs = NowMs();
    ScanPorts();

    return true;
}

/* ------------------------------------------------------------------ */
/* Controller: events                                                  */
/* ------------------------------------------------------------------ */

Device* Controller::FindDeviceBySlot(u8 slot)
{
    if (slot == 0)
        return nullptr;

    for (ulong i = 0; i < MaxDevices; i++)
    {
        if (Devices[i].InUse && Devices[i].SlotId == slot)
            return &Devices[i];
    }

    return nullptr;
}

Device* Controller::AllocDevice()
{
    for (ulong i = 0; i < MaxDevices; i++)
    {
        if (!Devices[i].InUse)
            return &Devices[i];
    }

    return nullptr;
}

void Controller::HandleCommandEvent(const Trb& ev)
{
    ulong trbPhys = (ulong)(ev.Param & ~0xFULL);
    u32 code = (ev.Status >> 24) & 0xFF;
    u8 slot = (u8)((ev.Control >> 24) & 0xFF);

    if (PendingCmdTrb != 0 && trbPhys == PendingCmdTrb)
    {
        CmdCompletion = code;
        CmdSlotId = slot;
        CmdDone = true;
        return;
    }

    Trace(UsbLL, "Xhci: stray command completion trb 0x%p code %u",
        trbPhys, (ulong)code);
}

void Controller::HandleTransferEvent(const Trb& ev)
{
    ulong trbPhys = (ulong)(ev.Param & ~0xFULL);
    u32 code = (ev.Status >> 24) & 0xFF;
    u32 residual = ev.Status & 0xFFFFFF;
    u8 slot = (u8)((ev.Control >> 24) & 0xFF);
    u8 dci = (u8)((ev.Control >> 16) & 0x1F);

    Device* dev = FindDeviceBySlot(slot);
    if (dev == nullptr)
    {
        Trace(UsbLL, "Xhci: transfer event for unknown slot %u", (ulong)slot);
        return;
    }

    Endpoint* ep = nullptr;
    if (dci == dev->Ep0.Dci)
        ep = &dev->Ep0;
    else if (dci == dev->IntrIn.Dci && dev->IntrIn.Dci != 0)
        ep = &dev->IntrIn;

    if (ep == nullptr)
    {
        Trace(UsbLL, "Xhci: transfer event for unknown dci %u slot %u",
            (ulong)dci, (ulong)slot);
        return;
    }

    /* Only the data-carrying TRB reports a meaningful residual. A short
       packet ends the data TD early and raises its own event (ISP is set);
       the status TD still runs and delivers the completion being waited on,
       so record the length here and let the wait continue. */
    if (trbPhys == ep->DataTrb)
        ep->Residual = residual;

    if (trbPhys == ep->PendingTrb)
    {
        ep->Completion = code;
        ep->Done = true;
        return;
    }

    if (code != CompSuccess && code != CompShortPacket)
    {
        /* An error on an earlier stage of the transfer: nothing further will
           be executed, so fail the wait now instead of timing out. */
        ep->Completion = code;
        ep->Done = true;
    }
}

void Controller::ProcessEvents()
{
    if (!EvtRing.IsReady())
        return;

    /* Bounded so a controller wedged into producing events forever cannot
       starve the rest of the USB task. */
    static const ulong MaxEventsPerPass = 256;

    Trb ev;
    ulong drained = 0;

    while (drained < MaxEventsPerPass && EvtRing.Pop(&ev))
    {
        drained++;

        u32 type = TrbTypeOf(ev.Control);
        switch (type)
        {
        case TrbTransferEvent:
            HandleTransferEvent(ev);
            break;
        case TrbCommandCompletion:
            HandleCommandEvent(ev);
            break;
        case TrbPortStatusChange:
            PortChangePending = true;
            Trace(UsbLL, "Xhci: port status change, port %u",
                (ulong)((ev.Param >> 24) & 0xFF));
            break;
        default:
            Trace(UsbLL, "Xhci: event type %u ignored", (ulong)type);
            break;
        }
    }

    if (drained > 0)
        RtWrite64(RtInterrupter0 + IrErdp,
            EvtRing.GetDequeuePhys() | ErdpEventHandlerBusy);

    u32 sts = OpRead32(OpUsbSts);
    if (sts & UsbStsEventInt)
        OpWrite32(OpUsbSts, UsbStsEventInt);
    if (sts & UsbStsPortChange)
    {
        OpWrite32(OpUsbSts, UsbStsPortChange);
        PortChangePending = true;
    }
    if (sts & (UsbStsHostSystemError | UsbStsHostControllerError))
    {
        Trace(0, "Xhci: host controller error, usbsts 0x%p", (ulong)sts);
        OpWrite32(OpUsbSts, sts & (UsbStsHostSystemError | UsbStsHostControllerError));
        Ready = false;
    }

    u32 iman = RtRead32(RtInterrupter0 + IrIman);
    if (iman & ImanInterruptPending)
        RtWrite32(RtInterrupter0 + IrIman,
            (iman & ~ImanInterruptEnable) | ImanInterruptPending);
}

/* ------------------------------------------------------------------ */
/* Controller: commands                                                */
/* ------------------------------------------------------------------ */

u32 Controller::WaitCommand(ulong trbPhys, u8* slotIdOut)
{
    ulong deadline = NowMs() + CommandTimeoutMs;

    while (!CmdDone)
    {
        ProcessEvents();

        if (CmdDone)
            break;

        if (NowMs() >= deadline)
        {
            Trace(0, "Xhci: command trb 0x%p timed out", trbPhys);
            PendingCmdTrb = 0;
            return CompInvalid;
        }

        SleepMs(1);
    }

    PendingCmdTrb = 0;
    CmdDone = false;

    if (slotIdOut != nullptr)
        *slotIdOut = CmdSlotId;

    return CmdCompletion;
}

u32 Controller::RunCommand(u64 param, u32 status, u32 control, u8* slotIdOut)
{
    if (!CmdRing.IsReady())
        return CompInvalid;

    CmdDone = false;
    CmdCompletion = CompInvalid;
    CmdSlotId = 0;

    ulong trbPhys = CmdRing.Push(param, status, control);
    if (trbPhys == 0)
        return CompInvalid;

    PendingCmdTrb = trbPhys;

    Doorbell(0, 0);

    return WaitCommand(trbPhys, slotIdOut);
}

/* ------------------------------------------------------------------ */
/* Controller: transfers                                               */
/* ------------------------------------------------------------------ */

u32 Controller::WaitTransfer(Device& dev, Endpoint& ep, ulong trbPhys, ulong timeoutMs)
{
    (void)dev;

    ulong deadline = NowMs() + timeoutMs;

    while (!ep.Done)
    {
        ProcessEvents();

        if (ep.Done)
            break;

        if (NowMs() >= deadline)
        {
            Trace(0, "Xhci: transfer trb 0x%p dci %u timed out",
                trbPhys, (ulong)ep.Dci);
            ep.InFlight = false;
            ep.PendingTrb = 0;
            return CompInvalid;
        }

        SleepMs(1);
    }

    ep.Done = false;
    ep.InFlight = false;
    ep.PendingTrb = 0;

    return ep.Completion;
}

u32 Controller::ControlTransfer(Device& dev, u8 requestType, u8 request, u16 value,
    u16 index, u16 length, u32* transferred)
{
    Endpoint& ep = dev.Ep0;

    if (!ep.TrRing.IsReady())
        return CompInvalid;

    if (length > Const::PageSize)
        return CompInvalid;

    bool dirIn = (requestType & ReqDirIn) != 0;

    ep.Residual = 0;
    ep.Completion = CompInvalid;
    ep.Done = false;
    ep.DataTrb = 0;

    u64 setup = (u64)requestType |
        ((u64)request << 8) |
        ((u64)value << 16) |
        ((u64)index << 32) |
        ((u64)length << 48);

    u32 trt = TrtNoData;
    if (length > 0)
        trt = dirIn ? TrtInData : TrtOutData;

    ep.TrRing.Push(setup, 8, TrbTypeField(TrbSetupStage) | TrbIdt | (trt << 16));

    if (length > 0)
    {
        u32 control = TrbTypeField(TrbDataStage) | TrbIsp;
        if (dirIn)
            control = control | TrbDirIn;

        ep.DataTrb = ep.TrRing.Push(dev.BufPhys, length, control);
    }

    /* The status stage runs opposite to the data stage (IN when there was no
       data), and carries the IOC that raises the completion event. */
    u32 statusControl = TrbTypeField(TrbStatusStage) | TrbIoc;
    if (length == 0 || !dirIn)
        statusControl = statusControl | TrbDirIn;

    ulong statusTrb = ep.TrRing.Push(0, 0, statusControl);
    if (statusTrb == 0)
        return CompInvalid;

    ep.PendingTrb = statusTrb;
    ep.InFlight = true;

    Doorbell(dev.SlotId, ep.Dci);

    u32 code = WaitTransfer(dev, ep, statusTrb, TransferTimeoutMs);

    if (transferred != nullptr)
    {
        u32 done = 0;
        if (length > ep.Residual)
            done = length - ep.Residual;
        *transferred = done;
    }

    return code;
}

/* ------------------------------------------------------------------ */
/* Controller: enumeration                                             */
/* ------------------------------------------------------------------ */

/* Encode bInterval into the xHCI endpoint-context Interval field, which is
   log2 of the service interval in 125us microframes. */
static u8 EncodeInterval(u8 speed, u8 bInterval)
{
    if (speed == SpeedHigh || speed == SpeedSuper || speed == SpeedSuperPlus)
    {
        u8 b = bInterval;
        if (b < 1)
            b = 1;
        if (b > 16)
            b = 16;
        return (u8)(b - 1);
    }

    /* Low/Full speed express bInterval in whole 1ms frames */
    u32 micro = (bInterval < 1 ? 1u : (u32)bInterval) * 8;
    u8 i = 3;
    while (i < 10 && (1u << (i + 1)) <= micro)
        i++;

    return i;
}

bool Controller::ResetPort(u8 port)
{
    u32 v = PortRead32(port, 0);

    /* USB3 ports train their link automatically and come up enabled */
    if ((v & PortScEnabled) != 0)
        return true;

    /* Acknowledge stale change bits so the reset-done edge is unambiguous */
    PortWrite32(port, 0, PortScBase(v) | (v & PortScChangeMask));

    v = PortRead32(port, 0);
    PortWrite32(port, 0, PortScBase(v) | PortScReset);

    ulong deadline = NowMs() + PortResetTimeoutMs;
    for (;;)
    {
        u32 cur = PortRead32(port, 0);

        if (cur == 0xFFFFFFFFu)
            return false;

        if ((cur & PortScResetChange) != 0)
        {
            PortWrite32(port, 0, PortScBase(cur) | PortScResetChange);
            break;
        }

        if (NowMs() >= deadline)
        {
            Trace(0, "Xhci: port %u reset timed out (portsc 0x%p)",
                (ulong)port, (ulong)cur);
            return false;
        }

        SleepMs(2);
    }

    /* USB 2.0 TRSTRCY: the device is unresponsive for 10ms after reset */
    SleepMs(10);

    u32 after = PortRead32(port, 0);
    if ((after & PortScEnabled) != 0)
        return true;

    /* A SuperSpeed link that failed to train needs a warm reset; the bit is
       reserved on USB2 ports, where this write is a no-op. */
    Trace(0, "Xhci: port %u not enabled after hot reset (portsc 0x%p), "
        "trying warm reset", (ulong)port, (ulong)after);

    PortWrite32(port, 0, PortScBase(after) | PortScWarmReset);

    deadline = NowMs() + PortResetTimeoutMs;
    for (;;)
    {
        u32 cur = PortRead32(port, 0);

        if (cur == 0xFFFFFFFFu)
            return false;

        if ((cur & PortScEnabled) != 0)
        {
            PortWrite32(port, 0, PortScBase(cur) | (cur & PortScChangeMask));
            SleepMs(10);
            return true;
        }

        if (NowMs() >= deadline)
        {
            Trace(0, "Xhci: port %u warm reset failed (portsc 0x%p)",
                (ulong)port, (ulong)cur);
            return false;
        }

        SleepMs(2);
    }
}

bool Controller::AddressDevice(Device& dev, bool blockSetAddress)
{
    Stdlib::MemSet(dev.InputCtx, 0, Const::PageSize);

    /* Input Control Context: add the slot context (A0) and EP0 (A1) */
    u32* icc = (u32*)dev.InputCtx;
    icc[0] = 0;                 /* drop flags */
    icc[1] = (1u << 0) | (1u << 1);

    u32* slot = (u32*)SlotContext(dev.InputCtx + ContextSize);
    slot[0] = ((u32)1 << 27) |                     /* Context Entries = 1 */
              (((u32)dev.Speed & 0xF) << 20) |     /* Speed              */
              (dev.Route & 0xFFFFF);               /* Route String       */
    if (dev.Mtt)
        slot[0] = slot[0] | (1u << 25);
    slot[1] = ((u32)dev.RootPort & 0xFF) << 16;    /* Root Hub Port Num  */
    /* TT Hub Slot ID / TT Port Number route a low- or full-speed device's
       split transactions through the transaction translator of the nearest
       high-speed hub; both stay zero on a root port. */
    slot[2] = ((u32)dev.TtPort << 8) | (u32)dev.TtSlot;
    slot[3] = 0;

    u32* ep0 = (u32*)EndpointContext(dev.InputCtx + ContextSize, 1);
    ep0[0] = 0;
    ep0[1] = (EpTypeControl << 3) |                /* EP Type = Control  */
             (3u << 1) |                           /* CErr = 3           */
             ((u32)dev.Ep0.MaxPacket << 16);
    u64 deq = dev.Ep0.TrRing.GetPhys() | (dev.Ep0.TrRing.GetCycle() ? 1u : 0u);
    ep0[2] = (u32)(deq & 0xFFFFFFFFu);
    ep0[3] = (u32)(deq >> 32);
    ep0[4] = 8;                                    /* Average TRB Length */

    u32 control = TrbTypeField(TrbAddressDevice) | ((u32)dev.SlotId << 24);
    if (blockSetAddress)
        control = control | (1u << 9);             /* BSR */

    Hal::DmaWmb();

    u32 code = RunCommand(dev.InputCtxPhys, 0, control, nullptr);
    if (code != CompSuccess)
    {
        Trace(0, "Xhci: address device slot %u failed, code %u",
            (ulong)dev.SlotId, (ulong)code);
        return false;
    }

    return true;
}

bool Controller::ParseConfiguration(Device& dev, u32 length)
{
    const u8* p = dev.Buf;
    u32 off = 0;

    bool inBootKeyboard = false;
    u8 keyboardInterface = 0;

    while (off + 2 <= length)
    {
        u8 len = p[off];
        u8 type = p[off + 1];

        if (len < 2 || off + len > length)
            break;

        if (type == DescInterface && len >= sizeof(InterfaceDescriptor))
        {
            const InterfaceDescriptor* itf = (const InterfaceDescriptor*)(p + off);

            inBootKeyboard = (itf->InterfaceClass == ClassHid) &&
                             (itf->InterfaceSubClass == SubClassBoot) &&
                             (itf->InterfaceProtocol == ProtocolKeyboard);

            if (inBootKeyboard)
                keyboardInterface = itf->InterfaceNumber;

            Trace(UsbLL, "Xhci: interface %u class %u/%u/%u",
                (ulong)itf->InterfaceNumber, (ulong)itf->InterfaceClass,
                (ulong)itf->InterfaceSubClass, (ulong)itf->InterfaceProtocol);
        }
        else if (type == DescEndpoint && len >= sizeof(EndpointDescriptor))
        {
            const EndpointDescriptor* epd = (const EndpointDescriptor*)(p + off);

            bool isIntrIn = ((epd->Attributes & EndpointXferMask) == EndpointXferInterrupt) &&
                            ((epd->EndpointAddress & EndpointDirIn) != 0);

            if (inBootKeyboard && isIntrIn && dev.IntrIn.Dci == 0)
            {
                u8 num = epd->EndpointAddress & EndpointNumMask;

                dev.IntrIn.Address = epd->EndpointAddress;
                dev.IntrIn.Dci = (u8)(num * 2 + 1);
                dev.IntrIn.MaxPacket = epd->MaxPacketSize & 0x7FF;
                dev.IntrIn.Interval = EncodeInterval(dev.Speed, epd->Interval);
                dev.InterfaceNum = keyboardInterface;
                dev.IsKeyboard = true;

                Trace(0, "Xhci: boot keyboard on interface %u ep 0x%p "
                    "mps %u interval %u dci %u",
                    (ulong)keyboardInterface, (ulong)epd->EndpointAddress,
                    (ulong)dev.IntrIn.MaxPacket, (ulong)dev.IntrIn.Interval,
                    (ulong)dev.IntrIn.Dci);
            }
        }

        off += len;
    }

    return dev.IsKeyboard;
}

bool Controller::ConfigureKeyboard(Device& dev)
{
    /* Configure Endpoint adds the interrupt-IN pipe. The slot context has
       to be resubmitted with Context Entries raised to cover the new DCI,
       seeded from the controller's own output context. */
    Stdlib::MemSet(dev.InputCtx, 0, Const::PageSize);

    u32* icc = (u32*)dev.InputCtx;
    icc[0] = 0;
    icc[1] = (1u << 0) | (1u << dev.IntrIn.Dci);

    /* The slot context is written by the controller; order the copy after
       the command completion that published it. */
    Hal::DmaRmb();

    u8* inSlot = SlotContext(dev.InputCtx + ContextSize);
    Stdlib::MemCpy(inSlot, SlotContext(dev.DevCtx), ContextSize);

    u32* slot = (u32*)inSlot;
    slot[0] = (slot[0] & ~(0x1Fu << 27)) | ((u32)dev.IntrIn.Dci << 27);
    /* Slot State and Device Address are controller-owned; the input copy
       must present them as zero. */
    slot[3] = 0;

    u32* ep = (u32*)EndpointContext(dev.InputCtx + ContextSize, dev.IntrIn.Dci);
    u32 maxEsit = dev.IntrIn.MaxPacket;

    ep[0] = ((u32)dev.IntrIn.Interval << 16) | ((maxEsit >> 16) << 24);
    ep[1] = (EpTypeInterruptIn << 3) |
            (3u << 1) |                            /* CErr = 3 */
            ((u32)dev.IntrIn.MaxPacket << 16);
    u64 deq = dev.IntrIn.TrRing.GetPhys() | (dev.IntrIn.TrRing.GetCycle() ? 1u : 0u);
    ep[2] = (u32)(deq & 0xFFFFFFFFu);
    ep[3] = (u32)(deq >> 32);
    ep[4] = (u32)dev.IntrIn.MaxPacket | ((maxEsit & 0xFFFFu) << 16);

    Hal::DmaWmb();

    u32 code = RunCommand(dev.InputCtxPhys, 0,
        TrbTypeField(TrbConfigureEndpoint) | ((u32)dev.SlotId << 24), nullptr);
    if (code != CompSuccess)
    {
        Trace(0, "Xhci: configure endpoint slot %u failed, code %u",
            (ulong)dev.SlotId, (ulong)code);
        return false;
    }

    return true;
}

bool Controller::EnumeratePort(u8 port)
{
    u32 portsc = PortRead32(port, 0);
    if ((portsc & PortScConnected) == 0)
        return false;

    if (!ResetPort(port))
        return false;

    portsc = PortRead32(port, 0);
    u8 speed = (u8)((portsc >> PortScSpeedShift) & PortScSpeedMask);

    Trace(0, "Xhci: port %u connected, %s speed", (ulong)port, SpeedToStr(speed));

    {
        Stdlib::AutoLock lock(DumpLock);
        Ports[port].Speed = speed;
    }

    AttachInfo info;
    info.RootPort = port;
    info.Speed = speed;
    info.Route = 0;
    info.Tier = 0;
    info.TtSlot = 0;
    info.TtPort = 0;
    info.Mtt = false;
    info.ParentSlot = 0;
    info.ParentPort = 0;

    return EnumerateDevice(info) != nullptr;
}

/* Address a freshly reset device, read enough of its descriptors to classify
   it, and keep it only if it is something this driver drives: a boot-protocol
   keyboard, or a hub whose downstream ports may hold one. Anything else has
   its slot released again immediately, so a machine full of USB devices does
   not exhaust the device records.

   Returns the retained device, or nullptr if nothing was kept. */
Device* Controller::EnumerateDevice(const AttachInfo& info)
{
    Device* devp = AllocDevice();
    if (devp == nullptr)
    {
        Trace(0, "Xhci: no free device record for root port %u route 0x%p",
            (ulong)info.RootPort, (ulong)info.Route);
        return nullptr;
    }

    Device& dev = *devp;
    dev.Reset();
    dev.InUse = true;
    dev.RootPort = info.RootPort;
    dev.Speed = info.Speed;
    dev.Route = info.Route;
    dev.Tier = info.Tier;
    dev.TtSlot = info.TtSlot;
    dev.TtPort = info.TtPort;
    dev.Mtt = info.Mtt;
    dev.ParentSlot = info.ParentSlot;
    dev.ParentPort = info.ParentPort;
    dev.Ep0.Dci = 1;
    dev.Ep0.MaxPacket = DefaultMaxPacket0(info.Speed);

    u8 slotId = 0;
    u32 code = RunCommand(0, 0, TrbTypeField(TrbEnableSlot), &slotId);
    if (code != CompSuccess || slotId == 0 || slotId > MaxSlots)
    {
        Trace(0, "Xhci: enable slot failed for root port %u, code %u",
            (ulong)info.RootPort, (ulong)code);
        dev.Reset();
        return nullptr;
    }

    dev.SlotId = slotId;

    if (!dev.AllocDma())
    {
        Trace(0, "Xhci: out of memory for slot %u", (ulong)slotId);
        ReleaseDevice(dev);
        return nullptr;
    }

    Dcbaa[slotId] = dev.DevCtxPhys;
    Hal::DmaWmb();

    if (!AddressDevice(dev, false))
    {
        ReleaseDevice(dev);
        return nullptr;
    }

    /* Low/Full speed devices only reveal their real EP0 packet size in the
       first eight bytes of the device descriptor. */
    u32 got = 0;
    code = ControlTransfer(dev, ReqDirIn | ReqTypeStandard | ReqRecipDevice,
        ReqGetDescriptor, (u16)(DescDevice << 8), 0, 8, &got);
    if (code != CompSuccess || got < 8)
    {
        Trace(0, "Xhci: short device descriptor on slot %u, code %u got %u",
            (ulong)slotId, (ulong)code, (ulong)got);
        ReleaseDevice(dev);
        return nullptr;
    }

    const DeviceDescriptor* dd = (const DeviceDescriptor*)dev.Buf;
    u16 realMps = dd->MaxPacketSize0;
    if (info.Speed == SpeedSuper || info.Speed == SpeedSuperPlus)
        realMps = (u16)(1u << dd->MaxPacketSize0);

    if (realMps != 0 && realMps != dev.Ep0.MaxPacket)
    {
        Trace(UsbLL, "Xhci: slot %u ep0 mps %u -> %u",
            (ulong)slotId, (ulong)dev.Ep0.MaxPacket, (ulong)realMps);

        dev.Ep0.MaxPacket = realMps;

        /* Evaluate Context updates EP0's packet size without disturbing the
           address the device has already accepted. */
        Stdlib::MemSet(dev.InputCtx, 0, Const::PageSize);
        u32* icc = (u32*)dev.InputCtx;
        icc[0] = 0;
        icc[1] = (1u << 1);

        u32* ep0 = (u32*)EndpointContext(dev.InputCtx + ContextSize, 1);
        ep0[1] = (EpTypeControl << 3) | (3u << 1) | ((u32)realMps << 16);

        Hal::DmaWmb();

        code = RunCommand(dev.InputCtxPhys, 0,
            TrbTypeField(TrbEvaluateContext) | ((u32)slotId << 24), nullptr);
        if (code != CompSuccess)
            Trace(0, "Xhci: evaluate context slot %u failed, code %u",
                (ulong)slotId, (ulong)code);
    }

    code = ControlTransfer(dev, ReqDirIn | ReqTypeStandard | ReqRecipDevice,
        ReqGetDescriptor, (u16)(DescDevice << 8), 0, sizeof(DeviceDescriptor), &got);
    if (code != CompSuccess || got < sizeof(DeviceDescriptor))
    {
        Trace(0, "Xhci: device descriptor read failed on slot %u, code %u",
            (ulong)slotId, (ulong)code);
        ReleaseDevice(dev);
        return nullptr;
    }

    dd = (const DeviceDescriptor*)dev.Buf;
    dev.VendorId = dd->VendorId;
    dev.ProductId = dd->ProductId;
    dev.DeviceClass = dd->DeviceClass;

    Trace(0, "Xhci: slot %u device %04x:%04x class %u route 0x%p tier %u",
        (ulong)slotId, (ulong)dev.VendorId, (ulong)dev.ProductId,
        (ulong)dev.DeviceClass, (ulong)dev.Route, (ulong)dev.Tier);

    if (info.Tier == 0 && info.RootPort < MaxPorts)
    {
        Stdlib::AutoLock lock(DumpLock);
        Ports[info.RootPort].VendorId = dev.VendorId;
        Ports[info.RootPort].ProductId = dev.ProductId;
        Ports[info.RootPort].DeviceClass = dev.DeviceClass;
    }

    /* Configuration descriptor: header first for its total length, then the
       whole thing so the interface and endpoint descriptors can be walked. */
    code = ControlTransfer(dev, ReqDirIn | ReqTypeStandard | ReqRecipDevice,
        ReqGetDescriptor, (u16)(DescConfiguration << 8), 0,
        sizeof(ConfigDescriptor), &got);
    if (code != CompSuccess || got < sizeof(ConfigDescriptor))
    {
        Trace(0, "Xhci: config descriptor header failed, code %u", (ulong)code);
        ReleaseDevice(dev);
        return nullptr;
    }

    const ConfigDescriptor* cd = (const ConfigDescriptor*)dev.Buf;
    u16 totalLength = cd->TotalLength;
    u8 configValue = cd->ConfigurationValue;

    if (totalLength > Const::PageSize)
        totalLength = Const::PageSize;

    code = ControlTransfer(dev, ReqDirIn | ReqTypeStandard | ReqRecipDevice,
        ReqGetDescriptor, (u16)(DescConfiguration << 8), 0, totalLength, &got);
    if (code != CompSuccess || got < sizeof(ConfigDescriptor))
    {
        Trace(0, "Xhci: config descriptor read failed, code %u", (ulong)code);
        ReleaseDevice(dev);
        return nullptr;
    }

    bool isKeyboard = ParseConfiguration(dev, got);
    bool isHub = (dev.DeviceClass == ClassHub) && (info.Tier < MaxTier);

    if (!isKeyboard && !isHub)
    {
        Trace(0, "Xhci: slot %u is neither keyboard nor hub, releasing",
            (ulong)slotId);
        ReleaseDevice(dev);
        return nullptr;
    }

    code = ControlTransfer(dev, ReqDirOut | ReqTypeStandard | ReqRecipDevice,
        ReqSetConfiguration, configValue, 0, 0, nullptr);
    if (code != CompSuccess)
    {
        Trace(0, "Xhci: set configuration %u failed, code %u",
            (ulong)configValue, (ulong)code);
        ReleaseDevice(dev);
        return nullptr;
    }

    if (isKeyboard)
    {
        if (!ConfigureKeyboard(dev))
        {
            ReleaseDevice(dev);
            return nullptr;
        }

        /* Boot protocol gives the fixed 8-byte report this driver decodes; a
           keyboard that only speaks report protocol would need a descriptor
           parser. Both of these are optional requests -- a device is allowed
           to STALL them -- so failures are logged, not fatal. */
        code = ControlTransfer(dev, ReqDirOut | ReqTypeClass | ReqRecipInterface,
            HidReqSetProtocol, HidProtocolBoot, dev.InterfaceNum, 0, nullptr);
        if (code != CompSuccess)
            Trace(0, "Xhci: set protocol(boot) failed, code %u", (ulong)code);

        code = ControlTransfer(dev, ReqDirOut | ReqTypeClass | ReqRecipInterface,
            HidReqSetIdle, 0, dev.InterfaceNum, 0, nullptr);
        if (code != CompSuccess)
            Trace(UsbLL, "Xhci: set idle failed, code %u", (ulong)code);

        if (info.Tier == 0 && info.RootPort < MaxPorts)
        {
            Stdlib::AutoLock lock(DumpLock);
            Ports[info.RootPort].SlotId = slotId;
            Ports[info.RootPort].Keyboard = true;
        }

        if (!SubmitReport(dev))
        {
            ReleaseDevice(dev);
            return nullptr;
        }

        Trace(0, "Xhci: keyboard ready on root port %u slot %u route 0x%p",
            (ulong)info.RootPort, (ulong)slotId, (ulong)dev.Route);
        return &dev;
    }

    if (!SetupHub(dev))
    {
        ReleaseDevice(dev);
        return nullptr;
    }

    ScanHubPorts(dev);

    /* An empty hub is kept anyway: its slot is what makes a keyboard plugged
       into it later reachable, and ScanPorts walks known hubs every rescan. */

    if (info.Tier == 0 && info.RootPort < MaxPorts)
    {
        Stdlib::AutoLock lock(DumpLock);
        Ports[info.RootPort].SlotId = slotId;
    }

    return &dev;
}

/* ------------------------------------------------------------------ */
/* Controller: hubs                                                    */
/* ------------------------------------------------------------------ */

bool Controller::HubPortStatus(Device& hub, u8 port, u16* status, u16* change)
{
    u32 got = 0;
    u32 code = ControlTransfer(hub, ReqDirIn | ReqTypeClass | ReqRecipOther,
        ReqGetStatus, 0, port, 4, &got);
    if (code != CompSuccess || got < 4)
    {
        Trace(UsbLL, "Xhci: hub slot %u port %u status failed, code %u",
            (ulong)hub.SlotId, (ulong)port, (ulong)code);
        return false;
    }

    /* The hub wrote this through DMA; order the reads after the completion */
    Hal::DmaRmb();

    *status = (u16)((u16)hub.Buf[0] | ((u16)hub.Buf[1] << 8));
    *change = (u16)((u16)hub.Buf[2] | ((u16)hub.Buf[3] << 8));
    return true;
}

bool Controller::HubSetPortFeature(Device& hub, u8 port, u16 feature)
{
    u32 code = ControlTransfer(hub, ReqDirOut | ReqTypeClass | ReqRecipOther,
        ReqSetFeature, feature, port, 0, nullptr);
    return code == CompSuccess;
}

bool Controller::HubClearPortFeature(Device& hub, u8 port, u16 feature)
{
    u32 code = ControlTransfer(hub, ReqDirOut | ReqTypeClass | ReqRecipOther,
        ReqClearFeature, feature, port, 0, nullptr);
    return code == CompSuccess;
}

bool Controller::HubResetPort(Device& hub, u8 port, u8* speedOut)
{
    if (!HubSetPortFeature(hub, port, HubFeaturePortReset))
    {
        Trace(0, "Xhci: hub slot %u port %u reset request failed",
            (ulong)hub.SlotId, (ulong)port);
        return false;
    }

    ulong deadline = NowMs() + PortResetTimeoutMs;
    u16 status = 0;
    u16 change = 0;

    for (;;)
    {
        if (!HubPortStatus(hub, port, &status, &change))
            return false;

        if ((change & HubPortChangeReset) != 0 ||
            ((status & HubPortStatusReset) == 0 && (status & HubPortStatusEnable) != 0))
            break;

        if (NowMs() >= deadline)
        {
            Trace(0, "Xhci: hub slot %u port %u reset timed out (status 0x%p)",
                (ulong)hub.SlotId, (ulong)port, (ulong)status);
            return false;
        }

        SleepMs(10);
    }

    HubClearPortFeature(hub, port, HubFeatureCPortReset);

    /* USB 2.0 TRSTRCY */
    SleepMs(10);

    if (!HubPortStatus(hub, port, &status, &change))
        return false;

    if ((status & HubPortStatusEnable) == 0)
    {
        Trace(0, "Xhci: hub slot %u port %u not enabled after reset (0x%p)",
            (ulong)hub.SlotId, (ulong)port, (ulong)status);
        return false;
    }

    if (hub.Speed == SpeedSuper || hub.Speed == SpeedSuperPlus)
        *speedOut = SpeedSuper;
    else if ((status & HubPortStatusLowSpeed) != 0)
        *speedOut = SpeedLow;
    else if ((status & HubPortStatusHighSpeed) != 0)
        *speedOut = SpeedHigh;
    else
        *speedOut = SpeedFull;

    return true;
}

/* Read the hub class descriptor, tell the controller this slot is a hub (the
   Hub / Number of Ports / TTT fields are only evaluated by a Configure
   Endpoint command), then switch on power to every downstream port. */
bool Controller::SetupHub(Device& hub)
{
    u8 descType = (hub.Speed == SpeedSuper || hub.Speed == SpeedSuperPlus)
        ? DescHubSuperSpeed : DescHub;

    u32 got = 0;
    u32 code = ControlTransfer(hub, ReqDirIn | ReqTypeClass | ReqRecipDevice,
        ReqGetDescriptor, (u16)(descType << 8), 0, 9, &got);
    if (code != CompSuccess || got < HubDescMinLength)
    {
        Trace(0, "Xhci: hub slot %u descriptor read failed, code %u got %u",
            (ulong)hub.SlotId, (ulong)code, (ulong)got);
        return false;
    }

    Hal::DmaRmb();

    u8 numPorts = hub.Buf[HubDescNumPorts];
    u16 characteristics = (u16)((u16)hub.Buf[HubDescCharacteristics] |
        ((u16)hub.Buf[HubDescCharacteristics + 1] << 8));

    /* The route string gives each tier one nibble, so port numbers above 15
       are unreachable however many the hub reports. */
    if (numPorts > 15)
        numPorts = 15;

    if (numPorts == 0)
    {
        Trace(0, "Xhci: hub slot %u reports no ports", (ulong)hub.SlotId);
        return false;
    }

    hub.IsHub = true;
    hub.HubPorts = numPorts;
    hub.HubTtt = (u8)((characteristics >> 5) & 0x3);
    hub.HubPowerOnDelayMs = (u16)((u16)hub.Buf[HubDescPowerOnDelay] * 2);

    Trace(0, "Xhci: hub slot %u: %u ports, ttt %u, power-on %u ms",
        (ulong)hub.SlotId, (ulong)hub.HubPorts, (ulong)hub.HubTtt,
        (ulong)hub.HubPowerOnDelayMs);

    Stdlib::MemSet(hub.InputCtx, 0, Const::PageSize);

    u32* icc = (u32*)hub.InputCtx;
    icc[0] = 0;
    icc[1] = 1u << 0;   /* slot context only */

    /* Seed from the controller's own output context so nothing else changes */
    Hal::DmaRmb();

    u8* inSlot = SlotContext(hub.InputCtx + ContextSize);
    Stdlib::MemCpy(inSlot, SlotContext(hub.DevCtx), ContextSize);

    u32* slot = (u32*)inSlot;
    slot[0] = slot[0] | (1u << 26);                          /* Hub */
    slot[1] = (slot[1] & 0x00FFFFFFu) | ((u32)hub.HubPorts << 24);
    slot[2] = (slot[2] & ~(3u << 16)) | ((u32)hub.HubTtt << 16);
    /* Slot State and Device Address are controller-owned */
    slot[3] = 0;

    Hal::DmaWmb();

    code = RunCommand(hub.InputCtxPhys, 0,
        TrbTypeField(TrbConfigureEndpoint) | ((u32)hub.SlotId << 24), nullptr);
    if (code != CompSuccess)
    {
        Trace(0, "Xhci: marking slot %u as a hub failed, code %u",
            (ulong)hub.SlotId, (ulong)code);
        return false;
    }

    bool anyPowered = false;
    for (u8 p = 1; p <= hub.HubPorts; p++)
    {
        if (HubSetPortFeature(hub, p, HubFeaturePortPower))
            anyPowered = true;
    }

    if (!anyPowered)
    {
        Trace(0, "Xhci: hub slot %u refused port power", (ulong)hub.SlotId);
        return false;
    }

    /* bPwrOn2PwrGood plus the attach debounce before status means anything */
    SleepMs((ulong)hub.HubPowerOnDelayMs + 100);

    return true;
}

void Controller::ScanHubPorts(Device& hub)
{
    for (u8 p = 1; p <= hub.HubPorts; p++)
    {
        u16 status = 0;
        u16 change = 0;

        if (!HubPortStatus(hub, p, &status, &change))
            continue;

        if ((change & HubPortChangeConnection) != 0)
            HubClearPortFeature(hub, p, HubFeatureCPortConnection);

        bool connected = (status & HubPortStatusConnection) != 0;
        bool known = (hub.HubEnumeratedMask & (1u << p)) != 0;

        if (!connected)
        {
            if (known)
            {
                hub.HubEnumeratedMask = hub.HubEnumeratedMask & ~(1u << p);

                for (ulong i = 0; i < MaxDevices; i++)
                {
                    if (Devices[i].InUse &&
                        Devices[i].ParentSlot == hub.SlotId &&
                        Devices[i].ParentPort == p)
                    {
                        Trace(0, "Xhci: hub slot %u port %u disconnected",
                            (ulong)hub.SlotId, (ulong)p);
                        ReleaseDevice(Devices[i]);
                    }
                }
            }
            continue;
        }

        if (known)
            continue;

        /* Attempted, successfully or not: only an unplug re-arms this port */
        hub.HubEnumeratedMask = hub.HubEnumeratedMask | (1u << p);

        u8 childSpeed = SpeedInvalid;
        if (!HubResetPort(hub, p, &childSpeed))
            continue;

        AttachInfo info;
        info.RootPort = hub.RootPort;
        info.Speed = childSpeed;
        info.Route = hub.Route | ((u32)(p & 0xF) << (4 * hub.Tier));
        info.Tier = (u8)(hub.Tier + 1);
        info.ParentSlot = hub.SlotId;
        info.ParentPort = p;

        /* Split transactions terminate at the nearest high-speed hub; deeper
           low/full-speed hubs inherit that hub's translator. */
        if (hub.Speed == SpeedHigh && (childSpeed == SpeedLow || childSpeed == SpeedFull))
        {
            info.TtSlot = hub.SlotId;
            info.TtPort = p;
            info.Mtt = hub.Mtt;
        }
        else
        {
            info.TtSlot = hub.TtSlot;
            info.TtPort = hub.TtPort;
            info.Mtt = false;
        }

        Trace(0, "Xhci: hub slot %u port %u: %s speed device",
            (ulong)hub.SlotId, (ulong)p, SpeedToStr(childSpeed));

        EnumerateDevice(info);
    }
}

void Controller::ReleaseDevice(Device& dev)
{
    /* Children first: a device behind this hub is unreachable once the hub's
       slot is gone, and its own slot would leak. */
    if (dev.SlotId != 0)
    {
        u8 slot = dev.SlotId;

        for (ulong i = 0; i < MaxDevices; i++)
        {
            if (&Devices[i] != &dev && Devices[i].InUse &&
                Devices[i].ParentSlot == slot)
            {
                ReleaseDevice(Devices[i]);
            }
        }

        RunCommand(0, 0, TrbTypeField(TrbDisableSlot) | ((u32)slot << 24), nullptr);

        if (Dcbaa != nullptr && slot <= MaxSlots)
            Dcbaa[slot] = 0;
    }

    u8 port = dev.RootPort;
    u8 tier = dev.Tier;

    dev.Reset();

    /* Only a root-port device owns the port record */
    if (tier != 0)
        return;

    Stdlib::AutoLock lock(DumpLock);
    if (port != 0 && port < MaxPorts)
    {
        Ports[port].Enumerated = false;
        Ports[port].SlotId = 0;
        Ports[port].Keyboard = false;
    }
}

void Controller::ScanPorts()
{
    PortChangePending = false;

    /* Hubs do not raise root-port events for their own downstream ports, so
       walk the hubs we already know about on every scan. */
    for (ulong i = 0; i < MaxDevices; i++)
    {
        if (Devices[i].InUse && Devices[i].IsHub)
            ScanHubPorts(Devices[i]);
    }

    for (u8 port = 1; port <= NumPorts && port < MaxPorts; port++)
    {
        u32 v = PortRead32(port, 0);
        if (v == 0xFFFFFFFFu)
            continue;

        if ((v & PortScChangeMask) != 0)
            PortWrite32(port, 0, PortScBase(v) | (v & PortScChangeMask));

        bool connected = (v & PortScConnected) != 0;
        bool wasEnumerated;

        {
            Stdlib::AutoLock lock(DumpLock);
            Ports[port].Connected = connected;
            wasEnumerated = Ports[port].Enumerated;
        }

        if (connected && !wasEnumerated)
        {
            EnumeratePort(port);

            /* Attempted, successfully or not: only an unplug re-arms it */
            Stdlib::AutoLock lock(DumpLock);
            Ports[port].Enumerated = true;
        }
        else if (!connected && wasEnumerated)
        {
            Trace(0, "Xhci: port %u disconnected", (ulong)port);

            for (ulong i = 0; i < MaxDevices; i++)
            {
                if (Devices[i].InUse && Devices[i].RootPort == port)
                    ReleaseDevice(Devices[i]);
            }

            Stdlib::AutoLock lock(DumpLock);
            Ports[port].Enumerated = false;
            Ports[port].Keyboard = false;
            Ports[port].SlotId = 0;
        }
    }
}

/* ------------------------------------------------------------------ */
/* Controller: keyboard servicing                                      */
/* ------------------------------------------------------------------ */

bool Controller::SubmitReport(Device& dev)
{
    Endpoint& ep = dev.IntrIn;

    if (!ep.TrRing.IsReady() || ep.Dci == 0)
        return false;

    u32 len = ep.MaxPacket;
    if (len == 0 || len > 64)
        len = HidBootKeyboard::ReportSize;

    Stdlib::MemSet(dev.ReportBuf, 0, len);

    ep.Residual = 0;
    ep.Completion = CompInvalid;
    ep.Done = false;

    ulong trb = ep.TrRing.Push(dev.ReportBufPhys, len,
        TrbTypeField(TrbNormal) | TrbIoc | TrbIsp);
    if (trb == 0)
        return false;

    ep.DataTrb = trb;
    ep.PendingTrb = trb;
    ep.InFlight = true;

    Doorbell(dev.SlotId, ep.Dci);
    return true;
}

void Controller::PumpKeyboards()
{
    ulong now = 0;
    Stdlib::Time t = GetBootTime();
    now = t.GetValue();

    for (ulong i = 0; i < MaxDevices; i++)
    {
        Device& dev = Devices[i];
        if (!dev.InUse || !dev.IsKeyboard)
            continue;

        Endpoint& ep = dev.IntrIn;

        if (ep.InFlight && ep.Done)
        {
            u32 code = ep.Completion;
            u32 len = ep.MaxPacket;
            if (len > ep.Residual)
                len = len - ep.Residual;
            else
                len = 0;

            ep.InFlight = false;
            ep.Done = false;
            ep.PendingTrb = 0;

            if (code == CompSuccess || code == CompShortPacket)
            {
                dev.Reports++;
                dev.Kbd.OnReport(dev.ReportBuf, len);
            }
            else if (code == CompStallError)
            {
                dev.Errors++;
                Trace(0, "Xhci: keyboard slot %u endpoint stalled, resetting",
                    (ulong)dev.SlotId);

                RunCommand(0, 0, TrbTypeField(TrbResetEndpoint) |
                    ((u32)ep.Dci << 16) | ((u32)dev.SlotId << 24), nullptr);

                /* Point the controller at the producer's current position,
                   not at the head: the ring behind the enqueue pointer holds
                   consumed TRBs the controller must not replay. */
                u64 deq = ep.TrRing.GetEnqueuePhys() | (ep.TrRing.GetCycle() ? 1u : 0u);
                RunCommand(deq, 0, TrbTypeField(TrbSetTrDequeue) |
                    ((u32)ep.Dci << 16) | ((u32)dev.SlotId << 24), nullptr);

                ControlTransfer(dev, ReqDirOut | ReqTypeStandard | ReqRecipEndpoint,
                    ReqClearFeature, FeatureEndpointHalt, ep.Address, 0, nullptr);

                dev.Kbd.Reset();
            }
            else
            {
                dev.Errors++;
                Trace(UsbLL, "Xhci: keyboard slot %u transfer code %u",
                    (ulong)dev.SlotId, (ulong)code);
            }
        }

        if (!ep.InFlight)
        {
            if (!SubmitReport(dev))
            {
                Trace(0, "Xhci: cannot resubmit report for slot %u",
                    (ulong)dev.SlotId);
                continue;
            }
        }

        dev.Kbd.OnTick(now);
    }
}

void Controller::Poll()
{
    if (!Ready)
        return;

    ProcessEvents();

    /* Hot-plug normally arrives as a Port Status Change event; the periodic
       rescan is a backstop for controllers that drop one. */
    ulong now = NowMs();
    if (PortChangePending || now - LastScanMs >= PortRescanPeriodMs)
    {
        LastScanMs = now;
        ScanPorts();
    }

    PumpKeyboards();
}

/* ------------------------------------------------------------------ */
/* Controller: statics                                                 */
/* ------------------------------------------------------------------ */

void Controller::InitAll()
{
    auto& pci = Pci::GetInstance();

    for (ulong i = 0; i < pci.GetDeviceCount() && InstanceCount < MaxControllers; i++)
    {
        Pci::DeviceInfo* dev = pci.GetDevice(i);
        if (dev == nullptr)
            break;

        /* Serial bus controller / USB / xHCI programming interface */
        if (dev->Class != Pci::ClsSerialBus || dev->SubClass != Pci::SubClsUSB)
            continue;
        if (dev->ProgIF != 0x30)
            continue;

        Controller* ctrl = new (Mm::NoThrow) Controller();
        if (ctrl == nullptr)
        {
            Trace(0, "Xhci: out of memory for controller");
            break;
        }

        if (!ctrl->Init(dev))
        {
            delete ctrl;
            continue;
        }

        Instances[InstanceCount] = ctrl;
        InstanceCount++;
    }

    Trace(0, "Xhci: initialized %u controllers", InstanceCount);
}

void Controller::PollAll()
{
    for (ulong i = 0; i < InstanceCount; i++)
    {
        if (Instances[i] != nullptr)
            Instances[i]->Poll();
    }
}

void Controller::DumpSelf(Stdlib::Printer& printer)
{
    Stdlib::AutoLock lock(DumpLock);

    printer.Printf("xhci %04x:%04x hci %u.%u slots %u ports %u ctx %u\n",
        (ulong)(PciDev ? PciDev->Vendor : 0), (ulong)(PciDev ? PciDev->Device : 0),
        (ulong)(HciVersion >> 8), (ulong)(HciVersion & 0xFF),
        (ulong)MaxSlots, (ulong)NumPorts, (ulong)ContextSize);

    for (u8 port = 1; port <= NumPorts && port < MaxPorts; port++)
    {
        if (!Ports[port].Connected)
            continue;

        printer.Printf("  port %u: %s speed %04x:%04x class %u slot %u%s\n",
            (ulong)port, SpeedToStr(Ports[port].Speed),
            (ulong)Ports[port].VendorId, (ulong)Ports[port].ProductId,
            (ulong)Ports[port].DeviceClass, (ulong)Ports[port].SlotId,
            Ports[port].Keyboard ? " [boot keyboard]" : "");
    }

    for (ulong i = 0; i < MaxDevices; i++)
    {
        const Device& dev = Devices[i];
        if (!dev.InUse || !dev.IsKeyboard)
            continue;

        printer.Printf("  keyboard slot %u: ep 0x%p reports %u errors %u\n",
            (ulong)dev.SlotId, (ulong)dev.IntrIn.Address,
            (ulong)dev.Reports, (ulong)dev.Errors);
    }
}

void Controller::Dump(Stdlib::Printer& printer)
{
    if (InstanceCount == 0)
    {
        printer.Printf("no xhci controllers\n");
        return;
    }

    for (ulong i = 0; i < InstanceCount; i++)
    {
        if (Instances[i] != nullptr)
            Instances[i]->DumpSelf(printer);
    }
}

/* ------------------------------------------------------------------ */
/* USB task                                                            */
/* ------------------------------------------------------------------ */

static const ulong UsbTaskTag = 'Usb ';

static Task* UsbTask;
static bool UsbActive;

static void UsbTaskFunc(void* ctx)
{
    (void)ctx;

    while (!Task::GetCurrentTask()->IsStopping())
    {
        Controller::PollAll();
        SleepMs(PollPeriodMs);
    }
}

void Init()
{
    Controller::InitAll();
}

bool Start()
{
    if (UsbTask != nullptr)
        return false;

    Task* task = Mm::TAlloc<Task, UsbTaskTag>("usb");
    if (task == nullptr)
        return false;

    UsbTask = task;

    if (!task->Start(&UsbTaskFunc, nullptr))
    {
        UsbTask = nullptr;
        task->Put();
        return false;
    }

    UsbActive = true;
    return true;
}

void Stop()
{
    if (!UsbActive)
        return;

    UsbActive = false;

    if (UsbTask != nullptr)
    {
        UsbTask->SetStopping();
        UsbTask->Wait();
    }
}

}
}
