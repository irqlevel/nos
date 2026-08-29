#pragma once

#include <include/types.h>
#include <include/const.h>
#include <drivers/pci.h>
#include <kernel/spin_lock.h>
#include <lib/printer.h>

#include "usb.h"
#include "hid_kbd.h"

namespace Kernel
{
namespace Usb
{

/* ------------------------------------------------------------------ */
/* Transfer Request Block                                              */
/* ------------------------------------------------------------------ */

struct Trb
{
    u64 Param;
    u32 Status;
    u32 Control;
} __attribute__((packed));

static_assert(sizeof(Trb) == 16, "bad TRB size");

/* TRB types, xHCI 1.2 table 6-91 */
enum TrbType : u32
{
    TrbNormal = 1,
    TrbSetupStage = 2,
    TrbDataStage = 3,
    TrbStatusStage = 4,
    TrbLink = 6,
    TrbEnableSlot = 9,
    TrbDisableSlot = 10,
    TrbAddressDevice = 11,
    TrbConfigureEndpoint = 12,
    TrbEvaluateContext = 13,
    TrbResetEndpoint = 14,
    TrbStopEndpoint = 15,
    TrbSetTrDequeue = 16,
    TrbNoOpCommand = 23,
    TrbTransferEvent = 32,
    TrbCommandCompletion = 33,
    TrbPortStatusChange = 34,
};

/* Completion codes, xHCI 1.2 table 6-90 */
enum TrbCompletion : u32
{
    CompInvalid = 0,
    CompSuccess = 1,
    CompDataBufferError = 2,
    CompBabbleDetected = 3,
    CompUsbTransactionError = 4,
    CompTrbError = 5,
    CompStallError = 6,
    CompResourceError = 7,
    CompBandwidthError = 8,
    CompNoSlotsAvailable = 9,
    CompShortPacket = 13,
    CompRingUnderrun = 14,
    CompRingOverrun = 15,
    CompParameterError = 17,
    CompContextStateError = 19,
    CompCommandRingStopped = 24,
};

/* TRB control-field bits shared by every type */
static const u32 TrbCycle = 1u << 0;
static const u32 TrbEnt = 1u << 1;
static const u32 TrbIsp = 1u << 2;
static const u32 TrbChain = 1u << 4;
static const u32 TrbIoc = 1u << 5;
static const u32 TrbIdt = 1u << 6;
static const u32 TrbToggleCycle = 1u << 1; /* Link TRB only */
static const u32 TrbDirIn = 1u << 16;      /* Data/Status Stage only */

static inline u32 TrbTypeField(u32 type)
{
    return (type & 0x3F) << 10;
}

static inline u32 TrbTypeOf(u32 control)
{
    return (control >> 10) & 0x3F;
}

/* ------------------------------------------------------------------ */
/* Rings                                                               */
/* ------------------------------------------------------------------ */

/* Producer ring (command ring or a transfer ring). One page of TRBs whose
   last entry is a Link TRB with Toggle Cycle set, so the ring wraps in
   hardware and the producer cycle state flips on every lap. */
class Ring
{
public:
    Ring();
    ~Ring();

    bool Init();
    void Deinit();

    bool IsReady() const { return Trbs != nullptr; }
    ulong GetPhys() const { return Phys; }
    /* Where the next TRB will land -- the value to hand Set TR Dequeue
       Pointer when recovering an endpoint, so the controller resumes where
       the producer actually is. */
    ulong GetEnqueuePhys() const { return Phys + (ulong)Enqueue * sizeof(Trb); }
    /* Dequeue Cycle State to publish in an endpoint/command ring pointer */
    u8 GetCycle() const { return Cycle; }

    /* Append one TRB, supplying the cycle bit. Returns the physical address
       of the slot written -- the completion event reports it back -- or 0
       when the ring is exhausted. */
    ulong Push(u64 param, u32 status, u32 control);

private:
    Ring(const Ring& other) = delete;
    Ring(Ring&& other) = delete;
    Ring& operator=(const Ring& other) = delete;
    Ring& operator=(Ring&& other) = delete;

    static const u32 NumTrbs = Const::PageSize / sizeof(Trb);

    Trb* Trbs;
    ulong Phys;
    u32 Enqueue;
    u8 Cycle;
};

/* Consumer ring. A single segment described by a one-entry ERST; software
   wraps by hand and toggles its consumer cycle state. */
class EventRing
{
public:
    EventRing();
    ~EventRing();

    bool Init();
    void Deinit();

    bool IsReady() const { return Trbs != nullptr; }
    ulong GetSegmentPhys() const { return Phys; }
    ulong GetErstPhys() const { return ErstPhys; }
    ulong GetDequeuePhys() const { return Phys + (ulong)Dequeue * sizeof(Trb); }

    /* Consume one event into *out. False when the ring is empty. */
    bool Pop(Trb* out);

private:
    EventRing(const EventRing& other) = delete;
    EventRing(EventRing&& other) = delete;
    EventRing& operator=(const EventRing& other) = delete;
    EventRing& operator=(EventRing&& other) = delete;

    static const u32 NumTrbs = Const::PageSize / sizeof(Trb);

    struct ErstEntry
    {
        u64 SegmentBase;
        u32 SegmentSize;
        u32 Reserved;
    } __attribute__((packed));

    Trb* Trbs;
    ulong Phys;
    ErstEntry* Erst;
    ulong ErstPhys;
    u32 Dequeue;
    u8 Cycle;
};

/* ------------------------------------------------------------------ */
/* Devices                                                             */
/* ------------------------------------------------------------------ */

struct Endpoint
{
    Ring TrRing;
    u8 Dci;          /* Device Context Index: 1 for EP0, 2*n+dir otherwise */
    u8 Address;      /* bEndpointAddress, 0 for the default control pipe */
    u16 MaxPacket;
    u8 Interval;     /* xHCI-encoded interval */
    bool InFlight;
    bool Done;
    u32 Completion;
    u32 Residual;
    ulong PendingTrb;   /* TRB whose completion ends the wait  */
    ulong DataTrb;      /* TRB whose residual is the byte count */

    Endpoint();

private:
    Endpoint(const Endpoint& other) = delete;
    Endpoint& operator=(const Endpoint& other) = delete;
};

class Controller;

/* Everything the slot context needs to describe where a device sits in the
   topology. Root-port devices have Route 0 and Tier 0; a device behind a hub
   inherits the root port and adds its own nibble to the route string. */
struct AttachInfo
{
    u8 RootPort;     /* 1-based root hub port carrying this branch */
    u8 Speed;
    u32 Route;       /* xHCI route string, 4 bits per hub tier */
    u8 Tier;         /* 0 = directly on a root port */
    u8 TtSlot;       /* slot of the nearest high-speed hub, 0 if none */
    u8 TtPort;       /* that hub's downstream port number */
    bool Mtt;
    u8 ParentSlot;   /* hub slot this device hangs off, 0 for a root port */
    u8 ParentPort;
};

class Device
{
public:
    Device();
    ~Device();

    bool InUse;
    u8 SlotId;
    u8 RootPort;     /* 1-based root hub port number */
    u8 Speed;
    u16 VendorId;
    u16 ProductId;
    u8 DeviceClass;
    u8 InterfaceNum;
    bool IsKeyboard;
    u32 Reports;     /* interrupt-IN reports accepted, for the `usb` dump */
    u32 Errors;      /* transfers that came back with a failure code      */

    /* Topology */
    u32 Route;
    u8 Tier;
    u8 TtSlot;
    u8 TtPort;
    bool Mtt;
    u8 ParentSlot;
    u8 ParentPort;

    /* Hub state, valid when IsHub */
    bool IsHub;
    u8 HubPorts;
    u8 HubTtt;
    u16 HubPowerOnDelayMs;
    u32 HubEnumeratedMask;  /* downstream ports already looked at */

    Endpoint Ep0;
    Endpoint IntrIn;

    /* Boot-protocol report decoder, valid when IsKeyboard */
    HidBootKeyboard Kbd;

    /* DMA structures. The input context is only touched during Address
       Device / Configure Endpoint; the device context is owned by the HC. */
    u8* InputCtx;
    ulong InputCtxPhys;
    u8* DevCtx;
    ulong DevCtxPhys;
    u8* Buf;         /* one page: control-transfer payloads */
    ulong BufPhys;
    u8* ReportBuf;   /* one page: interrupt-IN report landing zone */
    ulong ReportBufPhys;

    bool AllocDma();
    void FreeDma();
    void Reset();

private:
    Device(const Device& other) = delete;
    Device(Device&& other) = delete;
    Device& operator=(const Device& other) = delete;
    Device& operator=(Device&& other) = delete;
};

/* ------------------------------------------------------------------ */
/* Controller                                                          */
/* ------------------------------------------------------------------ */

/* Per-root-port bookkeeping, kept for the `usb` shell command even after a
   non-keyboard device has had its slot released. */
struct PortRecord
{
    bool Connected;
    bool Enumerated;
    u8 Speed;
    u8 SlotId;
    u16 VendorId;
    u16 ProductId;
    u8 DeviceClass;
    bool Keyboard;
};

class Controller
{
public:
    Controller();
    ~Controller();

    /* Bring up every xHCI PCI function found by the PCI scan. Must run in
       task context: the reset and port-reset paths sleep. */
    static void InitAll();

    /* Drain events, enumerate freshly connected ports and pump keyboard
       reports. Called from the USB task. */
    static void PollAll();

    static void Dump(Stdlib::Printer& printer);

    bool Init(Pci::DeviceInfo* dev);
    void Poll();
    void DumpSelf(Stdlib::Printer& printer);

    static const ulong MaxControllers = 4;
    static const ulong MaxDevices = 12;
    static const ulong MaxPorts = 64;

private:
    Controller(const Controller& other) = delete;
    Controller(Controller&& other) = delete;
    Controller& operator=(const Controller& other) = delete;
    Controller& operator=(Controller&& other) = delete;

    /* --- register windows --- */
    u32 CapRead32(ulong off) const;
    u32 OpRead32(ulong off) const;
    void OpWrite32(ulong off, u32 val);
    void OpWrite64(ulong off, u64 val);
    u32 PortRead32(u8 port, ulong off) const;
    void PortWrite32(u8 port, ulong off, u32 val);
    u32 RtRead32(ulong off) const;
    void RtWrite32(ulong off, u32 val);
    void RtWrite64(ulong off, u64 val);
    void Doorbell(u8 slot, u32 value);

    /* --- bring-up --- */
    bool MapBar(Pci::DeviceInfo* dev);
    void TakeOwnershipFromBios();
    void ApplyIntelPortSwitch(Pci::DeviceInfo* dev);
    bool ResetController();
    bool SetupMemory();
    bool StartController();
    void PowerPorts();

    /* --- event handling --- */
    void ProcessEvents();
    void HandleTransferEvent(const Trb& ev);
    void HandleCommandEvent(const Trb& ev);

    /* --- commands --- */
    u32 RunCommand(u64 param, u32 status, u32 control, u8* slotIdOut);
    u32 WaitCommand(ulong trbPhys, u8* slotIdOut);

    /* --- transfers --- */
    u32 ControlTransfer(Device& dev, u8 requestType, u8 request, u16 value,
        u16 index, u16 length, u32* transferred);
    u32 WaitTransfer(Device& dev, Endpoint& ep, ulong trbPhys, ulong timeoutMs);

    /* --- enumeration --- */
    void ScanPorts();
    bool ResetPort(u8 port);
    bool EnumeratePort(u8 port);
    Device* EnumerateDevice(const AttachInfo& info);
    bool AddressDevice(Device& dev, bool setAddress);
    bool ConfigureKeyboard(Device& dev);
    bool ParseConfiguration(Device& dev, u32 length);
    void ReleaseDevice(Device& dev);

    /* --- hubs --- */
    bool HubPortStatus(Device& hub, u8 port, u16* status, u16* change);
    bool HubSetPortFeature(Device& hub, u8 port, u16 feature);
    bool HubClearPortFeature(Device& hub, u8 port, u16 feature);
    bool HubResetPort(Device& hub, u8 port, u8* speedOut);
    bool SetupHub(Device& hub);
    void ScanHubPorts(Device& hub);

    /* Route strings hold five nibbles; stopping at four keeps the recursion
       and every downstream port number encodable. */
    static const u8 MaxTier = 4;

    /* --- keyboard servicing --- */
    void PumpKeyboards();
    bool SubmitReport(Device& dev);

    Device* FindDeviceBySlot(u8 slot);
    Device* AllocDevice();

    /* --- context helpers --- */
    u8* SlotContext(u8* base) const;
    u8* EndpointContext(u8* base, u8 dci) const;

    bool Ready;
    Pci::DeviceInfo* PciDev;

    volatile u8* CapBase;
    volatile u8* OpBase;
    volatile u8* RtBase;
    volatile u8* DbBase;

    u8 CapLength;
    u16 HciVersion;
    u8 MaxSlots;
    u8 NumPorts;
    u16 MaxInterrupters;
    u32 ContextSize;     /* 32 or 64 bytes, from HCCPARAMS1.CSZ */
    u32 PageSizeBytes;
    bool Ac64;
    u16 ExtCapOffset;    /* in dwords, 0 when absent */

    u64* Dcbaa;
    ulong DcbaaPhys;
    u64* ScratchpadArray;
    ulong ScratchpadArrayPhys;
    void** ScratchpadPages;
    u32 ScratchpadCount;

    Ring CmdRing;
    EventRing EvtRing;

    /* One command in flight at a time; the USB task is the only issuer. */
    ulong PendingCmdTrb;
    bool CmdDone;
    u32 CmdCompletion;
    u8 CmdSlotId;

    bool PortChangePending;
    ulong LastScanMs;

    Device Devices[MaxDevices];
    PortRecord Ports[MaxPorts];

    /* Guards Ports[]/Devices[] metadata against a concurrent `usb` dump.
       The USB task is the only writer. */
    SpinLock DumpLock;

    static Controller* Instances[MaxControllers];
    static ulong InstanceCount;
};

/* Bring up every xHCI controller and enumerate what is already attached.
   Runs synchronously in the caller's task -- the reset and port-reset paths
   sleep -- so that a keyboard is live, and its bring-up traces are on the
   console, before the shell takes the console over. */
void Init();

/* Start the task that polls event rings, services keyboards and picks up
   hot-plug. Call after Init(). */
bool Start();
void Stop();

}
}
