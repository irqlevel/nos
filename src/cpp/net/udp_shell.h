#pragma once

#include <include/types.h>

namespace Kernel
{

class NetDevice;

/* The shell over UDP is Rust (src/rust/net/src/udp_shell.rs): the protocol
   header, the command dispatch, the chunked and paced reply. What is left
   here is the way in.

   On a machine with no serial port this is the only console there is, so
   scripts/udpsh.py and every test that drives the shell speak this. */
class UdpShell
{
public:
    UdpShell() {}
    ~UdpShell() { Stop(); }

    /* Start on port. False when one is running already, or the device has no
       free listener slot. */
    bool Start(NetDevice* dev, u16 port);

    /* Stop it and give up the port; returns once its task has left. */
    void Stop();

private:
    UdpShell(const UdpShell& other) = delete;
    UdpShell(UdpShell&& other) = delete;
    UdpShell& operator=(const UdpShell& other) = delete;
    UdpShell& operator=(UdpShell&& other) = delete;
};

}
