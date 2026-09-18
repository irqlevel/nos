#pragma once

#include <include/types.h>
#include <lib/printer.h>

namespace Kernel
{

struct NetDevice;

/* The UDP load target is Rust (src/rust/net/src/net_load.rs): the echo built
   in the receive callback itself, the batch's replies handed over together,
   the per-CPU counters and the line a second over the netconsole. What is
   left here is the way in.

   It exists to be hammered from outside, so that `profile` has something to
   look at other than an idle machine. */
class NetLoad final
{
public:
    static NetLoad& GetInstance()
    {
        static NetLoad Instance;
        return Instance;
    }

    bool Start(NetDevice* dev, u16 port, bool echo);
    void Stop();

    bool IsRunning();

    void ResetCounters();
    void Dump(Stdlib::Printer& printer);

    static const u16 DefaultPort = 9999;

private:
    NetLoad() {}
    ~NetLoad() {}
    NetLoad(const NetLoad& other) = delete;
    NetLoad& operator=(const NetLoad& other) = delete;
};

}
