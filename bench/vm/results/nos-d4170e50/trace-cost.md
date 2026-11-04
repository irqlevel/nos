# What a tracepoint costs on the EX44, before and after d4170e50

`trace bench` (the shell's CPU, and `cpu=N` where the build has it), `hv bench
cpu=16 profile` (a P-core) and `cpu=62` (an E-core) with every event off and
with `trace on hv`, and `sched 3000`'s decision cost with `sched idle` off and
on, the VMs panel and ubuntu running on the same CPUs both times
(session script `ex44-ab.sh`). Then the exit's record split: `hv` on and the
rings frozen -- the site's work, the kernel's write returning at `frozen` --
against `hv` on. What it says is in docs/tracing.md, "What a tracepoint
costs".

## Before: a0ae873e

```
== version
nos dev (a0ae873e)
== hv cores
== trace bench, six times
trace bench: a tracepoint that is off, 0.451 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 87.759 ns a record (100000 records into a ring of its own)
trace bench: a tracepoint that is off, 0.465 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 87.759 ns a record (100000 records into a ring of its own)
trace bench: a tracepoint that is off, 0.520 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 87.759 ns a record (100000 records into a ring of its own)
trace bench: a tracepoint that is off, 0.472 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 87.759 ns a record (100000 records into a ring of its own)
trace bench: a tracepoint that is off, 0.499 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 87.760 ns a record (100000 records into a ring of its own)
trace bench: a tracepoint that is off, 0.485 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 87.760 ns a record (100000 records into a ring of its own)
== hv bench cpu=16, every event off
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 115 ms, 1152 ns an exit, 867401 a second (11 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 12, VMCS written 13, x87/SSE in 58, vmresume to exit 602, x87/SSE out 63, VMCS read 30; the rest -- the exit handled, the loop -- 374
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 115 ms, 1153 ns an exit, 866576 a second (12 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 12, VMCS written 13, x87/SSE in 58, vmresume to exit 603, x87/SSE out 63, VMCS read 30; the rest -- the exit handled, the loop -- 374
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 114 ms, 1144 ns an exit, 874010 a second (13 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 12, VMCS written 13, x87/SSE in 57, vmresume to exit 597, x87/SSE out 62, VMCS read 30; the rest -- the exit handled, the loop -- 373
== hv bench cpu=16, hv on
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 128 ms, 1286 ns an exit, 777140 a second (8 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 12, VMCS written 13, x87/SSE in 57, vmresume to exit 595, x87/SSE out 62, VMCS read 30; the rest -- the exit handled, the loop -- 517
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 128 ms, 1282 ns an exit, 779938 a second (8 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 12, VMCS written 12, x87/SSE in 57, vmresume to exit 593, x87/SSE out 62, VMCS read 30; the rest -- the exit handled, the loop -- 516
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 125 ms, 1257 ns an exit, 795160 a second (9 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 12, VMCS written 12, x87/SSE in 56, vmresume to exit 582, x87/SSE out 60, VMCS read 29; the rest -- the exit handled, the loop -- 506
== hv bench cpu=16, every event off again
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 114 ms, 1146 ns an exit, 871959 a second (9 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 12, VMCS written 13, x87/SSE in 57, vmresume to exit 599, x87/SSE out 62, VMCS read 30; the rest -- the exit handled, the loop -- 373
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 114 ms, 1148 ns an exit, 870510 a second (16 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 12, VMCS written 13, x87/SSE in 58, vmresume to exit 600, x87/SSE out 62, VMCS read 30; the rest -- the exit handled, the loop -- 373
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 115 ms, 1154 ns an exit, 866295 a second (7 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 12, VMCS written 13, x87/SSE in 58, vmresume to exit 603, x87/SSE out 63, VMCS read 30; the rest -- the exit handled, the loop -- 375
== hv bench cpu=62, every event off
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 198 ms, 1982 ns an exit, 504335 a second (9 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 27, VMCS written 26, x87/SSE in 75, vmresume to exit 963, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 691
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 197 ms, 1973 ns an exit, 506792 a second (2 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 26, VMCS written 26, x87/SSE in 75, vmresume to exit 959, x87/SSE out 123, VMCS read 76; the rest -- the exit handled, the loop -- 688
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 197 ms, 1972 ns an exit, 506946 a second (6 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 26, VMCS written 26, x87/SSE in 75, vmresume to exit 958, x87/SSE out 123, VMCS read 76; the rest -- the exit handled, the loop -- 688
== hv bench cpu=62, hv on
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 225 ms, 2256 ns an exit, 443067 a second (12 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 26, VMCS written 26, x87/SSE in 75, vmresume to exit 960, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 969
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 225 ms, 2251 ns an exit, 444131 a second (9 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 26, VMCS written 26, x87/SSE in 75, vmresume to exit 958, x87/SSE out 123, VMCS read 76; the rest -- the exit handled, the loop -- 967
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 225 ms, 2257 ns an exit, 442891 a second (10 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 26, VMCS written 26, x87/SSE in 75, vmresume to exit 961, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 969
== hv bench cpu=62, every event off again
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 197 ms, 1975 ns an exit, 506256 a second (8 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 26, VMCS written 26, x87/SSE in 75, vmresume to exit 959, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 689
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 197 ms, 1976 ns an exit, 505906 a second (3 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 26, VMCS written 26, x87/SSE in 75, vmresume to exit 960, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 689
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 198 ms, 1983 ns an exit, 504101 a second (8 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 27, VMCS written 26, x87/SSE in 75, vmresume to exit 964, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 691
== the scheduler's decisions, 3 s windows, sched idle off and on in turn
-- off
all     62    5436   <16us  <256us  <256us   9.2ms      4    <8us   <64us   16902   4.8us   1.5ms   1.3    3   0.4   3   0%
-- on
all     62    5435   <16us  <256us  <256us   9.2ms      6    <8us   <64us   19694   4.9us   1.5ms   1.2    3   0.4   3   0%
-- off
all     62    5394   <16us  <256us  <256us   9.2ms      4    <8us   <16us   16793   5.1us   1.5ms   1.3    3   0.4   3   0%
-- on
all     62    5402   <16us  <256us  <256us   9.2ms      3    <8us    <8us   16815   5.6us   1.5ms   1.3    3   0.4   3   0%
-- off
all     62    5440   <16us  <256us  <256us   9.2ms      4    <8us    <8us   18255   4.4us   1.5ms   1.2    3   0.4   3   0%
-- on
all     62    5445   <16us  <256us  <256us   9.2ms      5    <8us    <8us   18276   5.4us   1.5ms   1.2    3   0.4   3   0%
```

## After: d4170e50

```
== version
nos dev (d4170e50)
== hv cores
== trace bench, six times
trace bench: on cpu 56
trace bench: a tracepoint that is off, 0.459 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.309 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.597 ns (ordered: 8.597 ns), the slot's stores 3.306 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 4.312 ns (into 64 KiB: 3.306 ns)
trace bench: on cpu 56
trace bench: a tracepoint that is off, 0.477 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.308 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.597 ns (ordered: 8.597 ns), the slot's stores 3.363 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 3.273 ns (into 64 KiB: 3.363 ns)
trace bench: on cpu 56
trace bench: a tracepoint that is off, 0.452 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.309 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.597 ns (ordered: 8.598 ns), the slot's stores 3.433 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 3.116 ns (into 64 KiB: 3.433 ns)
trace bench: on cpu 56
trace bench: a tracepoint that is off, 0.520 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.309 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.597 ns (ordered: 8.597 ns), the slot's stores 3.343 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 4.475 ns (into 64 KiB: 3.343 ns)
trace bench: on cpu 56
trace bench: a tracepoint that is off, 0.537 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.308 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.597 ns (ordered: 8.598 ns), the slot's stores 3.351 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 3.259 ns (into 64 KiB: 3.351 ns)
trace bench: on cpu 56
trace bench: a tracepoint that is off, 0.459 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.308 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.597 ns (ordered: 8.598 ns), the slot's stores 3.395 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 3.575 ns (into 64 KiB: 3.395 ns)
== trace bench cpu=16, three times
trace bench: on cpu 16
trace bench: a tracepoint that is off, 0.537 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 16.081 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 7.563 ns, the cycle counter 5.500 ns (ordered: 5.581 ns), the slot's stores 2.409 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 2.427 ns (into 64 KiB: 2.409 ns)
trace bench: on cpu 16
trace bench: a tracepoint that is off, 0.512 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 16.060 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 7.563 ns, the cycle counter 5.339 ns (ordered: 5.355 ns), the slot's stores 2.277 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 2.306 ns (into 64 KiB: 2.277 ns)
trace bench: on cpu 16
trace bench: a tracepoint that is off, 0.499 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 16.063 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 7.730 ns, the cycle counter 5.325 ns (ordered: 5.321 ns), the slot's stores 2.283 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 2.301 ns (into 64 KiB: 2.283 ns)
== trace bench cpu=62, three times
trace bench: on cpu 62
trace bench: a tracepoint that is off, 0.476 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.308 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.600 ns (ordered: 8.598 ns), the slot's stores 3.217 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 4.438 ns (into 64 KiB: 3.217 ns)
trace bench: on cpu 62
trace bench: a tracepoint that is off, 0.499 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.309 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.598 ns (ordered: 8.600 ns), the slot's stores 3.212 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 3.177 ns (into 64 KiB: 3.212 ns)
trace bench: on cpu 62
trace bench: a tracepoint that is off, 0.510 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.309 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.598 ns (ordered: 8.598 ns), the slot's stores 3.192 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 4.434 ns (into 64 KiB: 3.192 ns)
== hv bench cpu=16, every event off
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 111 ms, 1111 ns an exit, 899616 a second (10 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 13, x87/SSE in 56, vmresume to exit 589, x87/SSE out 61, VMCS read 31; the rest -- the exit handled, the loop -- 348
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 112 ms, 1124 ns an exit, 889099 a second (14 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 13, x87/SSE in 57, vmresume to exit 596, x87/SSE out 62, VMCS read 31; the rest -- the exit handled, the loop -- 352
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 112 ms, 1123 ns an exit, 890173 a second (6 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 13, x87/SSE in 57, vmresume to exit 596, x87/SSE out 62, VMCS read 31; the rest -- the exit handled, the loop -- 351
== hv bench cpu=16, hv on
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 120 ms, 1204 ns an exit, 830055 a second (13 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 14, x87/SSE in 58, vmresume to exit 598, x87/SSE out 62, VMCS read 31; the rest -- the exit handled, the loop -- 428
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 118 ms, 1181 ns an exit, 846703 a second (12 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 13, x87/SSE in 56, vmresume to exit 586, x87/SSE out 61, VMCS read 30; the rest -- the exit handled, the loop -- 422
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 117 ms, 1178 ns an exit, 848561 a second (9 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 13, x87/SSE in 56, vmresume to exit 585, x87/SSE out 61, VMCS read 30; the rest -- the exit handled, the loop -- 420
== hv bench cpu=16, every event off again
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 113 ms, 1131 ns an exit, 883846 a second (7 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 14, x87/SSE in 58, vmresume to exit 600, x87/SSE out 63, VMCS read 31; the rest -- the exit handled, the loop -- 352
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 112 ms, 1121 ns an exit, 892045 a second (14 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 13, x87/SSE in 57, vmresume to exit 594, x87/SSE out 62, VMCS read 31; the rest -- the exit handled, the loop -- 351
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 113 ms, 1136 ns an exit, 880139 a second (12 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 14, x87/SSE in 58, vmresume to exit 602, x87/SSE out 63, VMCS read 31; the rest -- the exit handled, the loop -- 355
== hv bench cpu=62, every event off
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 194 ms, 1945 ns an exit, 513993 a second (6 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 75, vmresume to exit 966, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 658
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 194 ms, 1943 ns an exit, 514498 a second (9 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 75, vmresume to exit 966, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 656
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 194 ms, 1940 ns an exit, 515371 a second (10 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 75, vmresume to exit 964, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 655
== hv bench cpu=62, hv on
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 207 ms, 2074 ns an exit, 481989 a second (8 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 75, vmresume to exit 963, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 790
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 208 ms, 2082 ns an exit, 480188 a second (9 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 75, vmresume to exit 967, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 794
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 207 ms, 2077 ns an exit, 481432 a second (11 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 75, vmresume to exit 964, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 792
== hv bench cpu=62, every event off again
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 194 ms, 1941 ns an exit, 514957 a second (10 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 75, vmresume to exit 963, x87/SSE out 123, VMCS read 76; the rest -- the exit handled, the loop -- 658
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 194 ms, 1944 ns an exit, 514302 a second (8 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 75, vmresume to exit 966, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 657
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 194 ms, 1941 ns an exit, 515119 a second (11 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 75, vmresume to exit 965, x87/SSE out 124, VMCS read 76; the rest -- the exit handled, the loop -- 655
== the scheduler's decisions, 3 s windows, sched idle off and on in turn
-- off
all     61    5120   <16us  <128us  <128us   9.7ms      3   <64us   <64us   16263   5.0us 911.8us   1.3    3   0.4   3   0%
-- on
all     61    5123   <16us  <256us  <256us   9.7ms      6   <64us   <64us   17611   4.3us 911.8us   1.2    3   0.4   3   0%
-- off
all     61    5106   <16us  <256us  <256us   9.7ms      4   <64us   <64us   17621   4.2us 911.8us   1.2    3   0.4   3   0%
-- on
all     61    5095   <16us  <256us  <256us   9.7ms      4   <64us   <64us   16216   4.6us 911.8us   1.3    3   0.4   3   0%
-- off
all     61    5123   <16us  <256us  <256us   9.7ms      8   <32us   <64us   17352   4.2us 911.8us   1.2    3   0.4   3   0%
-- on
all     61    5149   <16us  <256us  <256us   9.7ms      8   <32us   <64us   17702   4.4us 911.8us   1.2    3   0.4   3   0%
```

## The exit's record split, d4170e50

```
== cpu 16: off
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 112 ms, 1127 ns an exit, 887107 a second (6 host interrupts among them)
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 112 ms, 1125 ns an exit, 888266 a second (12 host interrupts among them)
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 113 ms, 1135 ns an exit, 881055 a second (11 host interrupts among them)
== cpu 16: hv on, frozen
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 118 ms, 1182 ns an exit, 845629 a second (16 host interrupts among them)
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 119 ms, 1191 ns an exit, 839426 a second (15 host interrupts among them)
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 119 ms, 1193 ns an exit, 837710 a second (9 host interrupts among them)
== cpu 16: hv on
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 120 ms, 1207 ns an exit, 828140 a second (10 host interrupts among them)
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 120 ms, 1206 ns an exit, 828709 a second (8 host interrupts among them)
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 120 ms, 1201 ns an exit, 831982 a second (14 host interrupts among them)
== cpu 62: off
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 194 ms, 1946 ns an exit, 513832 a second (5 host interrupts among them)
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 195 ms, 1950 ns an exit, 512726 a second (8 host interrupts among them)
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 194 ms, 1943 ns an exit, 514511 a second (8 host interrupts among them)
== cpu 62: hv on, frozen
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 204 ms, 2048 ns an exit, 488274 a second (7 host interrupts among them)
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 204 ms, 2049 ns an exit, 487809 a second (13 host interrupts among them)
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 204 ms, 2041 ns an exit, 489763 a second (3 host interrupts among them)
== cpu 62: hv on
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 207 ms, 2078 ns an exit, 481191 a second (8 host interrupts among them)
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 208 ms, 2080 ns an exit, 480685 a second (9 host interrupts among them)
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 208 ms, 2080 ns an exit, 480701 a second (4 host interrupts among them)
```
