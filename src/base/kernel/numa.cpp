#include "numa.h"
#include "klog.h"
#include "panic.h"

namespace kernel
{

Numa::Numa()
    : sealed_(false)
    , overflow_(nullptr)
    , fallback_reason_(nullptr)
    , domain_count_(0)
    , cpu_said_(0)
    , node_count_(1)
    , range_count_(0)
    , distances_said_(false)
{
    for (ulong i = 0; i < MAX_NODES; i++)
    {
        domain_[i] = 0;
        node_cpus_[i] = 0;
        for (ulong j = 0; j < MAX_NODES; j++)
        {
            distance_[i][j] = 0;
            fallback_[i][j] = 0;
        }
    }
    distance_[0][0] = LOCAL_DISTANCE;
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        cpu_domain_[i] = 0;
        cpu_node_[i] = 0;
    }
    for (ulong i = 0; i < MAX_RANGES; i++)
        range_[i] = Range{ 0, 0, 0, 0 };
}

Numa::~Numa()
{
}

bool Numa::add_domain(u32 domain)
{
    ulong at = 0;
    while (at < domain_count_ && domain_[at] < domain)
        at++;
    if (at < domain_count_ && domain_[at] == domain)
        return true;

    if (domain_count_ == MAX_NODES || distances_said_)
        return false;

    for (ulong i = domain_count_; i > at; i--)
        domain_[i] = domain_[i - 1];
    domain_[at] = domain;
    domain_count_++;
    return true;
}

ulong Numa::domain_place(u32 domain) const
{
    for (ulong i = 0; i < domain_count_; i++)
    {
        if (domain_[i] == domain)
            return i;
    }
    /* add_domain put every domain of an entry kept here */
    PANIC("numa: domain %u not among the %lu named", domain, domain_count_);
    return 0;
}

bool Numa::add_cpu(u32 domain, ulong apic_id)
{
    BUG_ON(sealed_);

    if (apic_id >= MAX_CPUS)
        return false;

    if (!add_domain(domain))
    {
        overflow_ = distances_said_ ? "the SRAT read after the SLIT" : "more proximity domains than kept";
        return false;
    }

    const ulong bit = 1UL << apic_id;
    if ((cpu_said_ & bit) != 0 && cpu_domain_[apic_id] != domain)
    {
        overflow_ = "a CPU in two proximity domains";
        return false;
    }

    cpu_domain_[apic_id] = domain;
    cpu_said_ |= bit;
    return true;
}

bool Numa::add_memory(u32 domain, ulong base, ulong len)
{
    BUG_ON(sealed_);

    /* No memory, or a range past the end of the address space: nothing a
       page is in */
    if (len == 0 || base > ~0UL - len)
        return false;

    if (!add_domain(domain))
    {
        overflow_ = distances_said_ ? "the SRAT read after the SLIT" : "more proximity domains than kept";
        return false;
    }

    if (range_count_ == MAX_RANGES)
    {
        overflow_ = "more memory ranges than kept";
        return false;
    }

    range_[range_count_++] = Range{ base, base + len, domain, 0 };
    return true;
}

void Numa::refuse(const char* reason)
{
    BUG_ON(sealed_);
    if (overflow_ == nullptr)
        overflow_ = reason;
}

ulong Numa::get_domain_count() const
{
    return domain_count_;
}

u32 Numa::get_domain(ulong index) const
{
    BUG_ON(index >= domain_count_);
    return domain_[index];
}

void Numa::set_distance(ulong from, ulong to, u8 distance)
{
    BUG_ON(sealed_);
    BUG_ON(from >= domain_count_ || to >= domain_count_);
    distance_[from][to] = distance;
    distances_said_ = true;
}

bool Numa::sort_ranges()
{
    /* A few dozen at most: insertion */
    for (ulong i = 1; i < range_count_; i++)
    {
        const Range r = range_[i];
        ulong j = i;
        while (j > 0 && range_[j - 1].base > r.base)
        {
            range_[j] = range_[j - 1];
            j--;
        }
        range_[j] = r;
    }

    ulong kept = 0;
    for (ulong i = 0; i < range_count_; i++)
    {
        const Range& r = range_[i];
        if (kept != 0)
        {
            Range& last = range_[kept - 1];
            if (r.base < last.end && r.domain != last.domain)
                return false;
            /* Overlapping or touching, of one domain: one range */
            if (r.base <= last.end && r.domain == last.domain)
            {
                last.end = stdlib::max(last.end, r.end);
                continue;
            }
        }
        range_[kept++] = r;
    }
    range_count_ = kept;
    return true;
}

bool Numa::distances_are_distances() const
{
    for (ulong i = 0; i < domain_count_; i++)
    {
        for (ulong j = 0; j < domain_count_; j++)
        {
            const u8 d = distance_[i][j];
            if (i == j ? (d != LOCAL_DISTANCE) : (d < LOCAL_DISTANCE))
                return false;
        }
    }
    return true;
}

void Numa::make_fallback_order()
{
    for (ulong n = 0; n < node_count_; n++)
    {
        /* Itself first, whatever its distances say, then by distance and
           index -- insertion, of a handful */
        fallback_[n][0] = static_cast<u8>(n);
        ulong count = 1;
        for (ulong m = 0; m < node_count_; m++)
        {
            if (m == n)
                continue;
            ulong at = count;
            while (at > 1 && distance_[n][fallback_[n][at - 1]] > distance_[n][m])
            {
                fallback_[n][at] = fallback_[n][at - 1];
                at--;
            }
            fallback_[n][at] = static_cast<u8>(m);
            count++;
        }
    }
}

void Numa::make_one_node(ulong cpus, const char* reason)
{
    node_count_ = 1;
    fallback_reason_ = reason;
    for (ulong i = 0; i < MAX_CPUS; i++)
        cpu_node_[i] = 0;
    for (ulong i = 0; i < MAX_NODES; i++)
        node_cpus_[i] = 0;
    node_cpus_[0] = cpus;
    for (ulong i = 0; i < range_count_; i++)
        range_[i].node = 0;
    for (ulong i = 0; i < MAX_NODES; i++)
    {
        for (ulong j = 0; j < MAX_NODES; j++)
        {
            distance_[i][j] = 0;
            fallback_[i][j] = 0;
        }
    }
    distance_[0][0] = LOCAL_DISTANCE;
    sealed_ = true;
}

void Numa::seal(ulong cpus)
{
    BUG_ON(sealed_);

    if (overflow_ != nullptr)
    {
        make_one_node(cpus, overflow_);
        return;
    }

    if (!sort_ranges())
    {
        make_one_node(cpus, "two proximity domains' memory overlaps");
        return;
    }

    /* One domain, or a table naming none: one node, and nothing to say of it */
    if (domain_count_ <= 1)
    {
        make_one_node(cpus, nullptr);
        return;
    }

    node_count_ = domain_count_;

    for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
    {
        const ulong bit = 1UL << cpu;
        if ((cpus & bit) == 0)
            continue;
        const ulong node = ((cpu_said_ & bit) != 0) ? domain_place(cpu_domain_[cpu]) : 0;
        cpu_node_[cpu] = static_cast<u8>(node);
        node_cpus_[node] |= bit;
    }

    for (ulong i = 0; i < range_count_; i++)
    {
        Range& r = range_[i];
        r.node = domain_place(r.domain);
    }

    if (!distances_said_ || !distances_are_distances())
    {
        if (distances_said_)
            klog(0, "numa: the SLIT's distances are not distances (a diagonal not %lu, or less than it): "
                "%lu to every other node instead", (ulong)LOCAL_DISTANCE, (ulong)REMOTE_DISTANCE);
        for (ulong i = 0; i < node_count_; i++)
        {
            for (ulong j = 0; j < node_count_; j++)
                distance_[i][j] = (i == j) ? LOCAL_DISTANCE : REMOTE_DISTANCE;
        }
    }

    make_fallback_order();
    sealed_ = true;
}

bool Numa::is_sealed() const
{
    return sealed_;
}

ulong Numa::get_node_count() const
{
    return node_count_;
}

ulong Numa::get_cpu_node(ulong cpu) const
{
    if (cpu >= MAX_CPUS)
        return 0;
    return cpu_node_[cpu];
}

ulong Numa::get_current_node() const
{
    if (node_count_ == 1)
        return 0;
    return get_cpu_node(CpuTable::get_instance().get_current_cpu_id());
}

ulong Numa::get_phys_node(ulong phys) const
{
    if (node_count_ == 1)
        return 0;

    ulong lo = 0;
    ulong hi = range_count_;
    while (lo < hi)
    {
        const ulong mid = lo + (hi - lo) / 2;
        const Range& r = range_[mid];
        if (phys < r.base)
            hi = mid;
        else if (phys >= r.end)
            lo = mid + 1;
        else
            return r.node;
    }
    return 0;
}

ulong Numa::get_node_cpus(ulong node) const
{
    if (node >= node_count_)
        return 0;
    return node_cpus_[node];
}

u8 Numa::get_distance(ulong from, ulong to) const
{
    if (from >= node_count_ || to >= node_count_)
        return UNREACHABLE_DISTANCE;
    return distance_[from][to];
}

ulong Numa::get_fallback(ulong node, ulong rank) const
{
    BUG_ON(node >= node_count_ || rank >= node_count_);
    return fallback_[node][rank];
}

u32 Numa::get_node_domain(ulong node) const
{
    if (node_count_ == 1 || node >= node_count_)
        return 0;
    return domain_[node];
}

ulong Numa::get_range_count() const
{
    return range_count_;
}

bool Numa::get_range(ulong index, ulong& base, ulong& end, ulong& node) const
{
    if (index >= range_count_)
        return false;
    const Range& r = range_[index];
    base = r.base;
    end = r.end;
    node = r.node;
    return true;
}

const char* Numa::get_fallback_reason() const
{
    return fallback_reason_;
}

void Numa::format_cpus(ulong cpus, char* buf, size_t len)
{
    if (len == 0)
        return;
    buf[0] = '\0';

    size_t at = 0;
    bool first = true;
    for (ulong cpu = 0; cpu < MAX_CPUS; )
    {
        if ((cpus & (1UL << cpu)) == 0)
        {
            cpu++;
            continue;
        }
        ulong last = cpu;
        while (last + 1 < MAX_CPUS && (cpus & (1UL << (last + 1))) != 0)
            last++;

        const char* comma = first ? "" : ",";
        const int n = (last == cpu) ? stdlib::sn_printf(buf + at, len - at, "%s%lu", comma, cpu) :
            stdlib::sn_printf(buf + at, len - at, "%s%lu-%lu", comma, cpu, last);
        /* Cut: what fitted stays, ended by sn_printf */
        if (n < 0 || static_cast<size_t>(n) >= len - at)
            return;
        at += static_cast<size_t>(n);
        first = false;
        cpu = last + 1;
    }

    if (first)
        stdlib::sn_printf(buf, len, "none");
}

void Numa::report() const
{
    static const size_t CPU_TEXT_BYTES = 192;
    char text[CPU_TEXT_BYTES];

    if (node_count_ == 1)
    {
        if (fallback_reason_ != nullptr)
            klog(0, "numa: one node, the firmware's tables set aside: %s", fallback_reason_);
        else
            klog(0, "numa: one node");
        return;
    }

    for (ulong node = 0; node < node_count_; node++)
    {
        format_cpus(node_cpus_[node], text, sizeof(text));
        ulong bytes = 0;
        ulong ranges = 0;
        for (ulong i = 0; i < range_count_; i++)
        {
            const Range& r = range_[i];
            if (r.node != node)
                continue;
            bytes += r.end - r.base;
            ranges++;
        }
        klog(0, "numa: node %lu (domain %lu): CPUs %s (%lu), %lu MiB the firmware placed in %lu ranges", node,
            (ulong)domain_[node], text, (ulong)__builtin_popcountl(node_cpus_[node]), bytes / consts::MB, ranges);
    }

    /* A row a node: what one of 16 nodes fits in a line */
    for (ulong from = 0; from < node_count_; from++)
    {
        size_t at = 0;
        text[0] = '\0';
        for (ulong to = 0; to < node_count_ && at < sizeof(text); to++)
        {
            const int n = stdlib::sn_printf(text + at, sizeof(text) - at, " %lu", (ulong)distance_[from][to]);
            if (n < 0 || static_cast<size_t>(n) >= sizeof(text) - at)
                break;
            at += static_cast<size_t>(n);
        }
        klog(0, "numa: distances from node %lu:%s", from, text);
    }
}

}
