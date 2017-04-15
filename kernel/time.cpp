#include "time.h"

#include <drivers/pit.h>

namespace Kernel
{
    Shared::Time GetBootTime()
    {
        return Pit::GetInstance().GetTime();
    }
}