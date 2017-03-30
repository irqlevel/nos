#include "debug.h"
#include "helpers32.h"

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
        pause_32();
    }	
}

}
}
