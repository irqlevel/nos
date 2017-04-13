#include "debug.h"
#include "asm.h"

namespace Kernel
{

volatile bool DebugWaitActive;

void DebugWait()
{
    DebugWaitActive = true;
    while (DebugWaitActive)
    {
        Pause();
    }	
}

}
