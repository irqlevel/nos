#pragma once

#include "stdlib.h"

namespace Kernel
{

namespace Core
{

void PreemptActivate();

bool PreemptIsActive();

void PreemptDisable();

void PreemptEnable();

}
}