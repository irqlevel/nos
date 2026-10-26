#pragma once

namespace kernel
{

/* The runtime of clang's undefined-behaviour sanitizer, in a kernel built
   with UBSAN=1 (docs/build.md). In any other build these do nothing and the
   handlers in ubsan.cpp are never called. */
namespace ubsan
{
    /* ubsan=warn: a report is printed and the kernel goes on, so that one
       boot collects every report there is. Without it the first report is
       a panic -- which is what makes every boot gate a UB gate. Set while
       the command line is parsed; before that, a report panics. */
    void set_warn_only(bool on);

    /* A line in the boot log saying the kernel is instrumented, and which
       way a report goes. Nothing in a build without the sanitizer. */
    void announce();

    /* Whether this kernel was built with the sanitizer. */
    bool enabled();

    /* Reports printed so far, each from a site of its own, and failed
       checks that were not printed because another report was under way. */
    unsigned long reports();
    unsigned long unprinted();
}

}
