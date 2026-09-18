#pragma once

#include <include/types.h>
#include <lib/printer.h>

namespace Kernel
{

class NetDevice;

/* The kernel log over UDP ("netconsole=ip:port") is Rust
   (src/rust/net/src/netconsole.rs): the capture ring, the paced drain, the
   sequence numbers a collector tells a gap by, and the best-effort flush
   from panic context. What is left here is the way in.

   On a machine with no serial port this is the only console there is -- see
   docs/real-hardware.md and scripts/netconsole.py. */
class Netconsole final
{
public:
    static Netconsole& GetInstance()
    {
        static Netconsole Instance;
        return Instance;
    }

    /* Arm capture from the netconsole= kernel parameter. Safe to call long
       before the network exists; primes the ring with what dmesg already
       has, so lines traced before this point are not lost. */
    bool Setup();

    /* Attach a device and start the drain task. */
    bool Start(NetDevice* dev);
    void Stop();

    /* Capture hook: called from Tracer::Output for every message, and from
       the panic printer. Must be safe at any IRQ level. */
    void Log(const char* s);

    /* Called once a panic has started, before anything is printed:
       remembers how much undrained backlog sits in front of the report. */
    void PanicMark();

    /* Best-effort synchronous drain from panic context. */
    void PanicFlush();

    bool IsEnabled();

    void Dump(Stdlib::Printer& printer);

private:
    Netconsole() {}
    ~Netconsole() {}
    Netconsole(const Netconsole& other) = delete;
    Netconsole(Netconsole&& other) = delete;
    Netconsole& operator=(const Netconsole& other) = delete;
    Netconsole& operator=(Netconsole&& other) = delete;
};

}
