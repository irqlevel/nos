#include "debug.h"
#include "asm.h"

namespace Kernel
{
namespace Core
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
}
