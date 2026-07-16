#include "debug.h"
#include <hal/cpu.h>

namespace Kernel
{

volatile bool DebugWaitActive = false;

void DebugWait()
{
    DebugWaitActive = true;
    while (DebugWaitActive)
    {
        Pause();
    }	
}

}
