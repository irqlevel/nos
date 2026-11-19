#pragma once

#include <lib/stdlib.h>
#include "cpu.h"

namespace kernel
{

/*
 * The machine's NUMA topology: which CPUs and which memory are near each
 * other, and how far the rest is. On x86-64 the firmware says it in ACPI's
 * SRAT (a CPU or a range of memory in a proximity domain) and SLIT (the
 * distance between two domains); arm64 boots from a device tree and is one
 * node here.
 *
 * Written once, at boot, by the one CPU running -- Acpi::parse hands it the
 * entries as it reads them -- and then sealed, before the second CPU starts
 * and before the free pages are spread over the nodes (PageTable::spread_
 * free_pages). Read without a lock after that: nothing changes it again.
 * Until sealed, and on a machine whose tables say nothing, or say something
 * that does not hold together, it is one node holding every CPU and every
 * page -- what this kernel was before it knew of nodes, so that a wrong
 * table costs speed and never correctness.
 *
 * A node is an index, dense from 0: proximity domains are the firmware's
 * numbers, which need not be, and are taken in ascending order.
 */
class Numa final
{
public:
    static Numa& get_instance()
    {
        static Numa instance;
        return instance;
    }

    /* The most nodes kept: a machine of more is one node here, said */
    static const ulong MAX_NODES = 16;

    /* The most ranges of memory the SRAT may place -- a node has several,
       the low 2 GiB and the rest on a two-socket Xeon, and a table of more
       than this is one node here, said */
    static const ulong MAX_RANGES = 64;

    /* ACPI's distances (6.5, 5.2.17): a node's to itself is 10, and one
       without a SLIT is 20 to every other */
    static const u8 LOCAL_DISTANCE = 10;
    static const u8 REMOTE_DISTANCE = 20;
    /* What the SLIT says of two domains with no way between them */
    static const u8 UNREACHABLE_DISTANCE = 0xFF;

    /* What the SRAT says, entry by entry: a CPU, by its APIC id -- its index
       in CpuTable on x86-64 -- in a proximity domain; a range of memory in
       one. False for an entry not kept (an APIC id past MAX_CPUS, a range
       empty or wrapping, more domains or ranges than kept), which the caller
       says; an entry past a table this kernel cannot keep whole makes the
       machine one node at seal(). Before seal() only. */
    bool add_cpu(u32 domain, ulong apic_id);
    bool add_memory(u32 domain, ulong base, ulong len);

    /* The tables do not hold together -- one cut short, an entry of no
       length: whatever was said, the machine is one node, for `reason` */
    void refuse(const char* reason);

    /* The domains the SRAT named, ascending -- what a SLIT's rows and columns
       are looked up by, as the SRAT is read before it. Before seal() only. */
    ulong get_domain_count() const;
    u32 get_domain(ulong index) const;

    /* The SLIT's distance from the domain_count's index `from` to `to`:
       checked at seal(), the lot dropped for the defaults if any of them is
       not a distance (less than 10, the diagonal not 10, unreachable). */
    void set_distance(ulong from, ulong to, u8 distance);

    /* What was said, made the topology, once: `cpus` are the CPUs the MADT
       gave CpuTable, a bit each by index. A CPU the SRAT did not place is
       node 0's; so is memory no range of it covers. */
    void seal(ulong cpus);

    bool is_sealed() const;

    ulong get_node_count() const;

    /* The node of a CPU, by its index in CpuTable: 0 for one not placed */
    ulong get_cpu_node(ulong cpu) const;

    /* The node of a physical address, a page's: 0 for one no range covers.
       A binary search of a few ranges, under the page table's lock on every
       free -- and nothing at all on a machine of one node */
    ulong get_phys_node(ulong phys) const;

    /* The CPUs of a node, a bit each by index */
    ulong get_node_cpus(ulong node) const;

    /* Distance from one node to another, as the SLIT gives it (or the
       defaults): the local one is 10 */
    u8 get_distance(ulong from, ulong to) const;

    /* The nodes in the order an allocation meant for `node` tries them:
       itself first, then the nearest -- ties by index. `rank` from 0 to
       get_node_count() - 1 */
    ulong get_fallback(ulong node, ulong rank) const;

    /* The proximity domain a node was made of */
    u32 get_node_domain(ulong node) const;

    /* The memory the firmware placed, a range at a time, ascending:
       [base, end) of a node's. False past the last. */
    ulong get_range_count() const;
    bool get_range(ulong index, ulong& base, ulong& end, ulong& node) const;

    /* Why the machine is one node, when its tables named more: nullptr when
       it is not, or when they named one or none */
    const char* get_fallback_reason() const;

    /* What seal() made of it, into the kernel log: a line a node, and the
       distances */
    void report() const;

    /* `cpus`, a bit each, as runs -- "0-11,24-35", "none" -- into buf, cut
       to fit len and ended with a NUL whatever it is */
    static void format_cpus(ulong cpus, char* buf, size_t len);

private:
    Numa();
    ~Numa();
    Numa(const Numa& other) = delete;
    Numa(Numa&& other) = delete;
    Numa& operator=(const Numa& other) = delete;
    Numa& operator=(Numa&& other) = delete;

    /* domain's place in domain_, added in its order where it is not there
       yet: false when there is no room, or the SLIT has been read already
       and its distances are by the places as they were */
    bool add_domain(u32 domain);
    /* domain's place in domain_: there, after add_domain */
    ulong domain_place(u32 domain) const;
    void make_one_node(ulong cpus, const char* reason);
    /* The ranges sorted, a domain's touching ones merged: false where two
       domains' overlap */
    bool sort_ranges();
    bool distances_are_distances() const;
    void make_fallback_order();

    struct Range
    {
        ulong base;
        ulong end;
        u32 domain;
        /* The domain's node, from seal() on */
        ulong node;
    };

    bool sealed_;
    /* Something said that could not be kept: the machine is one node */
    const char* overflow_;
    const char* fallback_reason_;

    ulong domain_count_;
    u32 domain_[MAX_NODES];

    /* Each CPU's domain, as the SRAT said it, for those of cpu_said_ */
    u32 cpu_domain_[MAX_CPUS];
    ulong cpu_said_;

    /* From seal() on: each CPU's node, and each node's CPUs */
    u8 cpu_node_[MAX_CPUS];
    ulong node_cpus_[MAX_NODES];

    ulong node_count_;

    ulong range_count_;
    Range range_[MAX_RANGES];

    /* By the domains' places, which are the nodes from seal() on */
    u8 distance_[MAX_NODES][MAX_NODES];
    bool distances_said_;

    u8 fallback_[MAX_NODES][MAX_NODES];
};

}
