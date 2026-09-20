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

A log line is formatted into a 256-byte buffer (`Tracer::Output`), and a
panic's message into 512 (`Panicker::Message`, reached from Rust through
`kernel_panic`). What does not fit is **truncated and marked with `...`**,
never dropped: until `Stdlib::VsnPrintf` learned to truncate, a `%s` that
did not fit wrote none of its argument and returned -1, and `Tracer::Output`
threw away every line that reported it -- so the longest reports, a Rust
panic's message among them, reached the console as their prefix and nothing
else (`RUST PANIC: ` followed by the backtrace). `TestSnPrintf` covers it.

The boot sequence itself, and what each marker means, is in
[Boot](boot.md); `profile` and its two sample sources are in
[Profiler](profiler.md).

## Tests

The self-tests every boot runs, the smoke boots, and the gate each subsystem
has for what a smoke boot cannot notice are in [Tests and gates](testing.md).
The two lines worth knowing by heart: to run a single self-test, edit
`Test()` in `src/cpp/kernel/test.cpp` to call only that function; and gate
on a script's exit code, never on grepping its output.
