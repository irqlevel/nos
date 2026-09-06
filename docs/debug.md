# Debug

## GDB

Start QEMU with `-s` (GDB server on port 1234), then:

```sh
gdb -ex "symbol-file bin/kernel64.elf" \
    -ex "set architecture i386:x86-64" \
    -ex "target remote :1234"
# or: ./scripts/gdb64.sh
```

arm64 (start `./scripts/qemu-arm64.sh -s`, needs `gdb-multiarch`):

```sh
./scripts/gdb-arm64.sh
```

Stack traces resolve symbols from a table baked into the kernel by a two-pass
link (`out/$(ARCH)/pass1.elf` → `nm` → `symtab_data.cpp` → final ELF), so
`bt <pid>` and the panic handler name functions without an external symbol file.

## Without a serial port

On a machine with no UART, or a cloud VM, the kernel log can leave the box
over the network instead:

- [Netconsole](netconsole.md) — `netconsole=ip:port` streams the whole kernel
  log, panic report included, to a UDP collector as each line is produced.
- [UDP remote shell](udp-shell.md) — `udpshell=PORT` runs shell commands
  (`dmesg`, `bt`, `profile`, …) from a remote machine.
- `loglevel=N` at boot or `loglevel N` in the shell raises the trace level
  without a rebuild; see [Kernel parameters](kernel-parameters.md).

## Boot tests

There is no separate test binary: self-tests live in `src/cpp/kernel/test.cpp`
and run early in boot, from `Test::Test()` and `Test::TestMultiTasking()`. A
failing test returns a non-success `Stdlib::Error`. To run a single test, edit
`Test()` to call only that function, rebuild, boot, and watch `nos.log`.

`./scripts/smoke-test.sh` (x86-64) and `./scripts/smoke-arm64.sh` (arm64)
boot the kernel headless and assert the serial markers `After test` →
`Preempt is now on` → `boot: complete`, failing fast on `PANIC:`. Gate on the
exit code, never on grepping the output.
