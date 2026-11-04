# What a tracepoint costs on the EX44 at 6ecddfef

The same measurements as nos-d4170e50/trace-cost.md's, after the exit's
record stopped reading the counter at its site (since_ns a stamp's low
half), exit_fields and times went in line and kernel_tp_record became a
jump: `trace bench` as at d4170e50 (its path is unchanged), and `hv bench`'s
exit with `trace on hv` 1164 ns against 1121 off on a P-core (cpu 16), 2059
against 1954 on an E-core (cpu 62). Then the split: `hv` on and the rings
frozen -- the site's work alone -- against `hv` on.

## The suite

```
== version
nos dev (6ecddfef)
== hv cores
== trace bench, six times
trace bench: on cpu 56
trace bench: a tracepoint that is off, 0.525 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.309 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.597 ns (ordered: 8.598 ns), the slot's stores 3.298 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 4.058 ns (into 64 KiB: 3.298 ns)
trace bench: on cpu 56
trace bench: a tracepoint that is off, 0.499 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.309 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.598 ns (ordered: 8.597 ns), the slot's stores 3.301 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 4.295 ns (into 64 KiB: 3.301 ns)
trace bench: on cpu 56
trace bench: a tracepoint that is off, 0.492 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.308 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.597 ns (ordered: 8.597 ns), the slot's stores 3.339 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 3.702 ns (into 64 KiB: 3.339 ns)
trace bench: on cpu 56
trace bench: a tracepoint that is off, 0.535 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.309 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.597 ns (ordered: 8.599 ns), the slot's stores 3.330 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 3.888 ns (into 64 KiB: 3.330 ns)
trace bench: on cpu 56
trace bench: a tracepoint that is off, 0.474 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.308 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.597 ns (ordered: 8.597 ns), the slot's stores 3.388 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 3.113 ns (into 64 KiB: 3.388 ns)
trace bench: on cpu 56
trace bench: a tracepoint that is off, 0.467 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.309 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.597 ns (ordered: 8.597 ns), the slot's stores 3.348 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 4.109 ns (into 64 KiB: 3.348 ns)
== trace bench cpu=16, three times
trace bench: on cpu 16
trace bench: a tracepoint that is off, 0.508 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 16.077 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 8.408 ns, the cycle counter 5.571 ns (ordered: 5.568 ns), the slot's stores 2.420 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 2.406 ns (into 64 KiB: 2.420 ns)
trace bench: on cpu 16
trace bench: a tracepoint that is off, 0.538 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 16.061 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 8.568 ns, the cycle counter 5.572 ns (ordered: 5.568 ns), the slot's stores 2.405 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 2.393 ns (into 64 KiB: 2.405 ns)
trace bench: on cpu 16
trace bench: a tracepoint that is off, 0.574 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 16.066 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 8.528 ns, the cycle counter 5.571 ns (ordered: 5.567 ns), the slot's stores 2.402 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 2.416 ns (into 64 KiB: 2.402 ns)
== trace bench cpu=62, three times
trace bench: on cpu 62
trace bench: a tracepoint that is off, 0.470 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.309 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.598 ns (ordered: 8.598 ns), the slot's stores 3.185 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 3.848 ns (into 64 KiB: 3.185 ns)
trace bench: on cpu 62
trace bench: a tracepoint that is off, 0.473 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.308 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.598 ns (ordered: 8.598 ns), the slot's stores 3.896 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 3.243 ns (into 64 KiB: 3.896 ns)
trace bench: on cpu 62
trace bench: a tracepoint that is off, 0.487 ns over an empty loop's iteration (10000000 loops)
trace bench: one that is on, 10.309 ns a record (100000 records into a ring of its own)
trace bench: of it, each over an empty loop's iteration: interrupts off and back 1.717 ns, the cycle counter 8.600 ns (ordered: 8.598 ns), the slot's stores 3.192 ns
trace bench: the slot's stores into a ring of 1024 KiB, as `trace size` makes them: 3.980 ns (into 64 KiB: 3.192 ns)
== hv bench cpu=16, every event off
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 112 ms, 1128 ns an exit, 886244 a second (19 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 14, x87/SSE in 58, vmresume to exit 597, x87/SSE out 62, VMCS read 31; the rest -- the exit handled, the loop -- 353
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 112 ms, 1120 ns an exit, 892662 a second (8 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 13, x87/SSE in 57, vmresume to exit 594, x87/SSE out 62, VMCS read 31; the rest -- the exit handled, the loop -- 350
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 113 ms, 1139 ns an exit, 877603 a second (11 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 14, x87/SSE in 58, vmresume to exit 603, x87/SSE out 63, VMCS read 31; the rest -- the exit handled, the loop -- 357
== hv bench cpu=16, hv on
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 114 ms, 1146 ns an exit, 872491 a second (14 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 12, VMCS written 13, x87/SSE in 56, vmresume to exit 579, x87/SSE out 60, VMCS read 30; the rest -- the exit handled, the loop -- 396
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 118 ms, 1180 ns an exit, 847445 a second (12 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 13, x87/SSE in 58, vmresume to exit 597, x87/SSE out 62, VMCS read 31; the rest -- the exit handled, the loop -- 406
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 116 ms, 1167 ns an exit, 856745 a second (11 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 13, x87/SSE in 57, vmresume to exit 590, x87/SSE out 61, VMCS read 31; the rest -- the exit handled, the loop -- 402
== hv bench cpu=16, every event off again
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 112 ms, 1120 ns an exit, 892337 a second (11 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 13, VMCS written 13, x87/SSE in 57, vmresume to exit 594, x87/SSE out 62, VMCS read 31; the rest -- the exit handled, the loop -- 350
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 110 ms, 1108 ns an exit, 901817 a second (13 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 12, VMCS written 13, x87/SSE in 57, vmresume to exit 587, x87/SSE out 61, VMCS read 30; the rest -- the exit handled, the loop -- 348
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 111 ms, 1111 ns an exit, 899712 a second (12 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 12, VMCS written 13, x87/SSE in 57, vmresume to exit 589, x87/SSE out 61, VMCS read 31; the rest -- the exit handled, the loop -- 348
== hv bench cpu=62, every event off
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 195 ms, 1954 ns an exit, 511744 a second (10 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 74, vmresume to exit 962, x87/SSE out 128, VMCS read 76; the rest -- the exit handled, the loop -- 668
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 195 ms, 1953 ns an exit, 512004 a second (10 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 74, vmresume to exit 961, x87/SSE out 128, VMCS read 76; the rest -- the exit handled, the loop -- 668
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 194 ms, 1949 ns an exit, 512992 a second (7 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 74, vmresume to exit 960, x87/SSE out 127, VMCS read 76; the rest -- the exit handled, the loop -- 666
== hv bench cpu=62, hv on
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 206 ms, 2061 ns an exit, 485068 a second (6 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 21, VMCS written 26, x87/SSE in 74, vmresume to exit 962, x87/SSE out 127, VMCS read 76; the rest -- the exit handled, the loop -- 775
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 205 ms, 2053 ns an exit, 486904 a second (5 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 21, VMCS written 26, x87/SSE in 74, vmresume to exit 959, x87/SSE out 126, VMCS read 76; the rest -- the exit handled, the loop -- 771
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 206 ms, 2063 ns an exit, 484510 a second (7 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 21, VMCS written 26, x87/SSE in 74, vmresume to exit 963, x87/SSE out 128, VMCS read 76; the rest -- the exit handled, the loop -- 775
== hv bench cpu=62, every event off again
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 195 ms, 1959 ns an exit, 510320 a second (11 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 75, vmresume to exit 964, x87/SSE out 127, VMCS read 78; the rest -- the exit handled, the loop -- 669
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 195 ms, 1958 ns an exit, 510711 a second (8 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 74, vmresume to exit 964, x87/SSE out 127, VMCS read 77; the rest -- the exit handled, the loop -- 670
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 195 ms, 1951 ns an exit, 512375 a second (5 host interrupts among them)
hv: an exit, in ns: checks and VMPTRLD 20, VMCS written 26, x87/SSE in 74, vmresume to exit 962, x87/SSE out 126, VMCS read 76; the rest -- the exit handled, the loop -- 667
== the scheduler's decisions, 3 s windows, sched idle off and on in turn
all     61    5112   <16us  <128us  <256us   9.4ms      4   <64us   <64us   17642   4.8us 831.8us   1.2    3   0.4   3   0%
all     61    5094   <16us  <128us  <128us   9.4ms      4   <64us   <64us   17640   4.9us 831.8us   1.2    3   0.4   3   0%
all     61    5081   <16us  <128us  <128us   9.4ms      6   <64us   <64us   16179   5.1us 831.8us   1.3    3   0.4   3   0%
all     61    5129   <16us  <128us  <256us   9.4ms      4   <64us   <64us   16568   5.3us 831.8us   1.3    3   0.4   3   0%
all     61    5123   <16us  <128us  <128us   9.4ms      4   <16us   <64us   17655   4.9us 831.8us   1.2    3   0.4   3   0%
all     61    5115   <16us  <128us  <256us   9.4ms      4   <64us   <64us   17692   5.0us 831.8us   1.2    3   0.4   3   0%
```

## The exit's record split

```
== cpu 16: off
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 112 ms, 1125 ns an exit, 888163 a second (9 host interrupts among them)
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 112 ms, 1128 ns an exit, 886183 a second (4 host interrupts among them)
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 112 ms, 1121 ns an exit, 891412 a second (11 host interrupts among them)
== cpu 16: hv on, frozen
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 116 ms, 1163 ns an exit, 859384 a second (8 host interrupts among them)
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 115 ms, 1151 ns an exit, 868398 a second (6 host interrupts among them)
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 113 ms, 1138 ns an exit, 878042 a second (9 host interrupts among them)
== cpu 16: hv on
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 115 ms, 1150 ns an exit, 869149 a second (13 host interrupts among them)
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 117 ms, 1179 ns an exit, 847833 a second (11 host interrupts among them)
hv: 100000 exits on cpu 16, 0 pages read between, ASIDs kept: 118 ms, 1183 ns an exit, 844737 a second (13 host interrupts among them)
== cpu 62: off
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 195 ms, 1953 ns an exit, 511806 a second (5 host interrupts among them)
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 195 ms, 1957 ns an exit, 510925 a second (10 host interrupts among them)
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 194 ms, 1948 ns an exit, 513093 a second (12 host interrupts among them)
== cpu 62: hv on, frozen
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 202 ms, 2027 ns an exit, 493174 a second (8 host interrupts among them)
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 202 ms, 2027 ns an exit, 493202 a second (11 host interrupts among them)
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 202 ms, 2028 ns an exit, 492983 a second (10 host interrupts among them)
== cpu 62: hv on
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 206 ms, 2062 ns an exit, 484739 a second (6 host interrupts among them)
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 206 ms, 2061 ns an exit, 485009 a second (6 host interrupts among them)
hv: 100000 exits on cpu 62, 0 pages read between, ASIDs kept: 205 ms, 2058 ns an exit, 485820 a second (11 host interrupts among them)
```
