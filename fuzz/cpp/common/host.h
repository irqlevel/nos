// The host's headers, which every file of the fuzzers includes before
// anything else. Two things make the order matter. The kernel's mm/new.h
// declares the placement operator new, which the host's <new> defines with
// an attribute a later declaration may not add -- so <new> has to come
// first. And the host's <stdio.h> makes EOF a macro, which lib/error.h's
// Error::EOF must not meet -- so it is undefined here, after the host's
// headers have used it and before the kernel's are read.
#pragma once

#include <errno.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <algorithm>
#include <map>
#include <new>
#include <set>
#include <string>
#include <vector>

#undef EOF
