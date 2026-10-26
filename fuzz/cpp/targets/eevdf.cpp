// The scheduler's rules (kernel/eevdf.cpp), held to what EEVDF says: one CPU's
// queue, a handful of entities of random weights and slices, some always able
// to run and some that sleep and wake or move away and come back, and time
// going by a quantum at a time -- a tick, a slice's end, a wakeup -- with
// every decision made in the order the kernel's TaskQueue::Schedule makes it:
// the next picked while the last is still current, the last gone or back on
// the list, the next taken. Each decision is checked against a model that
// computes it from scratch in 128 bits: the one picked is eligible -- at or
// before V, the weighted average of every VRuntime on the queue -- and no
// eligible one has a sooner deadline. One that joins has, once it is in, the
// lag it left with, clamped -- what the lag's inflation is for; one that
// leaves takes the lag V gives it, V counting everything on the queue, the
// one picked to run next among them; one that has run is charged what its
// weight says; and every entity the model has on the queue the queue has, on
// its list or current, and none other. And over a run long enough of tasks
// that never sleep, each has had the CPU in proportion to its weight: a share
// out of bounds is a finding, as is a sanitizer's report or a kernel panic --
// the queue's BugOns on a call out of its order.
#include "host.h"

#include <kernel/eevdf.h>

#include "fuzz.h"

#include <vector>

namespace
{

using namespace kernel;

typedef unsigned __int128 U128;

const size_t MAX_ENTITIES = 8;

struct Model
{
    SchedEntity se;
    /* Never sleeps: held to its share at the end */
    bool hog;
    /* Off the queue: asleep, or on another CPU */
    bool away;
    /* Nanoseconds it has run, in all */
    uint64_t ran;
};

std::vector<Model>* entities;
EevdfQueue* queue;
uint64_t now;

bool after(uint64_t a, uint64_t b)
{
    return (int64_t)(a - b) > 0;
}

Model& model_of(const SchedEntity* se)
{
    for (auto& m : *entities)
    {
        if (&m.se == se)
            return m;
    }
    fuzz::finding(__FILE__, __LINE__, "an entity that is nobody's");
}

/* The model's V and eligibility, from scratch: every VRuntime on the queue
   counted -- the list and the current one -- relative to the smallest, in
   128 bits */
struct Exact
{
    uint64_t base;
    U128 weighted;
    U128 load;
};

Exact measure()
{
    Exact e = {0, 0, 0};
    bool any = false;
    for (auto& m : *entities)
    {
        if (m.away)
            continue;
        if (!any || after(e.base, m.se.vruntime))
            e.base = m.se.vruntime;
        any = true;
    }
    for (auto& m : *entities)
    {
        if (m.away)
            continue;
        e.weighted += (U128)m.se.weight * (m.se.vruntime - e.base);
        e.load += m.se.weight;
    }
    return e;
}

bool exact_eligible(const SchedEntity& se, const Exact& e)
{
    return (U128)(se.vruntime - e.base) * e.load <= e.weighted;
}

uint64_t exact_v(const Exact& e)
{
    return e.base + (uint64_t)(e.weighted / e.load);
}

/* What the model has on the queue the queue has: each on its list or
   current, never both or neither, and nothing else */
void check_members()
{
    size_t listed = 0;
    for (auto& m : *entities)
    {
        bool current = (&m.se == queue->current());
        bool on_list = !m.se.entry.is_empty();
        if (m.away)
        {
            INVARIANT(!current && !on_list, "an entity off the queue is %s", current ? "current" : "on its list");
            continue;
        }
        INVARIANT(current != on_list, "an entity on the queue is %s",
                  current ? "current and on the list" : "neither current nor on the list");
        if (on_list)
            listed++;
    }
    INVARIANT(listed == queue->get_count(), "the list counts %lu, the model %zu", queue->get_count(), listed);
}

void check_pick(SchedEntity* picked, bool with_curr)
{
    const SchedEntity* curr = queue->current();
    Exact e = measure();
    if (e.load == 0)
    {
        INVARIANT(picked == nullptr, "a pick from nothing gave something");
        return;
    }

    /* The candidates: the list, and the current one if it is one */
    const SchedEntity* best = nullptr;
    bool any_eligible = false;
    for (auto& m : *entities)
    {
        if (m.away || (&m.se == curr && !with_curr))
            continue;
        if (!exact_eligible(m.se, e))
            continue;
        any_eligible = true;
        if (best == nullptr || after(best->deadline, m.se.deadline))
            best = &m.se;
    }

    if (with_curr || curr == nullptr)
        INVARIANT(any_eligible, "nothing on the queue is at or before its average");

    if (!any_eligible)
    {
        /* Only the current one is eligible, and it was left out: the
           soonest deadline of the rest */
        INVARIANT(picked != nullptr && picked != curr, "a pick without the current one gave nothing, or it");
        fuzz::reached("only the current one eligible, the rest's soonest");
        return;
    }

    INVARIANT(picked != nullptr, "an eligible entity and no pick");
    INVARIANT(exact_eligible(*picked, e), "picked one past V: vruntime %llu, V %llu",
              (unsigned long long)picked->vruntime, (unsigned long long)exact_v(e));
    INVARIANT(picked->deadline == best->deadline,
              "picked a deadline %lld after the soonest eligible one",
              (long long)(int64_t)(picked->deadline - best->deadline));
}

/* The entity on the CPU runs for ns */
void run(uint64_t ns)
{
    now += ns;
    SchedEntity* curr = queue->current();
    if (curr == nullptr)
        return;

    Model& m = model_of(curr);
    uint64_t before = curr->vruntime;
    queue->charge(now);
    m.ran += ns;

    /* What a nanosecond is worth at its weight, within rounding */
    U128 want = ((U128)ns * curr->vmult) >> 32;
    INVARIANT((U128)(curr->vruntime - before) == want, "charged %llu for %llu ns, not %llu",
              (unsigned long long)(curr->vruntime - before), (unsigned long long)ns, (unsigned long long)want);
}

/* m joins the queue -- onto its list, woken or moved back, or onto the CPU,
   the idle task starting to run something -- and once in has the lag it
   brought */
void join(Model& m, bool current)
{
    Exact before = measure();
    bool placed = m.se.placed;
    int64_t limit = m.se.lag_limit();
    int64_t lag = placed ? std::max(-limit, std::min(limit, m.se.vlag)) : 0;

    if (current)
    {
        queue->join_current(&m.se);
        m.se.exec_start = now;
        fuzz::reached("a join onto the CPU");
    }
    else
    {
        queue->join(&m.se);
    }
    m.away = false;

    if (before.load == 0)
    {
        fuzz::reached("a join onto an empty queue");
        return;
    }

    /* The inflation made up for what its own weight moved V by -- within
       the rounding of the divisions, a few virtual nanoseconds a weight */
    int64_t lag_in = (int64_t)(exact_v(measure()) - m.se.vruntime);
    int64_t off = lag_in - lag;
    int64_t slack = 2 + (int64_t)(m.se.weight + before.load) / (int64_t)before.load;
    INVARIANT(off >= -slack && off <= slack, "joined with lag %lld and has %lld once in",
              (long long)lag, (long long)lag_in);

    if (placed)
        fuzz::reached(lag > 0 ? "a join owed" : (lag < 0 ? "a join owing" : "a join at zero lag"));
}

/* m leaves the queue -- current, going to sleep or idling, or on the list,
   moving away -- with the lag V gives it, V counting everything on the
   queue as it is */
void leave(Model& m)
{
    uint64_t v = exact_v(measure());
    bool current = (&m.se == queue->current());
    queue->leave(&m.se);

    int64_t lag = (int64_t)(v - m.se.vruntime);
    int64_t limit = m.se.lag_limit();
    int64_t want = std::max(-limit, std::min(limit, lag));
    int64_t off = m.se.vlag - want;
    INVARIANT(off >= -1 && off <= 1, "left with lag %lld where V says %lld", (long long)m.se.vlag,
              (long long)want);

    m.away = true;
    fuzz::reached(current ? "a leave from the CPU" : "a leave from the list");
}

/* A decision, as TaskQueue::Schedule makes it */
enum class Why
{
    /* An interrupt's: the current one kept when it is best */
    Preempt,
    /* The current one lets the others go first, and goes back on the list */
    Yield,
    /* The current one goes to sleep: the next picked while it is still
       current, so that the V it leaves by counts the next */
    Block,
};

void decide(Why why)
{
    SchedEntity* curr = queue->current();
    SchedEntity* picked = nullptr;
    if (curr == nullptr || why == Why::Preempt)
    {
        picked = queue->pick(true);
        check_pick(picked, true);
        if (picked == nullptr || picked == curr)
            return;
        if (curr != nullptr)
            queue->put_back();
    }
    else if (why == Why::Yield)
    {
        if (queue->is_empty())
            return;
        queue->yield();
        picked = queue->pick(false);
        check_pick(picked, false);
        queue->put_back();
        fuzz::reached("a yield");
    }
    else
    {
        if (!queue->is_empty())
        {
            picked = queue->pick(false);
            check_pick(picked, false);
        }
        leave(model_of(curr));
        fuzz::reached(picked != nullptr ? "a sleep with the next picked" : "a sleep with nothing to run");
    }

    if (picked == nullptr)
        return;
    queue->take(picked);
    picked->exec_start = now;
    fuzz::reached("a switch");
}

/* One of those off the queue, or nullptr */
Model* away_one(fuzz::Input& in)
{
    std::vector<Model*> away;
    for (auto& m : *entities)
    {
        if (m.away)
            away.push_back(&m);
    }
    if (away.empty())
        return nullptr;
    return away[in.below(away.size())];
}

/* One waiting on the list that is not a hog, or nullptr */
Model* listed_one(fuzz::Input& in)
{
    std::vector<Model*> listed;
    for (auto& m : *entities)
    {
        if (!m.away && &m.se != queue->current() && !m.hog)
            listed.push_back(&m);
    }
    if (listed.empty())
        return nullptr;
    return listed[in.below(listed.size())];
}

void reset()
{
}

void run_input(fuzz::Input& in)
{
    std::vector<Model> models(2 + in.below(MAX_ENTITIES - 1));
    EevdfQueue run_queue;
    entities = &models;
    queue = &run_queue;
    now = 1000000 + in.below(1000000);

    /* A third of the inputs all hogs, to be held to their shares at the
       end; the rest a mix, held to every decision */
    bool all_hogs = in.below(3) == 0;
    const long nices[] = {-20, -10, -5, -1, 0, 0, 0, 1, 5, 10, 19};
    const uint64_t slices[] = {100000, 500000, 1000000, 3000000, 3000000, 10000000};
    for (auto& m : models)
    {
        m.se.set_nice(in.boolean() ? in.pick(nices) : (long)in.range(0, 39) - 20);
        m.se.set_slice(in.pick(slices));
        m.hog = all_hogs || in.chance(160);
        m.away = true;
        m.ran = 0;
    }

    /* Everyone in, in turn: each placed new, the first onto nothing */
    for (auto& m : models)
        join(m, false);
    decide(Why::Preempt);
    check_members();

    /* The quantum a decision comes after: a tick, a slice's end, or a
       wakeup's moment */
    const uint64_t tick_ns = 10000000;
    uint64_t steps = 200 + in.below(4000);
    for (uint64_t step = 0; step < steps; step++)
    {
        uint64_t quantum;
        switch (in.below(4))
        {
        case 0:
            quantum = tick_ns;
            break;
        case 1:
            quantum = (queue->current() != nullptr) ? std::max<uint64_t>(1, queue->real_until_deadline()) : tick_ns;
            break;
        default:
            quantum = 1 + in.below(2 * tick_ns);
            break;
        }
        run(quantum);

        /* Those away may come back; one waiting may move away; the CPU, with
           nothing counted on it, may start to run something of its own */
        if (in.chance(64))
        {
            for (auto& m : models)
            {
                if (m.away && in.chance(128))
                    join(m, false);
            }
        }
        if (!all_hogs && in.chance(32))
        {
            if (Model* m = listed_one(in))
                leave(*m);
        }
        if (queue->current() == nullptr && in.chance(48))
        {
            if (Model* m = away_one(in))
                join(*m, true);
        }

        /* A sleep only of one that is not a hog, and a yield only among the
           mixed: a hog that yields gives its turn away, which its share is
           not held to. The current one may idle in place too, as the idle
           task does: gone from the count before the decision is made. */
        SchedEntity* curr = queue->current();
        bool may_stop = (curr != nullptr && !model_of(curr).hog);
        if (may_stop && in.chance(48))
        {
            decide(Why::Block);
        }
        else if (may_stop && in.chance(96))
        {
            leave(model_of(curr));
            decide(Why::Preempt);
        }
        else
        {
            decide((!all_hogs && in.chance(24)) ? Why::Yield : Why::Preempt);
        }
        check_members();
    }

    /* All of them hogs, and run long enough for the slices and the quanta
       to even out: each has had the CPU in proportion to its weight, within
       what a slice and two quanta each, a few times over, can leave
       uneven */
    if (!all_hogs)
        return;

    uint64_t weight = 0, ran = 0;
    for (auto& m : models)
    {
        weight += m.se.weight;
        ran += m.ran;
    }
    if (ran < 1000 * tick_ns)
        return;

    for (auto& m : models)
    {
        double want = (double)m.se.weight / weight;
        double got = (double)m.ran / ran;
        uint64_t uneven = 0;
        for (auto& o : models)
            uneven += o.se.slice_ns + 2 * tick_ns;
        double slack = (double)(4 * uneven) / ran + 0.01;
        INVARIANT(got >= want - slack && got <= want + slack,
                  "nice %ld, slice %lu us: had %.4f of the CPU, its weight says %.4f (slack %.4f)",
                  m.se.nice, m.se.slice_ns / 1000, got, want, slack);
    }
    fuzz::reached("hogs held to their shares");
}

}

const fuzz::Target fuzz::THE_TARGET = {"eevdf", reset, run_input, 4096, 20000};
