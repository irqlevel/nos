#include "eevdf.h"

#include <kernel/panic.h>

namespace kernel
{

namespace
{

/* Linux's sched_prio_to_weight: nice -20 to 19, each step about 1.25 times
   the one after, so that one nice level apart is about ten per cent of the
   CPU between two tasks that both want all of it */
const ulong NICE_WEIGHTS[] = {
    /* -20 */ 88761, 71755, 56483, 46273, 36291,
    /* -15 */ 29154, 23254, 18705, 14949, 11916,
    /* -10 */  9548,  7620,  6100,  4904,  3906,
    /*  -5 */  3121,  2501,  1991,  1586,  1277,
    /*   0 */  1024,   820,   655,   526,   423,
    /*   5 */   335,   272,   215,   172,   137,
    /*  10 */   110,    87,    70,    56,    45,
    /*  15 */    36,    29,    23,    18,    15,
};

static_assert(sizeof(NICE_WEIGHTS) / sizeof(NICE_WEIGHTS[0]) ==
              (ulong)(SchedEntity::NICE_MAX - SchedEntity::NICE_MIN + 1), "a weight for every nice");

/* The tick, as Linux's lag limit has it: a task can run a tick past where it
   should have been stopped */
const ulong TICK_NS = 10 * consts::NANO_SECS_IN_MS;

/* VMult's fixed point, and the weight's: NICE0_WEIGHT is 2^10 */
const ulong VMULT_SHIFT = 32;
const ulong WEIGHT_SHIFT = 10;
static_assert(SchedEntity::NICE0_WEIGHT == (1UL << WEIGHT_SHIFT), "nice 0's weight is 2^10");

/* Wraps as VRuntime does: a after b by less than half the range */
bool after(u64 a, u64 b)
{
    return (s64)(a - b) > 0;
}

}

SchedEntity::SchedEntity()
    : weight(NICE0_WEIGHT)
    , vmult(1UL << VMULT_SHIFT)
    , nice(0)
    , slice_ns(DEFAULT_SLICE_NS)
    , vruntime(0)
    , deadline(0)
    , vlag(0)
    , placed(false)
    , placed_lag(0)
    , exec_start(0)
{
    entry.init();
}

void SchedEntity::set_nice(long value)
{
    value = stdlib::max(NICE_MIN, stdlib::min(NICE_MAX, value));
    nice = value;
    weight = NICE_WEIGHTS[value - NICE_MIN];
    vmult = (NICE0_WEIGHT << VMULT_SHIFT) / weight;
}

void SchedEntity::set_slice(ulong value)
{
    slice_ns = stdlib::max(MIN_SLICE_NS, stdlib::min(MAX_SLICE_NS, value));
}

/* ns at most a few seconds and VMult at most 2^42 (nice 19): the product
   fits 128 bits with room, and the result 64 */
u64 SchedEntity::virtual_of(ulong ns) const
{
    return (u64)(((unsigned __int128)ns * vmult) >> VMULT_SHIFT);
}

ulong SchedEntity::real_of(u64 v) const
{
    return (ulong)(((unsigned __int128)v * weight) >> WEIGHT_SHIFT);
}

u64 SchedEntity::vslice() const
{
    return virtual_of(slice_ns);
}

s64 SchedEntity::lag_limit() const
{
    return (s64)virtual_of(stdlib::max(2 * slice_ns, TICK_NS));
}

EevdfQueue::EevdfQueue()
    : count_(0)
    , curr_(nullptr)
    , last_v_(0)
{
    list_.init();
}

bool EevdfQueue::is_empty()
{
    return list_.is_empty();
}

SchedEntity* EevdfQueue::current()
{
    return curr_;
}

ulong EevdfQueue::get_count()
{
    return count_;
}

void EevdfQueue::join(SchedEntity* se)
{
    BUG_ON(se == curr_ || !se->entry.is_empty());

    place(se);
    list_.insert_tail(&se->entry);
    count_++;
}

void EevdfQueue::join_current(SchedEntity* se)
{
    BUG_ON(curr_ != nullptr || !se->entry.is_empty());

    place(se);
    curr_ = se;
}

void EevdfQueue::leave(SchedEntity* se)
{
    /* On the queue: current, or on the list */
    BUG_ON(se != curr_ && se->entry.is_empty());

    /* V with se in it */
    s64 lag = (s64)(v() - se->vruntime);
    s64 limit = se->lag_limit();
    se->vlag = stdlib::max(-limit, stdlib::min(limit, lag));

    if (se == curr_)
    {
        curr_ = nullptr;
        return;
    }
    se->entry.remove_init();
    count_--;
}

void EevdfQueue::put_back()
{
    BUG_ON(curr_ == nullptr);

    list_.insert_tail(&curr_->entry);
    count_++;
    curr_ = nullptr;
}

void EevdfQueue::take(SchedEntity* se)
{
    BUG_ON(curr_ != nullptr || se->entry.is_empty());

    se->entry.remove_init();
    count_--;
    curr_ = se;
}

void EevdfQueue::clear()
{
    while (!list_.is_empty())
        list_.remove_head()->init();
    count_ = 0;
    curr_ = nullptr;
}

void EevdfQueue::sum(Sums& sums)
{
    sums.base = 0;
    sums.weighted = 0;
    sums.load = 0;

    /* The base first, the smallest VRuntime, so that every difference
       below is not negative */
    bool any = false;
    if (curr_ != nullptr)
    {
        sums.base = curr_->vruntime;
        any = true;
    }
    for (auto entry = list_.flink; entry != &list_; entry = entry->flink)
    {
        auto* se = CONTAINING_RECORD(entry, SchedEntity, entry);
        if (!any || after(sums.base, se->vruntime))
            sums.base = se->vruntime;
        any = true;
    }

    /* In 128 bits, and kept to 64 after: a weight below 2^17 times a span
       of at most 2^40 is below 2^57 a term, and only past a hundred tasks
       spread that far apart would 64 not hold them -- a schedule gone
       wrong, which saturating does not make worse */
    unsigned __int128 weighted = 0;
    if (curr_ != nullptr)
    {
        weighted += (unsigned __int128)curr_->weight * stdlib::min(curr_->vruntime - sums.base, MAX_SPAN);
        sums.load += curr_->weight;
    }
    for (auto entry = list_.flink; entry != &list_; entry = entry->flink)
    {
        auto* se = CONTAINING_RECORD(entry, SchedEntity, entry);
        weighted += (unsigned __int128)se->weight * stdlib::min(se->vruntime - sums.base, MAX_SPAN);
        sums.load += se->weight;
    }

    const unsigned __int128 most = ~(u64)0;
    sums.weighted = (u64)((weighted > most) ? most : weighted);
}

u64 EevdfQueue::average(const Sums& sums)
{
    return sums.base + sums.weighted / sums.load;
}

/* VRuntime at or before V, without the division V takes: (v - base) * load
   at most the weighted sum (Linux's vruntime_eligible). In 128 bits: a span
   of 2^40 times a load of tens of tasks at 88761 each is past 64. */
bool EevdfQueue::eligible(const SchedEntity* se, const Sums& sums)
{
    u64 span = stdlib::min(se->vruntime - sums.base, MAX_SPAN);
    return (unsigned __int128)span * sums.load <= (unsigned __int128)sums.weighted;
}

u64 EevdfQueue::v()
{
    Sums sums;
    sum(sums);
    if (sums.load == 0)
        return last_v_;

    last_v_ = average(sums);
    return last_v_;
}

void EevdfQueue::place(SchedEntity* se)
{
    Sums sums;
    sum(sums);

    bool initial = !se->placed;
    se->placed = true;

    s64 lag = initial ? 0 : se->vlag;
    se->placed_lag = lag;
    u64 v;
    if (sums.load == 0)
    {
        /* Nothing to be fair to: it starts where the last there was left V */
        v = last_v_;
    }
    else
    {
        u64 avg = average(sums);
        last_v_ = avg;

        /* Its lag made larger by the weight it adds, so that it still has it
           once V has moved for that weight: lag * (load + w) / load. The lag
           is within its limit, a few seconds of virtual time at most, and
           the load tens of tasks of at most 88761 -- clamped to keep the
           product in 64 bits. */
        lag = stdlib::max(-se->lag_limit(), stdlib::min(se->lag_limit(), lag));
        u64 load = stdlib::min(sums.load, (u64)1 << 24);
        lag = (lag * (s64)(load + se->weight)) / (s64)load;
        v = avg - (u64)lag;
    }

    se->vruntime = v;
    u64 vslice = se->vslice();
    se->deadline = v + (initial ? vslice / 2 : vslice);
    se->vlag = 0;
}

bool EevdfQueue::charge(u64 now)
{
    BUG_ON(curr_ == nullptr);

    if (now > curr_->exec_start)
    {
        curr_->vruntime += curr_->virtual_of(now - curr_->exec_start);
        curr_->exec_start = now;
    }

    if (after(curr_->deadline, curr_->vruntime))
        return false;

    curr_->deadline = curr_->vruntime + curr_->vslice();
    return true;
}

SchedEntity* EevdfQueue::pick(bool with_current)
{
    Sums sums;
    sum(sums);
    if (sums.load == 0)
        return nullptr;
    last_v_ = average(sums);

    SchedEntity* best = nullptr;
    SchedEntity* soonest = nullptr;

    if (with_current && curr_ != nullptr)
    {
        soonest = curr_;
        if (eligible(curr_, sums))
            best = curr_;
    }

    for (auto entry = list_.flink; entry != &list_; entry = entry->flink)
    {
        auto* se = CONTAINING_RECORD(entry, SchedEntity, entry);
        if (soonest == nullptr || after(soonest->deadline, se->deadline))
            soonest = se;
        if (eligible(se, sums) && (best == nullptr || after(best->deadline, se->deadline)))
            best = se;
    }

    /* Some entity is always at or before the average, so with the current
       one among the candidates one is always eligible; without it, it may
       be the only one, and then the soonest deadline of the rest goes */
    return (best != nullptr) ? best : soonest;
}

void EevdfQueue::yield()
{
    BUG_ON(curr_ == nullptr);

    u64 bound = v() + curr_->vslice();
    if (after(bound, curr_->deadline))
        curr_->deadline = bound;
}

bool EevdfQueue::is_eligible(const SchedEntity* se)
{
    Sums sums;
    sum(sums);
    return sums.load != 0 && eligible(se, sums);
}

ulong EevdfQueue::real_until_deadline()
{
    BUG_ON(curr_ == nullptr);

    if (!after(curr_->deadline, curr_->vruntime))
        return 0;

    return curr_->real_of(curr_->deadline - curr_->vruntime);
}

}
