#include "rootfs.h"

extern "C" {

void rust_mount_root_fs();

}

namespace Kernel
{

void MountRootFs()
{
    rust_mount_root_fs();
}

}
