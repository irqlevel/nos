#pragma once

#include <include/types.h>
#include <net/net_device.h>
#include <net/net.h>
#include <kernel/task.h>
#include <kernel/spin_lock.h>

namespace Kernel
{

struct DhcpPacket
{
    u8 Op;
    u8 HType;
    u8 HLen;
    u8 Hops;
    u32 Xid;
    u16 Secs;
    u16 Flags;
    u32 CIAddr;
    u32 YIAddr;
    u32 SIAddr;
    u32 GIAddr;
    u8 CHAddr[16];
    u8 SName[64];
    u8 File[128];
} __attribute__((packed));

static_assert(sizeof(DhcpPacket) == 236, "Invalid size");

/* DHCP magic cookie */
static const u32 DhcpMagicCookie = 0x63825363;

/* DHCP option codes */
static const u8 DhcpOptSubnetMask   = 1;
static const u8 DhcpOptRouter       = 3;
static const u8 DhcpOptDns          = 6;
static const u8 DhcpOptRequestedIp  = 50;
static const u8 DhcpOptLeaseTime    = 51;
static const u8 DhcpOptMessageType  = 53;
static const u8 DhcpOptServerId     = 54;
static const u8 DhcpOptParamRequest = 55;
static const u8 DhcpOptEnd          = 255;

/* DHCP message types */
static const u8 DhcpDiscover = 1;
static const u8 DhcpOffer    = 2;
static const u8 DhcpRequest  = 3;
static const u8 DhcpAck      = 5;
static const u8 DhcpNak      = 6;

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
    DhcpClient();
    ~DhcpClient();

    bool Start(NetDevice* dev);
    void Stop();

    bool IsReady();
    DhcpResult GetResult();

private:
    DhcpClient(const DhcpClient& other) = delete;
    DhcpClient(DhcpClient&& other) = delete;
    DhcpClient& operator=(const DhcpClient& other) = delete;
    DhcpClient& operator=(DhcpClient&& other) = delete;

    static void TaskFunc(void* ctx);
    void Run();

    bool DoDiscover();
    bool DoRequest();
    bool WaitForResponse(u8 expectedType, ulong timeoutMs);

    ulong BuildDiscover(u8* frame, ulong maxLen);
    ulong BuildRequest(u8* frame, ulong maxLen);
    bool ParseResponse(const u8* frame, ulong len, u8 expectedType);

    static void RxCallbackFn(const u8* frame, ulong len, void* ctx);

    NetDevice* Dev;
    Task* TaskPtr;
    DhcpResult Result;
    bool Ready;

    u32 Xid;
    Net::IpAddress OfferedIp;
    Net::IpAddress ServerId;

    /* RX buffer for DHCP responses */
    static const ulong RxBufMaxLen = 1500;
    u8 RxBuf[RxBufMaxLen];
    ulong RxBufLen;
    bool RxBufReady;
    SpinLock RxLock;

    static const ulong Tag = 'Dhcp';
};

}
