#pragma once

#include <include/types.h>

namespace Kernel
{
namespace Net
{

/* Ethernet header */
struct EthHdr
{
    u8 DstMac[6];
    u8 SrcMac[6];
    u16 EtherType;
} __attribute__((packed));

static_assert(sizeof(EthHdr) == 14, "Invalid size");

static const u16 EtherTypeIp  = 0x0800;
static const u16 EtherTypeArp = 0x0806;

/* ARP packet */
struct ArpPacket
{
    u16 HwType;
    u16 ProtoType;
    u8 HwSize;
    u8 ProtoSize;
    u16 Opcode;
    u8 SenderMac[6];
    u32 SenderIp;
    u8 TargetMac[6];
    u32 TargetIp;
} __attribute__((packed));

static_assert(sizeof(ArpPacket) == 28, "Invalid size");

/* IP header */
struct IpHdr
{
    u8 VersionIhl;
    u8 Tos;
    u16 TotalLen;
    u16 Id;
    u16 FragOff;
    u8 Ttl;
    u8 Protocol;
    u16 Checksum;
    u32 SrcAddr;
    u32 DstAddr;
} __attribute__((packed));

static_assert(sizeof(IpHdr) == 20, "Invalid size");

/* UDP header */
struct UdpHdr
{
    u16 SrcPort;
    u16 DstPort;
    u16 Length;
    u16 Checksum;
} __attribute__((packed));

static_assert(sizeof(UdpHdr) == 8, "Invalid size");

/* Byte-order helpers */
inline u16 Htons(u16 v) { return (u16)((v >> 8) | (v << 8)); }
inline u32 Htonl(u32 v)
{
    return ((v >> 24) & 0xFF) |
           ((v >> 8) & 0xFF00) |
           ((v << 8) & 0xFF0000) |
           ((v << 24) & 0xFF000000);
}
inline u16 Ntohs(u16 v) { return Htons(v); }
inline u32 Ntohl(u32 v) { return Htonl(v); }

/* IP checksum */
inline u16 IpChecksum(const void* data, ulong len)
{
    const u8* p = (const u8*)data;
    u32 sum = 0;

    for (ulong i = 0; i < len; i += 2)
    {
        u16 word;
        if (i + 1 < len)
            word = (u16)((p[i] << 8) | p[i + 1]);
        else
            word = (u16)(p[i] << 8);
        sum += word;
    }

    while (sum >> 16)
        sum = (sum & 0xFFFF) + (sum >> 16);

    return (u16)(~sum);
}

} /* namespace Net */
} /* namespace Kernel */
