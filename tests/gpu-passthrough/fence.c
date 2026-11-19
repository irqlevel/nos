/* tests/gpu-passthrough-test.py, in the guest: fences on the GPU through
 * nouveau, each an EXEC of nothing whose fence the kernel puts on the
 * channel, waited for on its syncobj -- a wait only the GPU's interrupt ends.
 * How many came back, and how fast. Built static on the machine with the GPU
 * (make-root.sh), against its kernel's uAPI headers.
 *
 *   fence [count]
 */
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#include <drm/drm.h>
#include <drm/nouveau_drm.h>

/* Mesa's: the kernel's part of the GPU's address space, past what a client
 * maps itself */
#define KERNEL_VA_START (1ULL << 39)
#define KERNEL_VA_SIZE (1ULL << 32)
#define WAIT_NS 5000000000LL

static int64_t now_ns(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (int64_t)t.tv_sec * 1000000000LL + t.tv_nsec;
}

int main(int argc, char** argv)
{
    int count = argc > 1 ? atoi(argv[1]) : 100;
    int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        perror("gpu-fence: renderD128");
        return 1;
    }
    struct drm_nouveau_vm_init vm = { .kernel_managed_addr = KERNEL_VA_START, .kernel_managed_size = KERNEL_VA_SIZE };
    if (ioctl(fd, DRM_IOCTL_NOUVEAU_VM_INIT, &vm)) {
        perror("gpu-fence: VM_INIT");
        return 1;
    }
    struct drm_nouveau_channel_alloc chan;
    memset(&chan, 0, sizeof(chan));
    if (ioctl(fd, DRM_IOCTL_NOUVEAU_CHANNEL_ALLOC, &chan)) {
        perror("gpu-fence: CHANNEL_ALLOC");
        return 1;
    }
    struct drm_syncobj_create sync = { .handle = 0, .flags = 0 };
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &sync)) {
        perror("gpu-fence: SYNCOBJ_CREATE");
        return 1;
    }
    printf("gpu-fence: channel %d, %d fences\n", chan.channel, count);

    int64_t start = now_ns(), worst = 0;
    int done = 0;
    for (int i = 0; i < count; i++) {
        struct drm_nouveau_sync sig = { .flags = DRM_NOUVEAU_SYNC_SYNCOBJ, .handle = sync.handle };
        struct drm_nouveau_exec exec = { .channel = (uint32_t)chan.channel, .push_count = 0, .wait_count = 0,
                                         .sig_count = 1, .sig_ptr = (uint64_t)(uintptr_t)&sig };
        int64_t t0 = now_ns();
        if (ioctl(fd, DRM_IOCTL_NOUVEAU_EXEC, &exec)) {
            perror("gpu-fence: EXEC");
            break;
        }
        uint32_t handle = sync.handle;
        struct drm_syncobj_wait wait = { .handles = (uint64_t)(uintptr_t)&handle, .timeout_nsec = t0 + WAIT_NS,
                                         .count_handles = 1, .flags = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT };
        if (ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &wait)) {
            printf("gpu-fence: fence %d not signalled in 5 s: ", i);
            fflush(stdout);
            perror("");
            break;
        }
        int64_t took = now_ns() - t0;
        if (took > worst) {
            worst = took;
        }
        done++;
        struct drm_syncobj_array reset = { .handles = (uint64_t)(uintptr_t)&handle, .count_handles = 1 };
        if (ioctl(fd, DRM_IOCTL_SYNCOBJ_RESET, &reset)) {
            perror("gpu-fence: SYNCOBJ_RESET");
            break;
        }
    }
    int64_t all = now_ns() - start;
    printf("gpu-fence: %d of %d fences signalled, %lld us each on average, %lld us at most\n", done, count,
           done ? (long long)(all / done / 1000) : 0LL, (long long)(worst / 1000));
    return done == count ? 0 : 1;
}
