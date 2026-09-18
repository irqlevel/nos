#pragma once

#include <include/types.h>
#include <net/net.h>

namespace Kernel
{

struct NetDevice;

/* The client itself is Rust (src/rust/net/src/dhcp.rs): the discover, the
   request, the parsing, and the task that renews the lease at half its life.
   What is left here is the way in. */

struct DhcpResult
{
    Net::IpAddress Ip;
    Net::IpAddress Mask;
    Net::IpAddress Router;
    Net::IpAddress Dns;
    Net::IpAddress ServerIp;
    u32 LeaseTime; /* seconds */
};

class DhcpClient
{
public:
    DhcpClient() {}
    ~DhcpClient() {}

    /* Start on dev. False when a client is running already. */
    bool Start(NetDevice* dev);

    /* Stop it and give up the port; returns once its task has left. */
    void Stop();

    bool IsReady();
    DhcpResult GetResult();

private:
    DhcpClient(const DhcpClient& other) = delete;
    DhcpClient(DhcpClient&& other) = delete;
    DhcpClient& operator=(const DhcpClient& other) = delete;
    DhcpClient& operator=(DhcpClient&& other) = delete;
};

}
