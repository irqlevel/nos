/* tests/hv-passthrough-test.py's hostile guest: an NVMe controller the
 * guest's own driver has let go of, driven by hand from user space -- its
 * BAR0 mapped through sysfs, its admin queues in pages of this process's
 * whose addresses /proc/self/pagemap gives -- and told to write an Identify
 * where each argument says: "buf", a page of this process's, which it may;
 * or an address, where the guest has no RAM, or the interrupt range, where a
 * write is a message. "A+B" puts the Identify's first bytes at A, to the end
 * of A's page, and the rest at B -- the second PRP. The guest has no IOMMU of
 * its own: what it names is what the device's DMA asks of nos's domain.
 *
 *     nvmepoke /sys/bus/pci/devices/0000:00:02.0 buf 0x40000000 0xfee01ff0+buf buf
 *
 * Each command's completion is said on a line of its own,
 * "nvmepoke: <where> status 0x<sct/sc>" -- or "timeout" -- and of "buf" the
 * vendor id the controller wrote there. Built static: the guest's root is
 * busybox's. */

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

enum {
    PAGE = 4096,
    /* Controller registers (NVMe 1.4, 3.1) */
    REG_CAP = 0x00,
    REG_CC = 0x14,
    REG_CSTS = 0x1C,
    REG_AQA = 0x24,
    REG_ASQ = 0x28,
    REG_ACQ = 0x30,
    DOORBELLS = 0x1000,
    /* CC: enabled, 64-byte submission and 16-byte completion entries */
    CC_EN = 1,
    CC_IOSQES = 6 << 16,
    CC_IOCQES = 4 << 20,
    CSTS_RDY = 1,
    /* Two entries a queue, the least there may be */
    QUEUE = 2,
    SQE = 64,
    CQE = 16,
    OPC_IDENTIFY = 0x06,
    CNS_CONTROLLER = 1,
    /* The command register's memory decoding and bus mastering */
    COMMAND = 0x04,
    COMMAND_MEMORY = 1 << 1,
    COMMAND_MASTER = 1 << 2,
    WAIT_SECS = 10,
    /* Between one command and the next */
    PAUSE_US = 200 * 1000,
};

static volatile uint32_t* bar;

static uint32_t rd32(unsigned off)
{
    return bar[off / 4];
}

static void wr32(unsigned off, uint32_t v)
{
    bar[off / 4] = v;
}

static void wr64(unsigned off, uint64_t v)
{
    wr32(off, (uint32_t)v);
    wr32(off + 4, (uint32_t)(v >> 32));
}

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (double)t.tv_sec + (double)t.tv_nsec / 1e9;
}

/* The physical address of the page `p` is in: this process's, locked */
static uint64_t phys_of(const void* p)
{
    uint64_t e = 0;
    int fd = open("/proc/self/pagemap", O_RDONLY);
    if (fd < 0 || pread(fd, &e, sizeof(e), (off_t)((uintptr_t)p / PAGE * sizeof(e))) != sizeof(e))
        e = 0;
    if (fd >= 0)
        close(fd);
    if (!(e & (1ULL << 63)))
        return 0;
    return (e & ((1ULL << 55) - 1)) * PAGE;
}

static int ready(uint32_t want)
{
    double end = now() + WAIT_SECS;
    while ((rd32(REG_CSTS) & CSTS_RDY) != want) {
        if (now() > end)
            return 0;
        usleep(1000);
    }
    return 1;
}

int main(int argc, char** argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: nvmepoke <the device's sysfs dir> buf|<address>...\n");
        return 2;
    }
    char path[512];
    snprintf(path, sizeof(path), "%s/config", argv[1]);
    int cfg = open(path, O_RDWR);
    uint16_t command = 0;
    if (cfg < 0 || pread(cfg, &command, 2, COMMAND) != 2) {
        printf("nvmepoke: no config space at %s\n", path);
        return 1;
    }
    command |= COMMAND_MEMORY | COMMAND_MASTER;
    if (pwrite(cfg, &command, 2, COMMAND) != 2) {
        printf("nvmepoke: the command register not written\n");
        return 1;
    }
    snprintf(path, sizeof(path), "%s/resource0", argv[1]);
    int fd = open(path, O_RDWR | O_SYNC);
    bar = fd < 0 ? MAP_FAILED : mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (bar == MAP_FAILED) {
        printf("nvmepoke: BAR0 not mapped from %s\n", path);
        return 1;
    }
    uint8_t* mem = mmap(NULL, 3 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE | MAP_LOCKED,
        -1, 0);
    if (mem == MAP_FAILED) {
        printf("nvmepoke: no pages to lock\n");
        return 1;
    }
    memset(mem, 0, 3 * PAGE);
    uint8_t* sq = mem;
    uint8_t* cq = mem + PAGE;
    uint8_t* buf = mem + 2 * PAGE;
    uint64_t sq_at = phys_of(sq), cq_at = phys_of(cq), buf_at = phys_of(buf);
    if (!sq_at || !cq_at || !buf_at) {
        printf("nvmepoke: /proc/self/pagemap gives no addresses\n");
        return 1;
    }

    /* Off, the admin queues where they are, on */
    wr32(REG_CC, 0);
    if (!ready(0)) {
        printf("nvmepoke: the controller does not go off\n");
        return 1;
    }
    wr32(REG_AQA, ((QUEUE - 1) << 16) | (QUEUE - 1));
    wr64(REG_ASQ, sq_at);
    wr64(REG_ACQ, cq_at);
    wr32(REG_CC, CC_IOCQES | CC_IOSQES | CC_EN);
    if (!ready(CSTS_RDY)) {
        printf("nvmepoke: the controller does not come up\n");
        return 1;
    }
    uint32_t stride = 4u << ((((uint64_t)rd32(REG_CAP + 4)) & 0xF));
    unsigned sq_bell = DOORBELLS, cq_bell = DOORBELLS + stride;
    printf("nvmepoke: up, its queues at %#llx and %#llx, a page of ours at %#llx\n", (unsigned long long)sq_at,
        (unsigned long long)cq_at, (unsigned long long)buf_at);

    unsigned tail = 0, head = 0, phase = 1;
    for (int i = 2; i < argc; i++) {
        char first[64];
        snprintf(first, sizeof(first), "%s", argv[i]);
        char* plus = strchr(first, '+');
        if (plus)
            *plus++ = '\0';
        int to_buf = strcmp(first, "buf") == 0;
        uint64_t prp = to_buf ? buf_at : strtoull(first, NULL, 0);
        uint64_t prp2 = !plus ? 0 : strcmp(plus, "buf") == 0 ? buf_at : strtoull(plus, NULL, 0);
        memset(buf, 0, PAGE);
        uint32_t* cmd = (uint32_t*)(sq + tail * SQE);
        memset(cmd, 0, SQE);
        cmd[0] = OPC_IDENTIFY | ((uint32_t)i << 16);
        cmd[6] = (uint32_t)prp;
        cmd[7] = (uint32_t)(prp >> 32);
        cmd[8] = (uint32_t)prp2;
        cmd[9] = (uint32_t)(prp2 >> 32);
        cmd[10] = CNS_CONTROLLER;
        __sync_synchronize();
        tail = (tail + 1) % QUEUE;
        wr32(sq_bell, tail);

        volatile uint32_t* cqe = (volatile uint32_t*)(cq + head * CQE);
        double end = now() + WAIT_SECS;
        while (((cqe[3] >> 16) & 1) != phase && now() < end)
            usleep(1000);
        if (((cqe[3] >> 16) & 1) != phase) {
            printf("nvmepoke: %s timeout\n", argv[i]);
            fflush(stdout);
            return 1;
        }
        __sync_synchronize();
        printf("nvmepoke: %s status %#x\n", argv[i], (unsigned)(cqe[3] >> 17));
        if (to_buf)
            printf("nvmepoke: %s vendor %#06x\n", argv[i], (unsigned)(buf[0] | (buf[1] << 8)));
        fflush(stdout);
        head = (head + 1) % QUEUE;
        if (head == 0)
            phase ^= 1;
        wr32(cq_bell, head);
        /* A pause before the next: the fault this one made, if any, said by
           the host before the next one's comes -- a unit records so many
           faults, and one past them is lost */
        usleep(PAUSE_US);
    }
    return 0;
}
