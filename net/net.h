#pragma once

#include <include/types.h>
#include <lib/byteorder.h>
#include <lib/stdlib.h>
#include <lib/printer.h>

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

static const u8 IpProtoIcmp = 1;
static const u8 IpProtoTcp  = 6;
static const u8 IpProtoUdp  = 17;

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

/* Byte-order helpers (aliases from Stdlib) */
using Stdlib::Htons;
using Stdlib::Htonl;
using Stdlib::Ntohs;
using Stdlib::Ntohl;

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

/* --- Address types --- */

struct MacAddress
{
    u8 Bytes[6];

    MacAddress()
    {
        Stdlib::MemSet(Bytes, 0, 6);
    }

    MacAddress(const u8 src[6])
    {
        Stdlib::MemCpy(Bytes, src, 6);
    }

    static MacAddress Broadcast()
    {
        MacAddress m;
        Stdlib::MemSet(m.Bytes, 0xFF, 6);
        return m;
    }

    bool operator==(const MacAddress& o) const { return Stdlib::MemCmp(Bytes, o.Bytes, 6) == 0; }
    bool operator!=(const MacAddress& o) const { return !(*this == o); }

    bool IsZero() const
    {
        for (ulong i = 0; i < 6; i++)
            if (Bytes[i] != 0) return false;
        return true;
    }

    bool IsBroadcast() const
    {
        for (ulong i = 0; i < 6; i++)
            if (Bytes[i] != 0xFF) return false;
        return true;
    }

    void CopyTo(u8 dst[6]) const { Stdlib::MemCpy(dst, Bytes, 6); }

    void Print(Stdlib::Printer& p) const
    {
        p.Printf("%p:%p:%p:%p:%p:%p",
            (ulong)Bytes[0], (ulong)Bytes[1], (ulong)Bytes[2],
            (ulong)Bytes[3], (ulong)Bytes[4], (ulong)Bytes[5]);
    }
};

struct IpAddress
{
    static const u8 V4 = 4;
    static const u8 V6 = 6;

    u8 Version;
    union {
        u32 Addr4;     /* host byte order, valid when Version == V4 */
        u8 Addr6[16];  /* valid when Version == V6 */
    };

    IpAddress() : Version(V4), Addr4(0) {}
    IpAddress(u32 hostOrder) : Version(V4), Addr4(hostOrder) {}
    IpAddress(u8 a, u8 b, u8 c, u8 d)
        : Version(V4)
        , Addr4(((u32)a << 24) | ((u32)b << 16) | ((u32)c << 8) | (u32)d)
    {}

    bool IsV4() const { return Version == V4; }
    bool IsV6() const { return Version == V6; }

    bool operator==(const IpAddress& o) const
    {
        if (Version != o.Version) return false;
        if (Version == V4) return Addr4 == o.Addr4;
        return Stdlib::MemCmp(Addr6, o.Addr6, 16) == 0;
    }
    bool operator!=(const IpAddress& o) const { return !(*this == o); }

    bool IsZero() const
    {
        if (Version == V4) return Addr4 == 0;
        for (ulong i = 0; i < 16; i++)
            if (Addr6[i] != 0) return false;
        return true;
    }

    /* IPv4 wire conversion */
    u32 ToNetwork() const { return Stdlib::Htonl(Addr4); }
    static IpAddress FromNetwork(u32 net) { return IpAddress(Stdlib::Ntohl(net)); }

    void Print(Stdlib::Printer& p) const
    {
        if (Version == V4)
        {
            p.Printf("%u.%u.%u.%u",
                (ulong)((Addr4 >> 24) & 0xFF),
                (ulong)((Addr4 >> 16) & 0xFF),
                (ulong)((Addr4 >> 8) & 0xFF),
                (ulong)(Addr4 & 0xFF));
        }
    }

    static bool Parse(const char* s, IpAddress& out)
    {
        u32 ip = 0;
        ulong octet = 0;
        ulong shift = 24;
        ulong dots = 0;
        const char* p = s;

        while (*p)
        {
            if (*p == '.')
            {
                if (octet > 255) return false;
                ip |= (u32)(octet << shift);
                if (shift == 0) return false;
                shift -= 8;
                octet = 0;
                dots++;
            }
            else if (*p >= '0' && *p <= '9')
            {
                octet = octet * 10 + (ulong)(*p - '0');
            }
            else
            {
                return false;
            }
            p++;
        }

        if (dots != 3 || octet > 255)
            return false;

        ip |= (u32)(octet << shift);
        out = IpAddress(ip);
        return true;
    }
};

} /* namespace Net */
} /* namespace Kernel */
