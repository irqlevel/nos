#include "time.h"

#include <drivers/pit.h>

namespace Kernel
{
    Stdlib::Time GetBootTime()
    {
        return Pit::GetInstance().GetTime();
    }
}