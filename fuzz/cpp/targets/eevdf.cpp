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

using namespace Kernel;

typedef unsigned __int128 U128;

const size_t MaxEntities = 8;

struct Model
{
    SchedEntity Se;
    /* Never sleeps: held to its share at the end */
    bool Hog;
    /* Off the queue: asleep, or on another CPU */
    bool Away;
    /* Nanoseconds it has run, in all */
    uint64_t Ran;
};

std::vector<Model>* Entities;
EevdfQueue* Queue;
uint64_t Now;

bool After(uint64_t a, uint64_t b)
{
    return (int64_t)(a - b) > 0;
}

Model& ModelOf(const SchedEntity* se)
{
    for (auto& m : *Entities)
    {
        if (&m.Se == se)
            return m;
    }
    Fuzz::Finding(__FILE__, __LINE__, "an entity that is nobody's");
}

/* The model's V and eligibility, from scratch: every VRuntime on the queue
   counted -- the list and the current one -- relative to the smallest, in
   128 bits */
struct Exact
{
    uint64_t Base;
    U128 Weighted;
    U128 Load;
};

Exact Measure()
{
    Exact e = {0, 0, 0};
    bool any = false;
    for (auto& m : *Entities)
    {
        if (m.Away)
            continue;
        if (!any || After(e.Base, m.Se.VRuntime))
            e.Base = m.Se.VRuntime;
        any = true;
    }
    for (auto& m : *Entities)
    {
        if (m.Away)
            continue;
        e.Weighted += (U128)m.Se.Weight * (m.Se.VRuntime - e.Base);
        e.Load += m.Se.Weight;
    }
    return e;
}

bool ExactEligible(const SchedEntity& se, const Exact& e)
{
    return (U128)(se.VRuntime - e.Base) * e.Load <= e.Weighted;
}

uint64_t ExactV(const Exact& e)
{
    return e.Base + (uint64_t)(e.Weighted / e.Load);
}

/* What the model has on the queue the queue has: each on its list or
   current, never both or neither, and nothing else */
void CheckMembers()
{
    size_t listed = 0;
    for (auto& m : *Entities)
    {
        bool current = (&m.Se == Queue->Current());
        bool onList = !m.Se.Entry.IsEmpty();
        if (m.Away)
        {
            INVARIANT(!current && !onList, "an entity off the queue is %s", current ? "current" : "on its list");
            continue;
        }
        INVARIANT(current != onList, "an entity on the queue is %s",
                  current ? "current and on the list" : "neither current nor on the list");
        if (onList)
            listed++;
    }
    INVARIANT(listed == Queue->GetCount(), "the list counts %lu, the model %zu", Queue->GetCount(), listed);
}

void CheckPick(SchedEntity* picked, bool withCurr)
{
    const SchedEntity* curr = Queue->Current();
    Exact e = Measure();
    if (e.Load == 0)
    {
        INVARIANT(picked == nullptr, "a pick from nothing gave something");
        return;
    }

    /* The candidates: the list, and the current one if it is one */
    const SchedEntity* best = nullptr;
    bool anyEligible = false;
    for (auto& m : *Entities)
    {
        if (m.Away || (&m.Se == curr && !withCurr))
            continue;
        if (!ExactEligible(m.Se, e))
            continue;
        anyEligible = true;
        if (best == nullptr || After(best->Deadline, m.Se.Deadline))
            best = &m.Se;
    }

    if (withCurr || curr == nullptr)
        INVARIANT(anyEligible, "nothing on the queue is at or before its average");

    if (!anyEligible)
    {
        /* Only the current one is eligible, and it was left out: the
           soonest deadline of the rest */
        INVARIANT(picked != nullptr && picked != curr, "a pick without the current one gave nothing, or it");
        Fuzz::Reached("only the current one eligible, the rest's soonest");
        return;
    }

    INVARIANT(picked != nullptr, "an eligible entity and no pick");
    INVARIANT(ExactEligible(*picked, e), "picked one past V: vruntime %llu, V %llu",
              (unsigned long long)picked->VRuntime, (unsigned long long)ExactV(e));
    INVARIANT(picked->Deadline == best->Deadline,
              "picked a deadline %lld after the soonest eligible one",
              (long long)(int64_t)(picked->Deadline - best->Deadline));
}

/* The entity on the CPU runs for ns */
void Run(uint64_t ns)
{
    Now += ns;
    SchedEntity* curr = Queue->Current();
    if (curr == nullptr)
        return;

    Model& m = ModelOf(curr);
    uint64_t before = curr->VRuntime;
    Queue->Charge(Now);
    m.Ran += ns;

    /* What a nanosecond is worth at its weight, within rounding */
    U128 want = ((U128)ns * curr->VMult) >> 32;
    INVARIANT((U128)(curr->VRuntime - before) == want, "charged %llu for %llu ns, not %llu",
              (unsigned long long)(curr->VRuntime - before), (unsigned long long)ns, (unsigned long long)want);
}

/* m joins the queue -- onto its list, woken or moved back, or onto the CPU,
   the idle task starting to run something -- and once in has the lag it
   brought */
void Join(Model& m, bool current)
{
    Exact before = Measure();
    bool placed = m.Se.Placed;
    int64_t limit = m.Se.LagLimit();
    int64_t lag = placed ? std::max(-limit, std::min(limit, m.Se.VLag)) : 0;

    if (current)
    {
        Queue->JoinCurrent(&m.Se);
        m.Se.ExecStart = Now;
        Fuzz::Reached("a join onto the CPU");
    }
    else
    {
        Queue->Join(&m.Se);
    }
    m.Away = false;

    if (before.Load == 0)
    {
        Fuzz::Reached("a join onto an empty queue");
        return;
    }

    /* The inflation made up for what its own weight moved V by -- within
       the rounding of the divisions, a few virtual nanoseconds a weight */
    int64_t now = (int64_t)(ExactV(Measure()) - m.Se.VRuntime);
    int64_t off = now - lag;
    int64_t slack = 2 + (int64_t)(m.Se.Weight + before.Load) / (int64_t)before.Load;
    INVARIANT(off >= -slack && off <= slack, "joined with lag %lld and has %lld once in",
              (long long)lag, (long long)now);

    if (placed)
        Fuzz::Reached(lag > 0 ? "a join owed" : (lag < 0 ? "a join owing" : "a join at zero lag"));
}

/* m leaves the queue -- current, going to sleep or idling, or on the list,
   moving away -- with the lag V gives it, V counting everything on the
   queue as it is */
void Leave(Model& m)
{
    uint64_t v = ExactV(Measure());
    bool current = (&m.Se == Queue->Current());
    Queue->Leave(&m.Se);

    int64_t lag = (int64_t)(v - m.Se.VRuntime);
    int64_t limit = m.Se.LagLimit();
    int64_t want = std::max(-limit, std::min(limit, lag));
    int64_t off = m.Se.VLag - want;
    INVARIANT(off >= -1 && off <= 1, "left with lag %lld where V says %lld", (long long)m.Se.VLag,
              (long long)want);

    m.Away = true;
    Fuzz::Reached(current ? "a leave from the CPU" : "a leave from the list");
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

void Decide(Why why)
{
    SchedEntity* curr = Queue->Current();
    SchedEntity* picked = nullptr;
    if (curr == nullptr || why == Why::Preempt)
    {
        picked = Queue->Pick(true);
        CheckPick(picked, true);
        if (picked == nullptr || picked == curr)
            return;
        if (curr != nullptr)
            Queue->PutBack();
    }
    else if (why == Why::Yield)
    {
        if (Queue->IsEmpty())
            return;
        Queue->Yield();
        picked = Queue->Pick(false);
        CheckPick(picked, false);
        Queue->PutBack();
        Fuzz::Reached("a yield");
    }
    else
    {
        if (!Queue->IsEmpty())
        {
            picked = Queue->Pick(false);
            CheckPick(picked, false);
        }
        Leave(ModelOf(curr));
        Fuzz::Reached(picked != nullptr ? "a sleep with the next picked" : "a sleep with nothing to run");
    }

    if (picked == nullptr)
        return;
    Queue->Take(picked);
    picked->ExecStart = Now;
    Fuzz::Reached("a switch");
}

/* One of those off the queue, or nullptr */
Model* AwayOne(Fuzz::Input& in)
{
    std::vector<Model*> away;
    for (auto& m : *Entities)
    {
        if (m.Away)
            away.push_back(&m);
    }
    if (away.empty())
        return nullptr;
    return away[in.Below(away.size())];
}

/* One waiting on the list that is not a hog, or nullptr */
Model* ListedOne(Fuzz::Input& in)
{
    std::vector<Model*> listed;
    for (auto& m : *Entities)
    {
        if (!m.Away && &m.Se != Queue->Current() && !m.Hog)
            listed.push_back(&m);
    }
    if (listed.empty())
        return nullptr;
    return listed[in.Below(listed.size())];
}

void Reset()
{
}

void RunInput(Fuzz::Input& in)
{
    std::vector<Model> entities(2 + in.Below(MaxEntities - 1));
    EevdfQueue queue;
    Entities = &entities;
    Queue = &queue;
    Now = 1000000 + in.Below(1000000);

    /* A third of the inputs all hogs, to be held to their shares at the
       end; the rest a mix, held to every decision */
    bool allHogs = in.Below(3) == 0;
    const long nices[] = {-20, -10, -5, -1, 0, 0, 0, 1, 5, 10, 19};
    const uint64_t slices[] = {100000, 500000, 1000000, 3000000, 3000000, 10000000};
    for (auto& m : entities)
    {
        m.Se.SetNice(in.Bool() ? in.Pick(nices) : (long)in.Range(0, 39) - 20);
        m.Se.SetSlice(in.Pick(slices));
        m.Hog = allHogs || in.Chance(160);
        m.Away = true;
        m.Ran = 0;
    }

    /* Everyone in, in turn: each placed new, the first onto nothing */
    for (auto& m : entities)
        Join(m, false);
    Decide(Why::Preempt);
    CheckMembers();

    /* The quantum a decision comes after: a tick, a slice's end, or a
       wakeup's moment */
    const uint64_t TickNs = 10000000;
    uint64_t steps = 200 + in.Below(4000);
    for (uint64_t step = 0; step < steps; step++)
    {
        uint64_t quantum;
        switch (in.Below(4))
        {
        case 0:
            quantum = TickNs;
            break;
        case 1:
            quantum = (Queue->Current() != nullptr) ? std::max<uint64_t>(1, Queue->RealUntilDeadline()) : TickNs;
            break;
        default:
            quantum = 1 + in.Below(2 * TickNs);
            break;
        }
        Run(quantum);

        /* Those away may come back; one waiting may move away; the CPU, with
           nothing counted on it, may start to run something of its own */
        if (in.Chance(64))
        {
            for (auto& m : entities)
            {
                if (m.Away && in.Chance(128))
                    Join(m, false);
            }
        }
        if (!allHogs && in.Chance(32))
        {
            if (Model* m = ListedOne(in))
                Leave(*m);
        }
        if (Queue->Current() == nullptr && in.Chance(48))
        {
            if (Model* m = AwayOne(in))
                Join(*m, true);
        }

        /* A sleep only of one that is not a hog, and a yield only among the
           mixed: a hog that yields gives its turn away, which its share is
           not held to. The current one may idle in place too, as the idle
           task does: gone from the count before the decision is made. */
        SchedEntity* curr = Queue->Current();
        bool mayStop = (curr != nullptr && !ModelOf(curr).Hog);
        if (mayStop && in.Chance(48))
        {
            Decide(Why::Block);
        }
        else if (mayStop && in.Chance(96))
        {
            Leave(ModelOf(curr));
            Decide(Why::Preempt);
        }
        else
        {
            Decide((!allHogs && in.Chance(24)) ? Why::Yield : Why::Preempt);
        }
        CheckMembers();
    }

    /* All of them hogs, and run long enough for the slices and the quanta
       to even out: each has had the CPU in proportion to its weight, within
       what a slice and two quanta each, a few times over, can leave
       uneven */
    if (!allHogs)
        return;

    uint64_t weight = 0, ran = 0;
    for (auto& m : entities)
    {
        weight += m.Se.Weight;
        ran += m.Ran;
    }
    if (ran < 1000 * TickNs)
        return;

    for (auto& m : entities)
    {
        double want = (double)m.Se.Weight / weight;
        double got = (double)m.Ran / ran;
        uint64_t uneven = 0;
        for (auto& o : entities)
            uneven += o.Se.SliceNs + 2 * TickNs;
        double slack = (double)(4 * uneven) / ran + 0.01;
        INVARIANT(got >= want - slack && got <= want + slack,
                  "nice %ld, slice %lu us: had %.4f of the CPU, its weight says %.4f (slack %.4f)",
                  m.Se.Nice, m.Se.SliceNs / 1000, got, want, slack);
    }
    Fuzz::Reached("hogs held to their shares");
}

}

const Fuzz::Target Fuzz::TheTarget = {"eevdf", Reset, RunInput, 4096, 20000};
