# Round 12's trace windows (nos a0ae873e, the EX44)

The bench run of `bench/vm` with the tracepoints on for a few seconds at each
phase below (`out/ex44-bench/run2/round-c12.sh`: rings of 1 MiB a CPU, the
events on, the rings frozen, a dump a vCPU's CPU -- 41, 40, 33, 32 -- of 2048
records and the disk path's across every CPU of 4096, thawed). Read back by
`scripts/trace-decode.py --hv`, a window's dumps together. What it says, and
what to do about it, is in `plans/vm-perf-todo.md`.

## pipe: perf bench sched pipe, 2 s of `hv sched idle` on the vCPUs' CPUs

```
hv: 2009 exits of 4 vCPUs
  vcpu 0 (pid 48): 1322 exits over 463.25ms, 98.5% of it in its guest
  vcpu 1 (pid 46): 235 exits over 425.48ms, 1.4% of it in its guest, 1 entries turned back
  vcpu 2 (pid 50): 199 exits over 424.52ms, 1.7% of it in its guest, 1 entries turned back
  vcpu 3 (pid 51): 253 exits over 102.59ms, 8.1% of it in its guest
  reason           exits   share   out of the guest after it (the host's cost)
  msr                838   41.7%   p50 1.3us p99 9.0us max 18.8us
      tsc_deadline write            617   p50 1.3us p99 9.0us max 12.3us
      x2apic icr write              221   p50 1.9us p99 3.9us max 18.8us
  host               528   26.3%   p50 2.4us p99 7.1us max 22.0us
      vector 0xfd                   527   p50 2.4us p99 7.1us max 22.0us
      vector 0xfe                     1   none
  cpuid              285   14.2%   p50 4.0us p99 6.1us max 6.7us
      leaf 0x4                      149   p50 4.0us p99 4.9us max 6.3us
      leaf 0x2                       48   p50 1.5us p99 1.7us max 1.7us
      leaf 0xd                       29   p50 2.7us p99 6.1us max 6.1us
      leaf 0x7                       11   p50 3.0us p99 3.8us max 3.8us
      leaf 0x0                        9   p50 2.5us p99 3.9us max 6.7us
  hlt                208   10.4%   p50 953.9us p99 42.97ms max 103.96ms -- a halt's sleep, idle
  io                 147    7.3%   p50 1.4us p99 2.2us max 4.0us
      com1 in                       101   p50 1.4us p99 1.9us max 2.1us
      com1 out                       35   p50 1.4us p99 2.1us max 4.0us
      pic out                         6   p50 1.4us p99 1.9us max 2.0us
      acpi pm in                      3   p50 1.6us p99 1.6us max 2.2us
      pic in                          2   p50 1.5us p99 1.5us max 2.2us
  irq_window           1    0.0%   p50 1.6us p99 1.6us max 1.6us
  an interrupt of the host's, from the exit to its record: p50 1.4us p99 4.6us max 6.5us
  halts: 312 (163 timed, 54 rung, 49 polled, 46 advanced); halted p50 987.3us p99 10.02ms max 10.02ms; woke past its time p50 8.4us p99 16.6us max 18.6us
  timer interrupts after host: 477, late p50 3.3us p99 5.9us max 8.5us
  timer interrupts after woke_on_time: 95, late p50 1.0us p99 6.4us max 7.1us
  timer interrupts after exit: 2, late p50 55ns p99 55ns max 2.0us
  timer interrupts after other: 1, late p50 110ns p99 110ns max 110ns
  injected: pic 0x34 x2, window 0x0 x1
  rings notified: 116, posted to a vCPU in its guest and taken there, no entry needed
  rings rung: 58, to the vCPU's next entry p50 3.7us p99 17.6us max 18.5us
  rings polling: 48, to the vCPU's next entry p50 1.1us p99 1.3us max 1.4us
```

## cyclictest: cyclictest, 3 s of `hv sched idle` on the vCPUs' CPUs

```
hv: 1338 exits of 4 vCPUs
  vcpu 0 (pid 48): 505 exits over 84.06ms, 7.4% of it in its guest
  vcpu 1 (pid 46): 330 exits over 84.36ms, 5.3% of it in its guest
  vcpu 2 (pid 50): 298 exits over 84.09ms, 4.2% of it in its guest
  vcpu 3 (pid 51): 205 exits over 83.25ms, 3.1% of it in its guest
  reason           exits   share   out of the guest after it (the host's cost)
  msr                888   66.4%   p50 11.0us p99 14.2us max 16.9us
      tsc_deadline write            887   p50 11.0us p99 14.2us max 16.9us
      x2apic icr write                1   p50 14.2us p99 14.2us max 14.2us
  hlt                446   33.3%   p50 627.6us p99 2.29ms max 2.29ms -- a halt's sleep, idle
  host                 4    0.3%   p50 28.4us p99 29.3us max 30.4us
      vector 0xfe                     4   p50 28.4us p99 29.3us max 30.4us
  an interrupt of the host's, from the exit to its record: p50 19.4us p99 20.3us max 20.7us
  halts: 447 (446 timed, 1 rung); halted p50 613.9us p99 2.28ms max 2.28ms; woke past its time p50 17.3us p99 21.8us max 31.0us
  timer interrupts after woke_on_time: 446, late p50 8.7us p99 13.1us max 22.0us
  timer interrupts after exit: 1, late p50 665ns p99 665ns max 665ns
  rings rung: 1, to the vCPU's next entry p50 15.9us p99 15.9us max 15.9us
```

## read-qd1: fio 4k random reads at QD1, 2 s of `hv blk disk`

```
hv: 1063 exits of 3 vCPUs
  vcpu 0 (pid 48): 2 exits over 13.00ms, 0.0% of it in its guest
  vcpu 2 (pid 50): 18 exits over 20.25ms, 0.2% of it in its guest
  vcpu 3 (pid 51): 1043 exits over 26.49ms, 10.4% of it in its guest, 6 entries turned back
  reason           exits   share   out of the guest after it (the host's cost)
  hlt                495   46.6%   p50 48.0us p99 115.6us max 13.00ms -- a halt's sleep, idle
  io                 465   43.7%   p50 3.0us p99 3.1us max 4.1us
      vda notify out                465   p50 3.0us p99 3.1us max 4.1us
  msr                 84    7.9%   p50 1.3us p99 1.5us max 1.5us
      tsc_deadline write             82   p50 1.3us p99 1.5us max 1.5us
      x2apic icr write                2   p50 1.2us p99 1.2us max 1.2us
  host                13    1.2%   p50 2.3us p99 2.7us max 3.2us
      vector 0xfd                    12   p50 2.3us p99 2.6us max 2.7us
      vector 0xfe                     1   p50 3.2us p99 3.2us max 3.2us
  an interrupt of the host's, from the exit to its record: p50 1.4us p99 1.6us max 2.3us
  halts: 963 (862 polled, 76 rung, 21 advanced, 4 timed); halted p50 50.8us p99 63.8us max 10.01ms; woke past its time p50 0ns p99 1.9us max 10.3us
  timer interrupts after woke_on_time: 23, late p50 186ns p99 1.4us max 1.4us
  timer interrupts after exit: 5, late p50 2.2us p99 2.3us max 2.6us
  timer interrupts after host: 4, late p50 2.3us p99 2.6us max 2.7us
  rings polling: 390, to the vCPU's next entry p50 852ns p99 919ns max 933ns
  rings rung: 77, to the vCPU's next entry p50 2.4us p99 2.7us max 3.0us
disk: 465 requests, 464 of them whole in the window
  read            464   in all p50 49.1us p99 53.9us max 59.5us
      to its task   p50 4.9us p99 5.6us max 5.8us
      served        p50 43.3us p99 47.4us max 51.2us
      by the device p50 37.9us p99 41.1us max 42.4us (464 of them through it)
      back          p50 1.3us p99 1.5us max 11.2us
      served: 464 async
  device commands: 465, p50 37.9us p99 41.1us max 42.4us, at most 1 in flight
  3 records whose request is not in the window
```

## read-qd32: fio 4k random reads at QD32 (one job), 2 s of `hv blk disk`

```
hv: 1838 exits of 4 vCPUs
  vcpu 0 (pid 48): 9 exits over 521.1us, 2.7% of it in its guest, 1 entries turned back
  vcpu 1 (pid 46): 1150 exits over 8.25ms, 50.7% of it in its guest, 1 entries turned back
  vcpu 2 (pid 50): 2 exits over 5.0us, 69.9% of it in its guest
  vcpu 3 (pid 51): 677 exits over 8.25ms, 18.3% of it in its guest, 23 entries turned back
  reason           exits   share   out of the guest after it (the host's cost)
  io                1136   61.8%   p50 3.0us p99 8.3us max 11.2us
      vda notify out               1136   p50 3.0us p99 8.3us max 11.2us
  hlt                651   35.4%   p50 11.2us p99 21.8us max 486.7us -- a halt's sleep, idle
  msr                 16    0.9%   p50 1.4us p99 1.6us max 8.2us
      tsc_deadline write             16   p50 1.4us p99 1.6us max 8.2us
  host                10    0.5%   p50 2.5us p99 3.0us max 6.3us
      vector 0xfe                     6   p50 2.4us p99 3.0us max 6.3us
      vector 0xfd                     4   p50 2.5us p99 2.6us max 2.7us
  an interrupt of the host's, from the exit to its record: p50 1.7us p99 2.0us max 2.1us
  halts: 1307 (1262 polled, 40 rung, 3 timed, 2 advanced); halted p50 11.1us p99 22.2us max 9.95ms; woke past its time p50 0ns p99 0ns max 4.5us
  timer interrupts after exit: 5, late p50 1.5us p99 4.8us max 5.5us
  timer interrupts after woke_on_time: 4, late p50 1.4us p99 1.5us max 2.5us
  timer interrupts after host: 3, late p50 3.5us p99 3.5us max 3.9us
  rings polling: 572, to the vCPU's next entry p50 841ns p99 935ns max 1.1us
  rings rung: 62, to the vCPU's next entry p50 2.3us p99 2.6us max 3.1us
  rings notified: 18, posted to a vCPU in its guest and taken there, no entry needed
disk: 683 requests, 674 of them whole in the window
  read            674   in all p50 50.6us p99 70.5us max 82.7us
      to its task   p50 4.1us p99 9.1us max 13.9us
      served        p50 44.2us p99 63.9us max 80.4us
      by the device p50 38.2us p99 55.9us max 73.1us (674 of them through it)
      back          p50 1.8us p99 4.3us max 5.2us
      served: 674 async
  device commands: 678, p50 38.2us p99 56.8us max 73.1us, at most 9 in flight
  22 records whose request is not in the window
```

## write-qd1: fio 4k random writes at QD1, 2 s of `hv blk disk fs`

```
hv: 1677 exits of 4 vCPUs
  vcpu 0 (pid 48): 8 exits over 12.00ms, 0.1% of it in its guest
  vcpu 1 (pid 46): 822 exits over 13.73ms, 19.8% of it in its guest, 8 entries turned back
  vcpu 2 (pid 50): 6 exits over 673.1us, 1.6% of it in its guest
  vcpu 3 (pid 51): 841 exits over 13.69ms, 11.4% of it in its guest, 4 entries turned back
  reason           exits   share   out of the guest after it (the host's cost)
  hlt                799   47.6%   p50 27.3us p99 39.6us max 2.00ms -- a halt's sleep, idle
  msr                464   27.7%   p50 1.4us p99 1.8us max 3.6us
      x2apic icr write              396   p50 1.4us p99 1.8us max 3.6us
      tsc_deadline write             68   p50 1.2us p99 1.6us max 1.9us
  io                 389   23.2%   p50 4.3us p99 5.3us max 7.5us
      vda notify out                389   p50 4.3us p99 5.3us max 7.5us
  host                13    0.8%   p50 2.4us p99 2.6us max 2.7us
      vector 0xfd                    13   p50 2.4us p99 2.6us max 2.7us
  an interrupt of the host's, from the exit to its record: p50 1.5us p99 1.6us max 1.6us
  halts: 1195 (1138 polled, 31 rung, 18 advanced, 8 timed); halted p50 27.8us p99 39.6us max 9.32ms; woke past its time p50 0ns p99 1.8us max 4.3us
  timer interrupts after woke_on_time: 15, late p50 1.3us p99 2.6us max 2.9us
  timer interrupts after exit: 7, late p50 2.0us p99 3.2us max 3.4us
  timer interrupts after host: 7, late p50 2.4us p99 2.7us max 3.3us
  timer interrupts after other: 1, late p50 87ns p99 87ns max 87ns
  rings polling: 751, to the vCPU's next entry p50 968ns p99 1.1us max 1.6us
  rings rung: 32, to the vCPU's next entry p50 2.6us p99 3.5us max 3.8us
  rings notified: 1, posted to a vCPU in its guest and taken there, no entry needed
disk: 389 requests, 388 of them whole in the window
  write           388   in all p50 20.7us p99 24.6us max 30.9us
      to its task   p50 5.0us p99 5.7us max 14.5us
      served        p50 14.8us p99 18.6us max 25.1us
      by the device p50 9.4us p99 12.3us max 14.9us (388 of them through it)
      back          p50 807ns p99 891ns max 924ns
      served: 388 async
  device commands: 388, p50 9.4us p99 12.3us max 14.9us, at most 1 in flight
```

## fdatasync: fio 4k writes with an fdatasync each, 3 s of `hv blk disk fs`

```
hv: 1340 exits of 4 vCPUs
  vcpu 0 (pid 48): 845 exits over 128.56ms, 2.5% of it in its guest, 2 entries turned back
  vcpu 1 (pid 46): 15 exits over 118.82ms, 0.1% of it in its guest
  vcpu 2 (pid 50): 107 exits over 120.86ms, 0.4% of it in its guest
  vcpu 3 (pid 51): 373 exits over 128.55ms, 1.3% of it in its guest
  reason           exits   share   out of the guest after it (the host's cost)
  msr                649   48.4%   p50 4.5us p99 9.5us max 11.1us
      tsc_deadline write            461   p50 6.4us p99 8.2us max 9.4us
      x2apic icr write              188   p50 1.8us p99 10.6us max 11.1us
  hlt                502   37.5%   p50 459.0us p99 9.98ms max 51.99ms -- a halt's sleep, idle
  io                 183   13.7%   p50 3.3us p99 4.5us max 5.2us
      vda notify out                183   p50 3.3us p99 4.5us max 5.2us
  host                 4    0.3%   p50 2.7us p99 10.4us max 13.8us
      vector 0xfd                     3   p50 10.4us p99 10.4us max 13.8us
      vector 0xfe                     1   p50 2.7us p99 2.7us max 2.7us
  an interrupt of the host's, from the exit to its record: p50 1.7us p99 7.8us max 8.0us
  halts: 695 (462 rung, 118 advanced, 92 polled, 23 timed); halted p50 51.6us p99 10.01ms max 10.01ms; woke past its time p50 0ns p99 11.1us max 15.3us
  timer interrupts after woke_on_time: 129, late p50 893ns p99 4.0us max 6.2us
  timer interrupts after other: 2, late p50 199ns p99 199ns max 280ns
  timer interrupts after exit: 2, late p50 59ns p99 59ns max 2.0us
  rings rung: 282, to the vCPU's next entry p50 9.2us p99 15.4us max 17.0us
  rings polling: 87, to the vCPU's next entry p50 899ns p99 969ns max 975ns
  rings notified: 1, posted to a vCPU in its guest and taken there, no entry needed
disk: 183 requests, 182 of them whole in the window
  flush            91   in all p50 1.31ms p99 1.34ms max 1.35ms
      to its task   p50 5.0us p99 5.5us max 5.5us
      served        p50 1.30ms p99 1.33ms max 1.34ms
      back          p50 10.7us p99 11.1us max 12.9us
      served: 91 command
  write            91   in all p50 22.7us p99 26.4us max 31.0us
      to its task   p50 5.0us p99 5.2us max 5.2us
      served        p50 14.8us p99 18.3us max 18.3us
      by the device p50 9.4us p99 12.0us max 12.1us (91 of them through it)
      back          p50 2.8us p99 2.9us max 11.2us
      served: 91 async
  device commands: 91, p50 9.4us p99 12.0us max 12.1us, at most 1 in flight
```
