#include "cmd.h"
#include "trace.h"
#include "asm.h"
#include "dmesg.h"
#include "cpu.h"
#include "interrupt.h"
#include "time.h"
#include "watchdog.h"
#include <block/block_device.h>
#include <block/partition.h>
#include "parameters.h"
#include <net/net_device.h>
#include <net/net.h>
#include <net/arp.h>
#include <net/dhcp.h>
#include <net/icmp.h>
#include <net/dns.h>
#include <net/tcp.h>
#include <net/http.h>
#include <fs/vfs.h>
#include <fs/ramfs.h>
#include <fs/nanofs.h>
#include "entropy.h"
#include "console.h"
#include "mutex.h"
#include "task.h"
#include "stack_trace.h"
#include "symtab.h"

#include <drivers/vga.h>
#include <drivers/pci.h>
#include <mm/page_table.h>
#include <mm/new.h>

namespace Kernel
{

static DhcpClient& GetDhcpClient()
{
    static DhcpClient instance;
    return instance;
}

struct CmdEntry
{
    const char* Name;
    void (*Handler)(const char* args, Stdlib::Printer& con);
    const char* Help;
};

static void CmdCls(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Console::GetInstance().Cls();
    (void)con;
}

static void CmdPoweroff(const char* args, Stdlib::Printer& con)
{
    (void)args;
    con.Printf("shutting down...\n");
    Cmd::GetInstance().RequestShutdown();
}

static void CmdReboot(const char* args, Stdlib::Printer& con)
{
    (void)args;
    con.Printf("rebooting...\n");
    Cmd::GetInstance().RequestReboot();
}

static void CmdCpu(const char* args, Stdlib::Printer& con)
{
    (void)args;
    con.Printf("ss 0x%p cs 0x%p ds 0x%p gs 0x%p fs 0x%p es 0x%p",
        (ulong)GetSs(), (ulong)GetCs(), (ulong)GetDs(),
        (ulong)GetGs(), (ulong)GetFs(), (ulong)GetEs());

    con.Printf("rflags 0x%p rsp 0x%p rip 0x%p\n",
        GetRflags(), GetRsp(), GetRip());

    con.Printf("cr0 0x%p cr2 0x%p cr3 0x%p cr4 0x%p",
        GetCr0(), GetCr2(), GetCr3(), GetCr4());
}

static void CmdDmesg(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* filterStart = Stdlib::NextToken(args, end);

    if (!filterStart)
    {
        Dmesg::GetInstance().Dump(con);
        return;
    }

    char filter[64];
    Stdlib::TokenCopy(filterStart, end, filter, sizeof(filter));

    for (DmesgMsg* msg = Dmesg::GetInstance().Next(nullptr);
         msg != nullptr;
         msg = Dmesg::GetInstance().Next(msg))
    {
        if (Stdlib::StrStr(msg->Str, filter))
            con.PrintString(msg->Str);
    }
}

static void CmdUptime(const char* args, Stdlib::Printer& con)
{
    (void)args;
    auto time = GetBootTime();
    con.Printf("%u.%u\n", time.GetSecs(), time.GetUsecs());
}

static void CmdDate(const char* args, Stdlib::Printer& con)
{
    (void)args;

    static const ulong SecsPerMin  = 60;
    static const ulong MinsPerHour = 60;
    static const ulong HoursPerDay = 24;
    static const ulong DaysPerYear = 365;
    static const ulong DaysPerLeapYear = 366;
    static const ulong MonthsPerYear = 12;
    static const ulong FebruaryIndex = 2;
    static const ulong UnixEpochYear = 1970;

    ulong epoch = GetWallTimeSecs();
    if (epoch == 0)
    {
        con.Printf("wall clock not available\n");
        return;
    }

    /* Decompose Unix epoch into Y/M/D H:M:S */
    ulong secs = epoch;
    ulong s = secs % SecsPerMin; secs /= SecsPerMin;
    ulong m = secs % MinsPerHour; secs /= MinsPerHour;
    ulong h = secs % HoursPerDay; secs /= HoursPerDay;

    ulong days = secs; /* days since 1970-01-01 */
    ulong y = UnixEpochYear;
    while (true)
    {
        bool leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        ulong daysInYear = leap ? DaysPerLeapYear : DaysPerYear;
        if (days < daysInYear)
            break;
        days -= daysInYear;
        y++;
    }

    static const u16 daysInMonth[13] = {
        0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31
    };
    bool leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
    ulong mo = 1;
    while (mo <= MonthsPerYear)
    {
        ulong dim = daysInMonth[mo];
        if (mo == FebruaryIndex && leap)
            dim++;
        if (days < dim)
            break;
        days -= dim;
        mo++;
    }
    ulong d = days + 1;

    con.Printf("%u-%u-%u %u:%u:%u UTC\n", y, mo, d, h, m, s);
}

static void CmdPs(const char* args, Stdlib::Printer& con)
{
    (void)args;
    TaskTable::GetInstance().Ps(con);
}

static void CmdWatchdog(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Watchdog::GetInstance().Dump(con);
}

static void CmdMemusage(const char* args, Stdlib::Printer& con)
{
    (void)args;
    auto& pt = Mm::PageTable::GetInstance();

    con.Printf("freePages: %u\n", pt.GetFreePagesCount());
    con.Printf("totalPages: %u\n", pt.GetTotalPagesCount());
}

static void CmdIrqstat(const char* args, Stdlib::Printer& con)
{
    (void)args;
    for (ulong i = 0; i < InterruptStats::Count; i++)
    {
        InterruptSource src = (InterruptSource)i;
        long count = InterruptStats::Get(src);
        if (count > 0)
            con.Printf("%s: %u\n", InterruptStats::GetName(src), count);
    }
}

static void CmdPci(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Pci::GetInstance().Dump(con);
}

static void CmdDisks(const char* args, Stdlib::Printer& con)
{
    (void)args;
    BlockDeviceTable::GetInstance().Dump(con);
}

static void CmdPartitions(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* nameStart = Stdlib::NextToken(args, end);
    if (!nameStart)
    {
        con.Printf("usage: partitions <disk>\n");
        return;
    }

    char diskName[16];
    Stdlib::TokenCopy(nameStart, end, diskName, sizeof(diskName));

    BlockDevice* dev = BlockDeviceTable::GetInstance().Find(diskName);
    if (!dev)
    {
        con.Printf("disk '%s' not found\n", diskName);
        return;
    }

    u8 buf[512];
    if (!dev->ReadSectors(0, buf, 1))
    {
        con.Printf("failed to read sector 0\n");
        return;
    }

    auto* mbr = reinterpret_cast<Mbr*>(buf);
    if (mbr->Signature != Mbr::ValidSignature)
    {
        con.Printf("no MBR partition table (signature 0x%p)\n", (ulong)mbr->Signature);
        return;
    }

    con.Printf("  #  Type  LBA Start   LBA Size    Size\n");
    for (ulong i = 0; i < Mbr::MaxParts; i++)
    {
        auto& entry = mbr->Parts[i];
        if (entry.Type == 0 && entry.LbaSize == 0)
            continue;

        u64 sizeBytes = (u64)entry.LbaSize * dev->GetSectorSize();
        u64 mb = sizeBytes / (1024 * 1024);

        con.Printf("  %u  0x%p  %u  %u  %u MB\n",
            i + 1, (ulong)entry.Type,
            (ulong)entry.LbaStart, (ulong)entry.LbaSize, mb);
    }
}

static void CmdDiskread(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* nameStart = Stdlib::NextToken(args, end);
    if (!nameStart)
    {
        con.Printf("usage: diskread <disk> <sector>\n");
        return;
    }

    char diskName[16];
    Stdlib::TokenCopy(nameStart, end, diskName, sizeof(diskName));

    const char* secStart = Stdlib::NextToken(end, end);
    ulong sector = 0;
    if (!secStart)
    {
        con.Printf("usage: diskread <disk> <sector>\n");
        return;
    }

    char secBuf[32];
    Stdlib::TokenCopy(secStart, end, secBuf, sizeof(secBuf));

    if (!Stdlib::ParseUlong(secBuf, sector))
    {
        con.Printf("invalid sector number\n");
        return;
    }

    BlockDevice* dev = BlockDeviceTable::GetInstance().Find(diskName);
    if (!dev)
    {
        con.Printf("disk '%s' not found\n", diskName);
        return;
    }

    u8* buf = (u8*)Mm::Alloc(512, 0);
    if (!buf)
    {
        con.Printf("alloc failed\n");
        return;
    }

    if (!dev->ReadSectors(sector, buf, 1))
    {
        con.Printf("read error\n");
        Mm::Free(buf);
        return;
    }

    for (ulong i = 0; i < 512; i += 16)
    {
        con.Printf("%p: ", sector * 512 + i);
        for (ulong j = 0; j < 16 && (i + j) < 512; j++)
        {
            con.Printf("%p ", (ulong)buf[i + j]);
        }
        con.Printf("\n");
    }

    Mm::Free(buf);
}

static void CmdDiskwrite(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* nameStart = Stdlib::NextToken(args, end);
    if (!nameStart)
    {
        con.Printf("usage: diskwrite <disk> <sector> <hex>\n");
        return;
    }

    char diskName[16];
    Stdlib::TokenCopy(nameStart, end, diskName, sizeof(diskName));

    const char* secStart = Stdlib::NextToken(end, end);
    ulong sector = 0;
    if (!secStart)
    {
        con.Printf("usage: diskwrite <disk> <sector> <hex>\n");
        return;
    }

    char secBuf[32];
    Stdlib::TokenCopy(secStart, end, secBuf, sizeof(secBuf));

    const char* hexStart = Stdlib::NextToken(end, end);
    if (!Stdlib::ParseUlong(secBuf, sector) || !hexStart)
    {
        con.Printf("usage: diskwrite <disk> <sector> <hex>\n");
        return;
    }

    BlockDevice* dev = BlockDeviceTable::GetInstance().Find(diskName);
    if (!dev)
    {
        con.Printf("disk '%s' not found\n", diskName);
        return;
    }

    u8* buf = (u8*)Mm::Alloc(512, 0);
    if (!buf)
    {
        con.Printf("alloc failed\n");
        return;
    }

    Stdlib::MemSet(buf, 0, 512);
    ulong hexLen = (ulong)(end - hexStart);
    ulong byteCount = 0;
    if (!Stdlib::HexDecode(hexStart, hexLen, buf, 512, byteCount))
    {
        con.Printf("invalid hex data\n");
        Mm::Free(buf);
        return;
    }

    if (byteCount > 0)
    {
        if (!dev->WriteSectors(sector, buf, 1))
            con.Printf("write error\n");
        else
            con.Printf("wrote %u bytes to sector %u\n", byteCount, sector);
    }

    Mm::Free(buf);
}

static void CmdNet(const char* args, Stdlib::Printer& con)
{
    (void)args;
    NetDeviceTable::GetInstance().Dump(con);
}

static void CmdArp(const char* args, Stdlib::Printer& con)
{
    (void)args;
    ArpTable::GetInstance().Dump(con);
}

static void CmdIcmpstat(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Icmp::GetInstance().Dump(con);
}

static void CmdTcpstat(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Tcp::GetInstance().Dump(con);
}

static void CmdWget(const char* args, Stdlib::Printer& con)
{
    if (!args || args[0] == '\0')
    {
        con.Printf("usage: wget <url>\n");
        return;
    }

    NetDevice* dev = NetDeviceTable::GetInstance().Find("eth0");
    if (!dev)
    {
        con.Printf("eth0 not found\n");
        return;
    }

    HttpClient client(dev);
    HttpResponse resp = client.Get(args);

    if (!resp.Ok)
    {
        con.Printf("wget: failed\n");
        return;
    }

    con.Printf("HTTP %u, %u bytes\n", (ulong)resp.StatusCode, resp.BodyLen);

    if (resp.Location[0] != '\0')
        con.Printf("Location: %s\n", resp.Location);

    if (resp.Body && resp.BodyLen > 0)
    {
        /* Print body as text, truncate to 4 KB for display */
        static const ulong MaxDisplay = 4096;
        ulong displayLen = resp.BodyLen;
        if (displayLen > MaxDisplay)
            displayLen = MaxDisplay;
        for (ulong i = 0; i < displayLen; i++)
            con.Printf("%c", (ulong)resp.Body[i]);
        con.Printf("\n");
        if (resp.BodyLen > MaxDisplay)
            con.Printf("... (%u bytes truncated)\n", resp.BodyLen - MaxDisplay);
    }

    if (resp.Body)
        Mm::Free(resp.Body);
}

static void CmdUdpsend(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* ipStart = Stdlib::NextToken(args, end);
    if (!ipStart)
    {
        con.Printf("usage: udpsend <ip> <port> <message>\n");
        return;
    }

    char ipBuf[16];
    Stdlib::TokenCopy(ipStart, end, ipBuf, sizeof(ipBuf));

    const char* portStart = Stdlib::NextToken(end, end);
    if (!portStart)
    {
        con.Printf("usage: udpsend <ip> <port> <message>\n");
        return;
    }

    char portBuf[8];
    Stdlib::TokenCopy(portStart, end, portBuf, sizeof(portBuf));

    ulong port = 0;
    if (!Stdlib::ParseUlong(portBuf, port) || port > 65535)
    {
        con.Printf("invalid port\n");
        return;
    }

    /* Skip whitespace to get message */
    const char* msg = end;
    while (*msg == ' ')
        msg++;

    if (*msg == '\0')
    {
        con.Printf("usage: udpsend <ip> <port> <message>\n");
        return;
    }

    Net::IpAddress dstIp;
    if (!Net::IpAddress::Parse(ipBuf, dstIp))
    {
        con.Printf("invalid IP '%s'\n", ipBuf);
        return;
    }

    /* Find first net device */
    NetDevice* dev = nullptr;
    if (NetDeviceTable::GetInstance().GetCount() > 0)
        dev = NetDeviceTable::GetInstance().Find("eth0");

    if (!dev)
    {
        con.Printf("no network device\n");
        return;
    }

    ulong msgLen = Stdlib::StrLen(msg);
    Net::IpAddress srcIp = dev->GetIp();

    /* Resolve destination MAC via ARP */
    Net::MacAddress dstMac;
    if (!ArpTable::GetInstance().Resolve(dev, dstIp, dstMac))
        dstMac = Net::MacAddress::Broadcast();

    /* Build UDP frame */
    ulong udpPayLen = sizeof(Net::UdpHdr) + msgLen;
    ulong ipPayLen = sizeof(Net::IpHdr) + udpPayLen;
    ulong frameLen = sizeof(Net::EthHdr) + ipPayLen;

    if (frameLen > 1514)
    {
        con.Printf("message too large\n");
        return;
    }

    u8 frame[1514];
    Stdlib::MemSet(frame, 0, sizeof(frame));
    ulong off = 0;

    Net::EthHdr* eth = (Net::EthHdr*)(frame + off);
    dstMac.CopyTo(eth->DstMac);
    dev->GetMac().CopyTo(eth->SrcMac);
    eth->EtherType = Net::Htons(Net::EtherTypeIp);
    off += sizeof(Net::EthHdr);

    Net::IpHdr* ip = (Net::IpHdr*)(frame + off);
    ip->VersionIhl = 0x45;
    ip->TotalLen = Net::Htons((u16)ipPayLen);
    ip->Ttl = 64;
    ip->Protocol = Net::IpProtoUdp;
    ip->SrcAddr = srcIp.ToNetwork();
    ip->DstAddr = dstIp.ToNetwork();
    ip->Checksum = Net::Htons(Net::IpChecksum(ip, sizeof(Net::IpHdr)));
    off += sizeof(Net::IpHdr);

    Net::UdpHdr* udp = (Net::UdpHdr*)(frame + off);
    udp->SrcPort = Net::Htons(12345);
    udp->DstPort = Net::Htons((u16)port);
    udp->Length = Net::Htons((u16)udpPayLen);
    off += sizeof(Net::UdpHdr);

    Stdlib::MemCpy(frame + off, msg, msgLen);
    off += msgLen;

    if (dev->SendRaw(frame, off))
    {
        con.Printf("sent %u bytes to %s:%u\n",
            msgLen, ipBuf, port);
    }
    else
    {
        con.Printf("send failed\n");
    }
}

static void CmdPing(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* ipStart = Stdlib::NextToken(args, end);
    if (!ipStart)
    {
        con.Printf("usage: ping <ip|hostname>\n");
        return;
    }

    char hostBuf[DnsResolver::MaxDomainLen + 1];
    Stdlib::TokenCopy(ipStart, end, hostBuf, sizeof(hostBuf));

    Net::IpAddress dstIp;
    if (!Net::IpAddress::Parse(hostBuf, dstIp))
    {
        if (!DnsResolver::GetInstance().IsInitialized() ||
            !DnsResolver::GetInstance().Resolve(hostBuf, dstIp))
        {
            con.Printf("cannot resolve '%s'\n", hostBuf);
            return;
        }
    }

    NetDevice* dev = nullptr;
    if (NetDeviceTable::GetInstance().GetCount() > 0)
        dev = NetDeviceTable::GetInstance().Find("eth0");

    if (!dev)
    {
        con.Printf("no network device\n");
        return;
    }

    u16 pingId = (u16)(ReadTsc() & 0xFFFF);

    con.Printf("PING %s\n", hostBuf);
    ulong received = 0;

    for (u16 seq = 0; seq < 5; seq++)
    {
        if (!Icmp::GetInstance().SendEchoRequest(dev, dstIp, pingId, seq))
        {
            con.Printf("send failed seq=%u\n", (ulong)seq);
        }
        else
        {
            ulong rttNs = 0;
            if (Icmp::GetInstance().WaitReply(pingId, seq, 3000, rttNs))
            {
                ulong rttMs = rttNs / Const::NanoSecsInMs;
                con.Printf("reply from %s: seq=%u time=%u ms\n",
                    hostBuf, (ulong)seq, rttMs);
                received++;
            }
            else
            {
                con.Printf("request timeout seq=%u\n", (ulong)seq);
            }
        }

        if (seq < 4)
            Sleep(1000 * Const::NanoSecsInMs);
    }

    con.Printf("%u/5 received\n", received);
}

static void CmdNslookup(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* nameStart = Stdlib::NextToken(args, end);
    if (!nameStart)
    {
        con.Printf("usage: nslookup <hostname>\n");
        return;
    }

    char hostBuf[DnsResolver::MaxDomainLen + 1];
    Stdlib::TokenCopy(nameStart, end, hostBuf, sizeof(hostBuf));

    if (!DnsResolver::GetInstance().IsInitialized())
    {
        con.Printf("DNS resolver not initialized\n");
        return;
    }

    Net::IpAddress ip;
    if (DnsResolver::GetInstance().Resolve(hostBuf, ip))
    {
        con.Printf("%s -> %u.%u.%u.%u\n", hostBuf,
            (ulong)((ip.Addr4 >> 24) & 0xFF),
            (ulong)((ip.Addr4 >> 16) & 0xFF),
            (ulong)((ip.Addr4 >> 8) & 0xFF),
            (ulong)(ip.Addr4 & 0xFF));
    }
    else
    {
        con.Printf("failed to resolve '%s'\n", hostBuf);
    }
}

static void CmdDnsflush(const char* args, Stdlib::Printer& con)
{
    (void)args;
    DnsResolver::GetInstance().Flush();
    con.Printf("dns cache flushed\n");
}

static void CmdDhcp(const char* args, Stdlib::Printer& con)
{
    if (Parameters::GetInstance().IsDhcpOff())
    {
        con.Printf("DHCP disabled (dhcp=off)\n");
        return;
    }

    static Mutex dhcpLock;
    Stdlib::AutoLock lock(dhcpLock);

    const char* devName = "eth0";
    if (args[0] != '\0')
        devName = args;

    NetDevice* dev = NetDeviceTable::GetInstance().Find(devName);
    if (!dev)
    {
        con.Printf("device '%s' not found\n", devName);
        return;
    }

    if (GetDhcpClient().IsReady())
    {
        DhcpResult r = GetDhcpClient().GetResult();
        con.Printf("already bound: ");
        r.Ip.Print(con);
        con.Printf("\n");
        return;
    }

    con.Printf("DHCP discovering on %s...\n", devName);
    if (!GetDhcpClient().Start(dev))
    {
        con.Printf("failed to start DHCP\n");
        return;
    }

    /* Wait up to 10 seconds for a lease */
    for (ulong i = 0; i < 100 && !GetDhcpClient().IsReady(); i++)
        Sleep(100 * Const::NanoSecsInMs);

    if (GetDhcpClient().IsReady())
    {
        DhcpResult r = GetDhcpClient().GetResult();
        con.Printf("ip:     "); r.Ip.Print(con); con.Printf("\n");
        con.Printf("mask:   "); r.Mask.Print(con); con.Printf("\n");
        con.Printf("router: "); r.Router.Print(con); con.Printf("\n");
        con.Printf("dns:    "); r.Dns.Print(con); con.Printf("\n");
        con.Printf("lease:  %u seconds\n", r.LeaseTime);

        if (Parameters::GetInstance().IsDnsEnabled() && !r.Dns.IsZero() &&
            !DnsResolver::GetInstance().IsInitialized())
        {
            if (DnsResolver::GetInstance().Init(dev, r.Dns))
            {
                con.Printf("DNS resolver started, server: ");
                r.Dns.Print(con);
                con.Printf("\n");
            }
        }
    }
    else
    {
        con.Printf("DHCP timeout\n");
    }
}

static void CmdMount(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* fsType = Stdlib::NextToken(args, end);
    if (fsType == nullptr)
    {
        con.Printf("usage: mount ramfs <path>\n");
        con.Printf("       mount nanofs <disk> <path>\n");
        return;
    }
    char fsName[16];
    Stdlib::TokenCopy(fsType, end, fsName, sizeof(fsName));

    if (Stdlib::StrCmp(fsName, "ramfs") == 0)
    {
        const char* pathStart = Stdlib::NextToken(end, end);
        if (pathStart == nullptr)
        {
            con.Printf("usage: mount ramfs <path>\n");
            return;
        }
        char path[Vfs::MaxPath];
        Stdlib::TokenCopy(pathStart, end, path, sizeof(path));

        RamFs* fs = new RamFs();
        if (fs == nullptr)
        {
            con.Printf("failed to allocate ramfs\n");
        }
        else if (!Vfs::GetInstance().Mount(path, fs))
        {
            delete fs;
            con.Printf("mount failed\n");
        }
        else
        {
            con.Printf("mounted ramfs on %s\n", path);
        }
    }
    else if (Stdlib::StrCmp(fsName, "nanofs") == 0)
    {
        const char* diskStart = Stdlib::NextToken(end, end);
        if (diskStart == nullptr)
        {
            con.Printf("usage: mount nanofs <disk> <path>\n");
            return;
        }
        char diskName[16];
        Stdlib::TokenCopy(diskStart, end, diskName, sizeof(diskName));

        const char* pathStart = Stdlib::NextToken(end, end);
        if (pathStart == nullptr)
        {
            con.Printf("usage: mount nanofs <disk> <path>\n");
            return;
        }
        char path[Vfs::MaxPath];
        Stdlib::TokenCopy(pathStart, end, path, sizeof(path));

        BlockDevice* dev = BlockDeviceTable::GetInstance().Find(diskName);
        if (dev == nullptr)
        {
            con.Printf("disk '%s' not found\n", diskName);
            return;
        }

        NanoFs* fs = new NanoFs(dev);
        if (fs == nullptr)
        {
            con.Printf("failed to allocate nanofs\n");
            return;
        }

        if (!Vfs::GetInstance().Mount(path, fs))
        {
            delete fs;
            con.Printf("mount failed\n");
        }
        else
        {
            con.Printf("mounted nanofs on %s\n", path);
        }
    }
    else
    {
        con.Printf("unknown filesystem '%s'\n", fsName);
    }
}

static void CmdUmount(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: umount <path>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));
    FileSystem* fs = Vfs::GetInstance().Unmount(path);
    if (fs == nullptr)
    {
        con.Printf("not mounted\n");
    }
    else
    {
        delete fs;
        con.Printf("unmounted %s\n", path);
    }
}

static void CmdMounts(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Vfs::GetInstance().DumpMounts(con);
}

static void CmdLs(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: ls <path>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));
    Vfs::GetInstance().ListDir(path, con);
}

static void CmdCat(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: cat <path>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));
    Vfs::GetInstance().ReadFile(path, con);
}

static void CmdWrite(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: write <path> <text>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));

    // Rest of the line after path is the content
    const char* content = end;
    while (*content == ' ')
        content++;

    ulong len = Stdlib::StrLen(content);
    if (Vfs::GetInstance().WriteFile(path, content, len))
    {
        con.Printf("wrote %u bytes\n", len);
    }
    else
    {
        con.Printf("write failed\n");
    }
}

static void CmdMkdir(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: mkdir <path>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));
    if (Vfs::GetInstance().CreateDir(path))
    {
        con.Printf("created %s\n", path);
    }
    else
    {
        con.Printf("mkdir failed\n");
    }
}

static void CmdTouch(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: touch <path>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));
    if (Vfs::GetInstance().CreateFile(path))
    {
        con.Printf("created %s\n", path);
    }
    else
    {
        con.Printf("touch failed\n");
    }
}

static void CmdDel(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: del <path>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));
    if (Vfs::GetInstance().Remove(path))
    {
        con.Printf("removed %s\n", path);
    }
    else
    {
        con.Printf("del failed\n");
    }
}

static void CmdFormat(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* fsType = Stdlib::NextToken(args, end);
    if (fsType == nullptr)
    {
        con.Printf("usage: format nanofs <disk>\n");
        return;
    }
    char fsName[16];
    Stdlib::TokenCopy(fsType, end, fsName, sizeof(fsName));

    if (Stdlib::StrCmp(fsName, "nanofs") != 0)
    {
        con.Printf("unknown filesystem '%s'\n", fsName);
        return;
    }

    const char* diskStart = Stdlib::NextToken(end, end);
    if (diskStart == nullptr)
    {
        con.Printf("usage: format nanofs <disk>\n");
        return;
    }
    char diskName[16];
    Stdlib::TokenCopy(diskStart, end, diskName, sizeof(diskName));

    BlockDevice* dev = BlockDeviceTable::GetInstance().Find(diskName);
    if (dev == nullptr)
    {
        con.Printf("disk '%s' not found\n", diskName);
        return;
    }

    NanoFs fs(dev);
    if (fs.Format(dev))
    {
        con.Printf("formatted %s as nanofs\n", diskName);
    }
    else
    {
        con.Printf("format failed\n");
    }
}

static void CmdVersion(const char* args, Stdlib::Printer& con)
{
    (void)args;
    con.Printf("nos %s\n", KERNEL_VERSION);
}

static void CmdRandom(const char* args, Stdlib::Printer& con)
{
    ulong len = 16;
    if (args[0] != '\0')
    {
        if (!Stdlib::ParseUlong(args, len) || len == 0 || len > 1024)
        {
            con.Printf("usage: random [len] (1..1024, default 16)\n");
            return;
        }
    }

    EntropySource* src = EntropySourceTable::GetInstance().GetDefault();
    if (!src)
    {
        con.Printf("no entropy source\n");
        return;
    }

    u8 buf[1024];
    if (!src->GetRandom(buf, len))
    {
        con.Printf("failed to get random bytes\n");
        return;
    }

    static const char hex[] = "0123456789abcdef";
    for (ulong i = 0; i < len; i++)
    {
        char s[3];
        s[0] = hex[(buf[i] >> 4) & 0xF];
        s[1] = hex[buf[i] & 0xF];
        s[2] = '\0';
        con.PrintString(s);
    }
    con.Printf("\n");
}

static void DumpStackTrace(ulong* frames, size_t count, Stdlib::Printer& con)
{
    auto& symtab = SymbolTable::GetInstance();
    for (size_t i = 0; i < count; i++)
    {
        const char* name;
        ulong offset;
        if (symtab.Resolve(frames[i], name, offset))
            con.Printf("  [%u] 0x%p %s+0x%p\n", (ulong)i, frames[i], name, offset);
        else
            con.Printf("  [%u] 0x%p\n", (ulong)i, frames[i]);
    }
}

struct BtCtx
{
    ulong Frames[16];
    size_t Count;
};

static void CmdBtIPIFunc(void* ctx, Context* ipiCtx)
{
    auto* bt = static_cast<BtCtx*>(ctx);
    bt->Count = StackTrace::CaptureFrom(ipiCtx->Rbp, bt->Frames, Stdlib::ArraySize(bt->Frames));
}

static void CmdBt(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pidStart = Stdlib::NextToken(args, end);
    if (!pidStart)
    {
        con.Printf("usage: bt <pid>\n");
        return;
    }

    char pidBuf[16];
    Stdlib::TokenCopy(pidStart, end, pidBuf, sizeof(pidBuf));

    ulong pid = 0;
    if (!Stdlib::ParseUlong(pidBuf, pid))
    {
        con.Printf("invalid pid\n");
        return;
    }

    ObjectPtr<Task> task(TaskTable::GetInstance().Lookup(pid));
    if (!task)
    {
        con.Printf("task %u not found\n", pid);
        return;
    }

    ulong frames[16];
    size_t count = 0;

    Task* self = Task::GetCurrentTask();
    if (task.Get() == self)
    {
        /* Target is the current task on this CPU */
        count = StackTrace::Capture(frames, Stdlib::ArraySize(frames));
        con.Printf("task %u (%s) running on current cpu:\n", task->Pid, task->GetName());
        DumpStackTrace(frames, count, con);
        return;
    }

    long state = task->State.Get();
    if (state == Task::StateRunning)
    {
        /* Task is running on another CPU — find which one */
        ulong cpuMask = CpuTable::GetInstance().GetRunningCpus();
        ulong runCpu = ~0UL;
        for (ulong i = 0; i < MaxCpus; i++)
        {
            if (!(cpuMask & (1UL << i)))
                continue;
            auto& cpuTaskQueue = CpuTable::GetInstance().GetCpu(i).GetTaskQueue();
            if (task->TaskQueue == &cpuTaskQueue)
            {
                runCpu = i;
                break;
            }
        }

        if (runCpu == ~0UL)
        {
            con.Printf("task %u (%s) running but cpu not found\n",
                task->Pid, task->GetName());
            return;
        }

        BtCtx btCtx;
        btCtx.Count = 0;

        IPITask ipiTask(CmdBtIPIFunc, &btCtx);
        CpuTable::GetInstance().GetCpu(runCpu).QueueIPITask(ipiTask);

        con.Printf("task %u (%s) running on cpu %u:\n",
            task->Pid, task->GetName(), runCpu);
        DumpStackTrace(btCtx.Frames, btCtx.Count, con);
        return;
    }

    if (state == Task::StateExited)
    {
        con.Printf("task %u (%s) has exited\n", task->Pid, task->GetName());
        return;
    }

    /* Task is waiting/sleeping — walk from saved context */
    ulong savedRsp = task->Rsp;
    if (savedRsp == 0)
    {
        con.Printf("task %u (%s) has no saved context\n", task->Pid, task->GetName());
        return;
    }

    Context* ctx = reinterpret_cast<Context*>(savedRsp);
    count = StackTrace::CaptureFrom(ctx->Rbp, frames, Stdlib::ArraySize(frames));

    con.Printf("task %u (%s) state %u:\n", task->Pid, task->GetName(), (ulong)state);
    DumpStackTrace(frames, count, con);
}

static void CmdPanic(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* typeStart = Stdlib::NextToken(args, end);

    if (!typeStart)
    {
        Panic("user requested panic");
    }

    char type[16];
    Stdlib::TokenCopy(typeStart, end, type, sizeof(type));

    if (Stdlib::StrCmp(type, "pf") == 0)
    {
        con.Printf("triggering page fault...\n");
        volatile int* p = nullptr;
        *p = 0;
    }
    else if (Stdlib::StrCmp(type, "div0") == 0)
    {
        con.Printf("triggering divide by zero...\n");
        volatile int zero = 0;
        volatile int x = 1 / zero;
        (void)x;
    }
    else if (Stdlib::StrCmp(type, "ud") == 0)
    {
        con.Printf("triggering invalid opcode...\n");
        asm volatile("ud2");
    }
    else
    {
        con.Printf("usage: panic [pf|div0|ud]\n");
        con.Printf("  (no arg) - direct panic\n");
        con.Printf("  pf       - page fault (null deref)\n");
        con.Printf("  div0     - divide by zero\n");
        con.Printf("  ud       - invalid opcode\n");
    }
}

// Forward declaration - CmdHelp needs the Commands array defined below
static void CmdHelp(const char* args, Stdlib::Printer& con);

static const CmdEntry Commands[] = {
    { "cls",       CmdCls,       "cls - clear screen" },
    { "cpu",       CmdCpu,       "cpu - dump cpu state" },
    { "dmesg",     CmdDmesg,     "dmesg [filter] - dump kernel log" },
    { "uptime",    CmdUptime,    "uptime - show uptime" },
    { "date",      CmdDate,      "date - show wall clock time" },
    { "ps",        CmdPs,        "ps - show tasks" },
    { "watchdog",  CmdWatchdog,  "watchdog - show watchdog stats" },
    { "memusage",  CmdMemusage,  "memusage - show memory usage stats" },
    { "irqstat",   CmdIrqstat,   "irqstat - show interrupt statistics" },
    { "pci",       CmdPci,       "pci - show pci devices" },
    { "disks",     CmdDisks,     "disks - list block devices" },
    { "partitions", CmdPartitions, "partitions <disk> - show partition table" },
    { "diskread",  CmdDiskread,  "diskread <disk> <sector> - read sector" },
    { "diskwrite", CmdDiskwrite, "diskwrite <disk> <sector> <hex> - write sector" },
    { "net",       CmdNet,       "net - list network devices" },
    { "arp",       CmdArp,       "arp - show ARP table" },
    { "icmpstat",  CmdIcmpstat,  "icmpstat - show ICMP statistics" },
    { "tcpstat",   CmdTcpstat,   "tcpstat - show TCP connections and statistics" },
    { "wget",      CmdWget,      "wget <url> - HTTP GET request" },
    { "udpsend",   CmdUdpsend,   "udpsend <ip> <port> <msg> - send UDP packet" },
    { "ping",      CmdPing,      "ping <ip|hostname> - send ICMP echo" },
    { "nslookup",  CmdNslookup,  "nslookup <hostname> - resolve hostname" },
    { "dnsflush",  CmdDnsflush,  "dnsflush - flush DNS cache" },
    { "dhcp",      CmdDhcp,      "dhcp [dev] - obtain IP via DHCP" },
    { "format",    CmdFormat,    "format nanofs <disk> - format disk" },
    { "mount",     CmdMount,     "mount <ramfs|nanofs> ... - mount filesystem" },
    { "umount",    CmdUmount,    "umount <path> - unmount filesystem" },
    { "mounts",    CmdMounts,    "mounts - list mount points" },
    { "ls",        CmdLs,        "ls <path> - list directory" },
    { "cat",       CmdCat,       "cat <path> - show file content" },
    { "write",     CmdWrite,     "write <path> <text> - write to file" },
    { "mkdir",     CmdMkdir,     "mkdir <path> - create directory" },
    { "touch",     CmdTouch,     "touch <path> - create empty file" },
    { "del",       CmdDel,       "del <path> - remove file or directory" },
    { "random",    CmdRandom,    "random [len] - get random bytes as hex" },
    { "version",   CmdVersion,   "version - show kernel version" },
    { "bt",        CmdBt,        "bt <pid> - show task backtrace" },
    { "panic",     CmdPanic,     "panic [pf|div0|ud] - trigger kernel panic" },
    { "poweroff",  CmdPoweroff,  "poweroff - power off (ACPI S5)" },
    { "shutdown",  CmdPoweroff,  nullptr },
    { "reboot",    CmdReboot,    "reboot - reset system" },
    { "help",      CmdHelp,      "help - help" },
    { nullptr,     nullptr,      nullptr },
};

static void CmdHelp(const char* args, Stdlib::Printer& con)
{
    (void)args;
    for (ulong i = 0; Commands[i].Name != nullptr; i++)
    {
        if (Commands[i].Help != nullptr)
            con.Printf("%s\n", Commands[i].Help);
    }
}

Cmd::Cmd()
    : TaskPtr(nullptr)
    , Shutdown(false)
    , Reboot(false)
    , Active(false)
{
    CmdLine[0] = '\0';
}

Cmd::~Cmd()
{
    if (TaskPtr != nullptr)
    {
        TaskPtr->Put();
        TaskPtr = nullptr;
    }
}

void Cmd::Dispatch(const char *cmd, Stdlib::Printer& out)
{
    bool found = false;
    for (ulong i = 0; Commands[i].Name != nullptr; i++)
    {
        ulong nameLen = Stdlib::StrLen(Commands[i].Name);
        if (Stdlib::StrCmp(cmd, Commands[i].Name) == 0)
        {
            Commands[i].Handler("", out);
            found = true;
            break;
        }
        else if (Stdlib::MemCmp(cmd, Commands[i].Name, nameLen) == 0 && cmd[nameLen] == ' ')
        {
            Commands[i].Handler(cmd + nameLen + 1, out);
            found = true;
            break;
        }
    }

    if (!found)
        out.Printf("command '%s' not found\n", cmd);
}

void Cmd::ProcessCmd(const char *cmd)
{
    auto& con = Console::GetInstance();
    Dispatch(cmd, con);
    con.Printf("$");
}

void Cmd::RequestShutdown()
{
    Shutdown = true;
}

void Cmd::RequestReboot()
{
    Reboot = true;
}

bool Cmd::ShouldShutdown()
{
    return Shutdown;
}

bool Cmd::ShouldReboot()
{
    return Reboot;
}

void Cmd::Stop()
{
    if (Active)
    {
        BugOn(TaskPtr == nullptr);
        Active = false;
        TaskPtr->SetStopping();
        TaskPtr->Wait();
    }
}

void Cmd::StopDhcp()
{
    GetDhcpClient().Stop();
}

bool Cmd::Start()
{
    if (TaskPtr != nullptr)
        return false;

    auto task = Mm::TAlloc<Task, Tag>("cmd");
    if (task == nullptr)
        return false;

    {
        Stdlib::AutoLock lock(Lock);
        if (TaskPtr == nullptr)
        {
            TaskPtr = task;
        }
    }

    if (TaskPtr != task)
    {
        task->Put();
        return false;
    }

    if (!TaskPtr->Start(&Cmd::RunFunc, this))
    {
        {
            Stdlib::AutoLock lock(Lock);
            task = TaskPtr;
            TaskPtr = nullptr;
        }
        task->Put();
        return false;
    }

    {
        Stdlib::AutoLock lock(Lock);
        Active = true;
    }
    return true;
}

void Cmd::ShowBanner(Stdlib::Printer& out)
{
    out.Printf("\n");
    out.Printf("  _   _  ___  ____\n");
    out.Printf(" | \\ | |/ _ \\/ ___|\n");
    out.Printf(" |  \\| | | | \\___ \\\n");
    out.Printf(" | |\\  | |_| |___) |\n");
    out.Printf(" |_| \\_|\\___/|____/\n");
    out.Printf("\n");
}

void Cmd::Run()
{
    size_t pos = 0;
    bool overflow = false;

    auto& con = Console::GetInstance();

    /* Wait for startup trace output to settle before suppressing console */
    Sleep(100 * Const::NanoSecsInMs);

    Tracer::GetInstance().SetConsoleSuppressed(true);

    ShowBanner(con);

    if (Parameters::GetInstance().IsDhcpAuto())
    {
        NetDevice* dev = NetDeviceTable::GetInstance().Find("eth0");
        if (dev)
        {
            con.Printf("DHCP auto on eth0...\n");
            if (GetDhcpClient().Start(dev))
            {
                for (ulong i = 0; i < 100 && !GetDhcpClient().IsReady(); i++)
                    Sleep(100 * Const::NanoSecsInMs);

                if (GetDhcpClient().IsReady())
                {
                    DhcpResult r = GetDhcpClient().GetResult();
                    con.Printf("DHCP ip: ");
                    r.Ip.Print(con);
                    con.Printf("\n");

                    if (Parameters::GetInstance().IsDnsEnabled() && !r.Dns.IsZero())
                    {
                        if (DnsResolver::GetInstance().Init(dev, r.Dns))
                        {
                            con.Printf("DNS resolver started, server: ");
                            r.Dns.Print(con);
                            con.Printf("\n");
                        }
                    }
                }
                else
                {
                    con.Printf("DHCP auto timeout\n");
                }
            }
            else
            {
                con.Printf("DHCP auto failed\n");
            }
        }
    }

    con.Printf("$");

    while (!Task::GetCurrentTask()->IsStopping())
    {
        KeyEvent keyEvent = {};
        bool hasEvent = false;
        bool backspace = false;
        {
            Stdlib::AutoLock lock(Lock);
            if (!Buf.IsEmpty())
            {
                keyEvent = Buf.Get();
                hasEvent = true;
                backspace = (keyEvent.Code == 0xE) ? true : false;
            }
        }

        if (hasEvent)
        {
            if (backspace)
            {
                if (pos > 0)
                    con.Backspace();
            }
            else
            {
                con.Printf("%c", keyEvent.Char);
            }

            if (keyEvent.Char == '\n')
            {
                CmdLine[pos] = '\0';
                if (!overflow)
                {
                    ProcessCmd(CmdLine);
                }
                else
                {
                    con.Printf("command too large\n");
                    con.Printf("$");
                    overflow = false;
                }
                Stdlib::MemSet(CmdLine, 0, sizeof(CmdLine));
                pos = 0;
            }
            else
            {
                if (pos < (Stdlib::ArraySize(CmdLine) - 1))
                {
                    if (backspace)
                    {
                        if (pos > 0)
                        {
                            pos--;
                            CmdLine[pos] = '\0';
                        }
                    }
                    else
                    {
                        CmdLine[pos++] = keyEvent.Char;
                    }
                }
                else
                {
                    overflow = true;
                }
            }
        }

        Sleep(10 * Const::NanoSecsInMs);
    }

    Tracer::GetInstance().SetConsoleSuppressed(false);
}

void Cmd::RunFunc(void *ctx)
{
    Cmd* cmd = static_cast<Cmd*>(ctx);
    cmd->Run();
}

void Cmd::OnChar(char c, u8 code)
{
    Stdlib::AutoLock lock(Lock);
    if (!Active)
        return;

    KeyEvent e;
    e.Char = c;
    e.Code = code;

    if (!Buf.Put(e))
    {
        Trace(0, "Can't save char");
        return;
    }
}

}
