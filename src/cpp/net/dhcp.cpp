#include "dhcp.h"

#include <net/net_device.h>

extern "C" {

/* What the lease turned out to be; crate::dhcp::Lease is the same struct. */
struct RustDhcpLease
{
    unsigned int Ip;
    unsigned int Mask;
    unsigned int Router;
    unsigned int Dns;
    unsigned int ServerIp;
    unsigned int LeaseSecs;
};

int rust_dhcp_start(unsigned long dev);
void rust_dhcp_stop();
int rust_dhcp_ready();
void rust_dhcp_lease(RustDhcpLease* out);

}

namespace Kernel
{

bool DhcpClient::Start(NetDevice* dev)
{
    if (dev == nullptr)
        return false;

    return rust_dhcp_start(reinterpret_cast<unsigned long>(dev)) == 0;
}

void DhcpClient::Stop()
{
    rust_dhcp_stop();
}

bool DhcpClient::IsReady()
{
    return rust_dhcp_ready() != 0;
}

DhcpResult DhcpClient::GetResult()
{
    RustDhcpLease lease = {};
    rust_dhcp_lease(&lease);

    DhcpResult result;
    result.Ip.Addr4 = lease.Ip;
    result.Mask.Addr4 = lease.Mask;
    result.Router.Addr4 = lease.Router;
    result.Dns.Addr4 = lease.Dns;
    result.ServerIp.Addr4 = lease.ServerIp;
    result.LeaseTime = lease.LeaseSecs;
    return result;
}

}
