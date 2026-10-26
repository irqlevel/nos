#include "debug.h"
#include <hal/cpu.h>

namespace kernel
{

volatile bool debug_wait_active = false;

void debug_wait()
{
    debug_wait_active = true;
    while (debug_wait_active)
    {
        cpu_relax();
    }	
}

}
