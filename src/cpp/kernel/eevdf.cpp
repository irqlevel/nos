#include "eevdf.h"

#include <kernel/panic.h>

namespace Kernel
{

namespace
{

/* Linux's sched_prio_to_weight: nice -20 to 19, each step about 1.25 times
   the one after, so that one nice level apart is about ten per cent of the
   CPU between two tasks that both want all of it */
const ulong NiceWeights[] = {
    /* -20 */ 88761, 71755, 56483, 46273, 36291,
    /* -15 */ 29154, 23254, 18705, 14949, 11916,
    /* -10 */  9548,  7620,  6100,  4904,  3906,
    /*  -5 */  3121,  2501,  1991,  1586,  1277,
    /*   0 */  1024,   820,   655,   526,   423,
    /*   5 */   335,   272,   215,   172,   137,
    /*  10 */   110,    87,    70,    56,    45,
    /*  15 */    36,    29,    23,    18,    15,
};

static_assert(sizeof(NiceWeights) / sizeof(NiceWeights[0]) ==
              (ulong)(SchedEntity::NiceMax - SchedEntity::NiceMin + 1), "a weight for every nice");

/* The tick, as Linux's lag limit has it: a task can run a tick past where it
   should have been stopped */
const ulong TickNs = 10 * Const::NanoSecsInMs;

/* VMult's fixed point, and the weight's: Nice0Weight is 2^10 */
const ulong VMultShift = 32;
const ulong WeightShift = 10;
static_assert(SchedEntity::Nice0Weight == (1UL << WeightShift), "nice 0's weight is 2^10");

/* Wraps as VRuntime does: a after b by less than half the range */
bool After(u64 a, u64 b)
{
    return (s64)(a - b) > 0;
}

}

SchedEntity::SchedEntity()
    : Weight(Nice0Weight)
    , VMult(1UL << VMultShift)
    , Nice(0)
    , SliceNs(DefaultSliceNs)
    , VRuntime(0)
    , Deadline(0)
    , VLag(0)
    , Placed(false)
    , PlacedLag(0)
    , ExecStart(0)
{
    Entry.Init();
}

void SchedEntity::SetNice(long nice)
{
    nice = Stdlib::Max(NiceMin, Stdlib::Min(NiceMax, nice));
    Nice = nice;
    Weight = NiceWeights[nice - NiceMin];
    VMult = (Nice0Weight << VMultShift) / Weight;
}

void SchedEntity::SetSlice(ulong sliceNs)
{
    SliceNs = Stdlib::Max(MinSliceNs, Stdlib::Min(MaxSliceNs, sliceNs));
}

/* ns at most a few seconds and VMult at most 2^42 (nice 19): the product
   fits 128 bits with room, and the result 64 */
u64 SchedEntity::VirtualOf(ulong ns) const
{
    return (u64)(((unsigned __int128)ns * VMult) >> VMultShift);
}

ulong SchedEntity::RealOf(u64 v) const
{
    return (ulong)(((unsigned __int128)v * Weight) >> WeightShift);
}

u64 SchedEntity::VSlice() const
{
    return VirtualOf(SliceNs);
}

s64 SchedEntity::LagLimit() const
{
    return (s64)VirtualOf(Stdlib::Max(2 * SliceNs, TickNs));
}

EevdfQueue::EevdfQueue()
    : Count(0)
    , Curr(nullptr)
    , LastV(0)
{
    List.Init();
}

bool EevdfQueue::IsEmpty()
{
    return List.IsEmpty();
}

SchedEntity* EevdfQueue::Current()
{
    return Curr;
}

ulong EevdfQueue::GetCount()
{
    return Count;
}

void EevdfQueue::Join(SchedEntity* se)
{
    BugOn(se == Curr || !se->Entry.IsEmpty());

    Place(se);
    List.InsertTail(&se->Entry);
    Count++;
}

void EevdfQueue::JoinCurrent(SchedEntity* se)
{
    BugOn(Curr != nullptr || !se->Entry.IsEmpty());

    Place(se);
    Curr = se;
}

void EevdfQueue::Leave(SchedEntity* se)
{
    /* On the queue: current, or on the list */
    BugOn(se != Curr && se->Entry.IsEmpty());

    /* V with se in it */
    s64 lag = (s64)(V() - se->VRuntime);
    s64 limit = se->LagLimit();
    se->VLag = Stdlib::Max(-limit, Stdlib::Min(limit, lag));

    if (se == Curr)
    {
        Curr = nullptr;
        return;
    }
    se->Entry.RemoveInit();
    Count--;
}

void EevdfQueue::PutBack()
{
    BugOn(Curr == nullptr);

    List.InsertTail(&Curr->Entry);
    Count++;
    Curr = nullptr;
}

void EevdfQueue::Take(SchedEntity* se)
{
    BugOn(Curr != nullptr || se->Entry.IsEmpty());

    se->Entry.RemoveInit();
    Count--;
    Curr = se;
}

void EevdfQueue::Clear()
{
    while (!List.IsEmpty())
        List.RemoveHead()->Init();
    Count = 0;
    Curr = nullptr;
}

void EevdfQueue::Sum(Sums& sums)
{
    sums.Base = 0;
    sums.Weighted = 0;
    sums.Load = 0;

    /* The base first, the smallest VRuntime, so that every difference
       below is not negative */
    bool any = false;
    if (Curr != nullptr)
    {
        sums.Base = Curr->VRuntime;
        any = true;
    }
    for (auto entry = List.Flink; entry != &List; entry = entry->Flink)
    {
        auto* se = CONTAINING_RECORD(entry, SchedEntity, Entry);
        if (!any || After(sums.Base, se->VRuntime))
            sums.Base = se->VRuntime;
        any = true;
    }

    /* In 128 bits, and kept to 64 after: a weight below 2^17 times a span
       of at most 2^40 is below 2^57 a term, and only past a hundred tasks
       spread that far apart would 64 not hold them -- a schedule gone
       wrong, which saturating does not make worse */
    unsigned __int128 weighted = 0;
    if (Curr != nullptr)
    {
        weighted += (unsigned __int128)Curr->Weight * Stdlib::Min(Curr->VRuntime - sums.Base, MaxSpan);
        sums.Load += Curr->Weight;
    }
    for (auto entry = List.Flink; entry != &List; entry = entry->Flink)
    {
        auto* se = CONTAINING_RECORD(entry, SchedEntity, Entry);
        weighted += (unsigned __int128)se->Weight * Stdlib::Min(se->VRuntime - sums.Base, MaxSpan);
        sums.Load += se->Weight;
    }

    const unsigned __int128 most = ~(u64)0;
    sums.Weighted = (u64)((weighted > most) ? most : weighted);
}

u64 EevdfQueue::Average(const Sums& sums)
{
    return sums.Base + sums.Weighted / sums.Load;
}

/* VRuntime at or before V, without the division V takes: (v - base) * load
   at most the weighted sum (Linux's vruntime_eligible). In 128 bits: a span
   of 2^40 times a load of tens of tasks at 88761 each is past 64. */
bool EevdfQueue::Eligible(const SchedEntity* se, const Sums& sums)
{
    u64 span = Stdlib::Min(se->VRuntime - sums.Base, MaxSpan);
    return (unsigned __int128)span * sums.Load <= (unsigned __int128)sums.Weighted;
}

u64 EevdfQueue::V()
{
    Sums sums;
    Sum(sums);
    if (sums.Load == 0)
        return LastV;

    LastV = Average(sums);
    return LastV;
}

void EevdfQueue::Place(SchedEntity* se)
{
    Sums sums;
    Sum(sums);

    bool initial = !se->Placed;
    se->Placed = true;

    s64 lag = initial ? 0 : se->VLag;
    se->PlacedLag = lag;
    u64 v;
    if (sums.Load == 0)
    {
        /* Nothing to be fair to: it starts where the last there was left V */
        v = LastV;
    }
    else
    {
        u64 average = Average(sums);
        LastV = average;

        /* Its lag made larger by the weight it adds, so that it still has it
           once V has moved for that weight: lag * (load + w) / load. The lag
           is within its limit, a few seconds of virtual time at most, and
           the load tens of tasks of at most 88761 -- clamped to keep the
           product in 64 bits. */
        lag = Stdlib::Max(-se->LagLimit(), Stdlib::Min(se->LagLimit(), lag));
        u64 load = Stdlib::Min(sums.Load, (u64)1 << 24);
        lag = (lag * (s64)(load + se->Weight)) / (s64)load;
        v = average - (u64)lag;
    }

    se->VRuntime = v;
    u64 vslice = se->VSlice();
    se->Deadline = v + (initial ? vslice / 2 : vslice);
    se->VLag = 0;
}

bool EevdfQueue::Charge(u64 now)
{
    BugOn(Curr == nullptr);

    if (now > Curr->ExecStart)
    {
        Curr->VRuntime += Curr->VirtualOf(now - Curr->ExecStart);
        Curr->ExecStart = now;
    }

    if (After(Curr->Deadline, Curr->VRuntime))
        return false;

    Curr->Deadline = Curr->VRuntime + Curr->VSlice();
    return true;
}

SchedEntity* EevdfQueue::Pick(bool withCurrent)
{
    Sums sums;
    Sum(sums);
    if (sums.Load == 0)
        return nullptr;
    LastV = Average(sums);

    SchedEntity* best = nullptr;
    SchedEntity* soonest = nullptr;

    if (withCurrent && Curr != nullptr)
    {
        soonest = Curr;
        if (Eligible(Curr, sums))
            best = Curr;
    }

    for (auto entry = List.Flink; entry != &List; entry = entry->Flink)
    {
        auto* se = CONTAINING_RECORD(entry, SchedEntity, Entry);
        if (soonest == nullptr || After(soonest->Deadline, se->Deadline))
            soonest = se;
        if (Eligible(se, sums) && (best == nullptr || After(best->Deadline, se->Deadline)))
            best = se;
    }

    /* Some entity is always at or before the average, so with the current
       one among the candidates one is always eligible; without it, it may
       be the only one, and then the soonest deadline of the rest goes */
    return (best != nullptr) ? best : soonest;
}

void EevdfQueue::Yield()
{
    BugOn(Curr == nullptr);

    u64 bound = V() + Curr->VSlice();
    if (After(bound, Curr->Deadline))
        Curr->Deadline = bound;
}

bool EevdfQueue::IsEligible(const SchedEntity* se)
{
    Sums sums;
    Sum(sums);
    return sums.Load != 0 && Eligible(se, sums);
}

ulong EevdfQueue::RealUntilDeadline()
{
    BugOn(Curr == nullptr);

    if (!After(Curr->Deadline, Curr->VRuntime))
        return 0;

    return Curr->RealOf(Curr->Deadline - Curr->VRuntime);
}

}
