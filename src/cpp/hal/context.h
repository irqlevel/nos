#pragma once

// Kernel::Context is the arch-specific interrupt/task register frame.
// Common code passes Context* opaquely; the accessor method names
// (GetRetRip/GetErrorCode/GetOrigRsp) are the cross-arch contract.
#if defined(__x86_64__)
#include <arch/x86_64/context.h>
#elif defined(__aarch64__)
#include <arch/arm64/context.h>
#else
#error "unsupported architecture"
#endif
