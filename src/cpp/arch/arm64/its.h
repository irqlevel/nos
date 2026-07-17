#pragma once

#include <include/types.h>

namespace Kernel
{

class InterruptHandler;

/* GICv3 ITS: translates device MSIs (DeviceID + EventID) into LPIs.
   Provisions the device + collection tables and a command queue, sets up
   the per-CPU LPI configuration/pending tables, and maps one collection to
   the boot CPU (all LPIs delivered there — no MSI balancing yet). */
class Its final
{
public:
    static Its& GetInstance()
    {
        static Its Instance;
        return Instance;
    }

    /* BSP once: map registers, provision tables + command queue, enable
       LPIs on this CPU's redistributor and the ITS. gicrBase/gicrSize come
       from the GIC driver (redistributor region for LPI enable). */
    bool Setup(ulong itsPhys, ulong gicrBase, ulong gicrSize);

    bool IsReady() const { return Ready; }

    /* Map a device (PCI RID) with room for `numEvents` events. Idempotent
       per deviceId. Returns false on failure. */
    bool MapDevice(u32 deviceId, u32 numEvents);

    /* Allocate an LPI and map (deviceId, eventId) -> LPI to the boot
       collection; registers `handler` for that LPI. Returns the MSI data
       value (the eventId) and fills msiAddr with GITS_TRANSLATER; returns
       0 on failure. Caller writes {msiAddr, eventId} into the MSI-X entry. */
    u32 MapEvent(u32 deviceId, u32 eventId, InterruptHandler& handler,
        u64& msiAddr);

    /* Dispatch an incoming LPI (INTID >= 8192) to its handler. */
    void HandleLpi(u32 intId);

    static const u32 LpiIntIdBase = 8192;

private:
    Its() = default;
    ~Its() = default;
    Its(const Its& other) = delete;
    Its(Its&& other) = delete;
    Its& operator=(const Its& other) = delete;
    Its& operator=(Its&& other) = delete;

    bool ProvisionTables();
    bool SetupCommandQueue();
    bool EnableLpisOnRedist(ulong gicrBase, ulong gicrSize);

    void CmdMapd(u32 deviceId, u64 ittPhys, u32 sizeBits, bool valid);
    void CmdMapc(u16 icid, u64 rdBase, bool valid);
    void CmdMapti(u32 deviceId, u32 eventId, u32 lpiIntId, u16 icid);
    void CmdInv(u32 deviceId, u32 eventId);
    void CmdSync(u64 rdBase);
    void PostCommand(const u64 cmd[4]);
    bool WaitCommands();

    u32 AllocLpi();

    ulong ItsBase = 0;   /* mapped VA */
    ulong ItsPhys = 0;   /* physical base (for the MSI translate address) */
    bool Ready = false;

    /* Command queue */
    ulong CmdQueueVa = 0;
    ulong CmdQueuePhys = 0;
    ulong CmdQueueBytes = 0;
    ulong CmdWriteOff = 0;

    /* Collection -> boot redistributor */
    u16 BootIcid = 0;
    u64 BootRdBase = 0; /* RDbase encoding (PTA-dependent) */

    u32 EventIdBits = 0;
    u32 DeviceIdBits = 0;
    u32  IttEntrySize = 0;
    bool Pta = false;

    /* LPI allocation + config table */
    static const u32 MaxLpis = 64;
    u32 NextLpi = 0;
    ulong LpiConfigVa = 0;

    struct LpiHandler
    {
        InterruptHandler* Handler;
    };
    LpiHandler LpiHandlers[MaxLpis];

    /* Per-device ITT tracking (small: only NVMe/r8168 here) */
    static const u32 MaxDevices = 8;
    struct DeviceEntry
    {
        u32 DeviceId;
        bool Used;
    };
    DeviceEntry Devices[MaxDevices];
};

}
