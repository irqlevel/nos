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

Stack traces resolve symbols from a table baked into the kernel by a
three-pass link (`out/$(ARCH)/pass2.elf` → `nm` → `symtab_data.cpp` → final
ELF, checked against the final ELF; [Build](build.md)), so `bt <pid>` and the
panic handler name functions without an external symbol file. A frame is a
return address, named by the call before it (`SymbolTable::DescribeReturn`):
after a call to a function that does not return -- every panic's -- the
address is the next function's first byte, and naming it as it stood put the
frame under that next function at `+0x0`.

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

Every format is C's printf, checked by the compiler: the functions that
take one carry `PRINTF_FORMAT` (`include/types.h`), the build has
`-Wformat=2 -Werror`, and `VsnPrintf` reads each argument as the length
modifier names it -- an int under `%u`, a `ulong` under `%lu`. Until then
every integer conversion read a 64-bit slot and the convention was to cast
each argument to `ulong`; an argument that was not -- the trace level
itself, in every `Trace` -- was read with whatever the other half of its
register or stack slot held. A format that is not a literal at its call is
a build error, so none can slip past the check through a variable or a
template. `cpp-fuzz.py`'s `format` target holds `VsnPrintf` to the host's
own `vsnprintf`.

The boot sequence itself, and what each marker means, is in
[Boot](boot.md); `profile` and its two sample sources are in
[Profiler](profiler.md).

## Tests

The self-tests every boot runs, the smoke boots, and the gate each subsystem
has for what a smoke boot cannot notice are in [Tests and gates](testing.md).
The two lines worth knowing by heart: to run a single self-test, edit
`Test()` in `src/cpp/kernel/test.cpp` to call only that function; and gate
on a script's exit code, never on grepping its output.
