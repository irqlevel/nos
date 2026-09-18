#include "udp_shell.h"

#include <net/net_device.h>

extern "C" {

int rust_udp_shell_start(unsigned long dev, unsigned short port);
void rust_udp_shell_stop();

}

namespace Kernel
{

bool UdpShell::Start(NetDevice* dev, u16 port)
{
    if (dev == nullptr)
        return false;

    return rust_udp_shell_start(reinterpret_cast<unsigned long>(dev), port) == 0;
}

void UdpShell::Stop()
{
    rust_udp_shell_stop();
}

}
