#pragma once

#include <include/types.h>

namespace kernel
{

class InterruptHandler;

/* GICv3 ITS: translates device MSIs (DeviceID + EventID) into LPIs.
   Provisions the device + collection tables and a command queue, sets up
   the per-CPU LPI configuration/pending tables, and maps one collection to
   the boot CPU (all LPIs delivered there — no MSI balancing yet). */
class Its final
{
public:
    static Its& get_instance()
    {
        static Its instance;
        return instance;
    }

    /* BSP once: map registers, provision tables + command queue, enable
       LPIs on this CPU's redistributor and the ITS. gicr_base/gicr_size come
       from the GIC driver (redistributor region for LPI enable). */
    bool setup(ulong its_phys, ulong gicr_base, ulong gicr_size);

    bool is_ready() const { return ready_; }

    /* Map a device (PCI RID) with room for `num_events` events. Idempotent
       per device_id. Returns false on failure. */
    bool map_device(u32 device_id, u32 num_events);

    /* Allocate an LPI and map (device_id, event_id) -> LPI to the boot
       collection; registers `handler` for that LPI. Returns the MSI data
       value (the event_id) and fills msi_addr with GITS_TRANSLATER; returns
       0 on failure. Caller writes {msi_addr, event_id} into the MSI-X entry. */
    u32 map_event(u32 device_id, u32 event_id, InterruptHandler& handler,
        u64& msi_addr);

    /* Dispatch an incoming LPI (INTID >= 8192) to its handler. */
    void handle_lpi(u32 int_id);

    static const u32 LPI_INT_ID_BASE = 8192;

private:
    Its() = default;
    ~Its() = default;
    Its(const Its& other) = delete;
    Its(Its&& other) = delete;
    Its& operator=(const Its& other) = delete;
    Its& operator=(Its&& other) = delete;

    bool provision_tables();
    bool setup_command_queue();
    bool enable_lpis_on_redist(ulong gicr_base, ulong gicr_size);

    void cmd_mapd(u32 device_id, u64 itt_phys, u32 size_bits, bool valid);
    void cmd_mapc(u16 icid, u64 rd_base, bool valid);
    void cmd_mapti(u32 device_id, u32 event_id, u32 lpi_int_id, u16 icid);
    void cmd_inv(u32 device_id, u32 event_id);
    void cmd_sync(u64 rd_base);
    void post_command(const u64 cmd[4]);
    bool wait_commands();

    u32 alloc_lpi();

    ulong its_base_ = 0;   /* mapped VA */
    ulong its_phys_ = 0;   /* physical base (for the MSI translate address) */
    bool ready_ = false;

    /* Command queue */
    ulong cmd_queue_va_ = 0;
    ulong cmd_queue_phys_ = 0;
    ulong cmd_queue_bytes_ = 0;
    ulong cmd_write_off_ = 0;

    /* Collection -> boot redistributor */
    u16 boot_icid_ = 0;
    u64 boot_rd_base_ = 0; /* RDbase encoding (PTA-dependent) */

    u32 event_id_bits_ = 0;
    u32 device_id_bits_ = 0;
    u32  itt_entry_size_ = 0;
    bool pta_ = false;

    /* LPI allocation + config table */
    static const u32 MAX_LPIS = 64;
    u32 next_lpi_ = 0;
    ulong lpi_config_va_ = 0;

    struct LpiHandler
    {
        InterruptHandler* handler;
    };
    LpiHandler lpi_handlers_[MAX_LPIS];

    /* Per-device ITT tracking (small: only NVMe/r8168 here) */
    static const u32 MAX_DEVICES = 8;
    struct DeviceEntry
    {
        u32 device_id;
        bool used;
    };
    DeviceEntry devices_[MAX_DEVICES];
};

}
