# Loadable kernel modules

A kernel service can be built on its own, as a module, and put into a running
kernel -- and taken out again -- without rebuilding or rebooting it:

```
$ wget http://10.0.2.2:8000/hello.ko /hello.ko
$ insmod /hello.ko
module: hello loaded at 0xFFFF8000...
$ hello nos
hello, nos -- call 1 since the module was loaded
$ lsmod
hello  24 KiB at 0xFFFF8000..., 7 kernel imports
$ rmmod hello
module: hello unloaded
```

Modules are written in Rust, and only in Rust. A module is a crate that runs
in the kernel and reaches it through exactly the API the drivers built into
the kernel use -- the `extern "C"` functions the `ffi` crate declares, wrapped
safely by `kcore` -- and through nothing else: the loader binds a module
against that API and refuses anything it does not recognise.

## Writing one

A module lives in `src/rust/modules/<name>/`. `hello` is the smallest one
worth reading:

```rust
#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use core::fmt::Write;
use kcore::cmd::Command;

struct Hello {
    _cmd: Command,
}

impl kmod::Module for Hello {}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    let cmd = Command::register("hello", "hello [name] - ...", |args, out| {
        let _ = writeln!(out, "hello, {}", args);
    })?;
    Ok(Box::new(Hello { _cmd: cmd }))
}

kmod::module!(name: "hello", init: init);
```

`kmod::module!` names the module -- what `lsmod` shows and `rmmod` takes, at
most 31 printable characters -- and says which function `insmod` runs. That
function builds the module's state and hands it back as a `Box<dyn
kmod::Module>`; an error instead fails the `insmod`. The kernel keeps the box
while the module is loaded, and `rmmod` drops it: there is no exit function to
write, because the module's `Drop` is its exit. Whatever the state holds --
the commands it registered, the tasks it started, the timers and interrupts
it took -- is released by the drops of the `kcore` handles in it, and must be,
because the module's code is freed right after. The state has to be `Send`:
the task that runs `rmmod` drops what the task that ran `insmod` built.

Dropping a handle is enough because every unregister the kernel offers waits
out a callback still running on another CPU before it returns: a command
(`kernel_cmd_unregister`), a timer, a legacy or MSI-X interrupt, a task
(`TaskHandle` joins it). A few registrations have no unregister at all: a block
device (`kcore::block::register`), a net device (`kcore::net::register`) and a
softirq handler (`kcore::softirq::register`) are the kernel's to call for as
long as it runs. A module that imports any of those is **permanent**: the
loader sees the import, `lsmod` says so, and `rmmod` refuses it -- the way
Linux keeps a module that has no exit function.

The `kmod` crate supplies the rest of what every module needs once: the
global allocator (the kernel heap, through `kernel_alloc`), the panic handler
(a module that panics panics the kernel, as the in-kernel Rust does) and the
header the loader looks for. `kcore::cmd` is how a module puts a command in
front of whoever runs the shell, on the console or over the [UDP
shell](udp-shell.md); the handler gets the rest of the command line and an
`Output` that is `core::fmt::Write`.

To add a module:

1. `src/rust/modules/<name>/Cargo.toml`, a `staticlib` whose library is named
   `mod_<name>`, depending on `kmod` and `kcore` (copy `hello`'s);
2. add `modules/<name>` to the workspace `members` in `src/rust/Cargo.toml`
   -- not to `default-members`, which is what the kernel's own build compiles;
3. add `<name>` to `MODULES` in the `Makefile`.

## Building

```
make modules                 # out/x86_64/modules/<name>.ko
make modules ARCH=aarch64    # out/aarch64/modules/<name>.ko
```

`make` and `make nocheck` build them too. Each module is compiled on its own,
with its own copy of `core`, `alloc`, `kcore` and `kmod`, into a target
directory of its own (`src/rust/target/modules`), because it is compiled with
flags the kernel's Rust is not:

- `-Crelocation-model=pic`, since nobody knows where it will be loaded;
- on x86, `-Ccode-model=small`: the kernel is built with the large code model
  so it can be linked in the top half of the address space, and a module has
  no need of that -- everything a PIC module addresses of its own is
  `rip`-relative, and everything of the kernel's goes through the GOT;
- compiler-builtins' own `memcpy`, `memset` and friends, so that a module
  imports nothing but the kernel's API.

The staticlib is then linked into an ELF shared object:

- `-Bsymbolic`, so every reference the module makes to itself is bound at
  link time and what is left for the loader is `RELATIVE` relocations and the
  kernel's functions;
- `-z separate-loadable-segments -z max-page-size=4096`: every segment starts
  on a page of its own, since page permissions are per page and each segment
  gets its own;
- a version script that exports `nos_module_info` and nothing else;
- `-z now`, `-z norelro`: every import is bound at load time, and there is no
  lazy binding and no dynamic linker to protect anything from.

`hello.ko` comes out around 25 KiB.

## Loading

`insmod <path>` reads a `.ko` off any mounted filesystem -- put it on the root
filesystem with `wget` from a web server on the build host, or build it into
a fresh root image with `scripts/mkrootfs.sh <image> <MiB> <dir>`. The loader
(`kernel/module.cpp`) then:

1. **checks the file**: ELF64, little-endian, a shared object for this CPU,
   headers inside the file. No thread-local or interpreter segment. Every
   loadable segment page-aligned, inside the file, clear of the others and
   never both writable and executable.
2. **binds its imports**, before mapping anything. Each undefined symbol in
   `.dynsym` is looked up by name in the kernel's export table; a module the
   kernel cannot satisfy is refused with the list of everything it lacks. A
   weak import may go unresolved and binds to 0.
3. **maps the image**: pages of its own, which need not be physically
   contiguous, mapped read-write and non-executable into one run of kernel
   virtual addresses. The segments are copied in; the zeroed tail of the last
   is `.bss`.
4. **relocates it**: every `RELA` section the loader is meant to see (`.rela.dyn`,
   `.rela.plt`). Three kinds of relocation reach it, and the architecture
   says which is which (`Hal::ClassifyModuleReloc`):

   | Meaning | x86-64 | arm64 |
   |---|---|---|
   | nothing | `NONE` | `NONE` |
   | load base + addend | `RELATIVE` | `RELATIVE` |
   | symbol + addend | `64`, `GLOB_DAT`, `JUMP_SLOT` | `ABS64`, `GLOB_DAT`, `JUMP_SLOT` |

   Anything else is refused. In practice x86 modules carry `RELATIVE` and
   `GLOB_DAT` relocations and arm64 ones `RELATIVE` and `JUMP_SLOT` -- calls
   through the PLT, whose slots are bound now rather than lazily.
5. **checks the header**, `nos_module_info`, now that its pointers are real:
   the magic, the layout version, the name, init and exit pointing into the
   module's own code -- and the kernel interface the module was built
   against (below).
6. **protects it**: each segment gets the permissions its program header
   asks for -- read-only, read-execute or read-write -- every CPU's TLB is
   shot down, and on arm64 the new code is cleaned from the data cache and
   invalidated in every CPU's instruction cache (`Hal::SyncInstructionCache`),
   which x86 does not need.
7. **runs its init**. A second module by the same name is refused before
   this, and an init that fails leaves nothing behind.

`rmmod <name>` runs the module's exit -- dropping its state -- and only then
unmaps and frees its pages. A command the module registered is taken away
with it, and `kernel_cmd_unregister` waits for any call of it still running
before it returns, so the code under a running command is never freed.
`rmmod` must not be run from the module's own command, which would wait for
itself.

Loads and unloads run in task context, one at a time: a module's init and exit
may sleep.

`poweroff` and `reboot` unload every module that can be unloaded, newest
first, once the shells have stopped and before the filesystems are unmounted
and the soft IRQs stop -- so a module's exit still has the kernel services it
may need, and nothing of the module is left running while they go.

## What a module may call

The export table is generated by the build, like the [symbol
table](debug.md): from `pass1.elf`, the kernel's first link, the Makefile
takes every function named in an `extern "C"` block of the `ffi` crate that the
kernel defines, and writes their names and addresses into
`out/<arch>/module_exports.S`, which the final link takes in. So what a module
can call is what an in-kernel Rust driver can call; adding a kernel service
for modules is the same three steps as adding one for the drivers (see
`.cursor/rules/rust-kernel-conventions.mdc`), and the table picks it up.

A module compiled against one version of those declarations and loaded into
a kernel built from another would call functions with arguments they no
longer take. The build hashes the `ffi` crate's sources into a digest that
goes into every module's header and into the kernel, and the loader refuses a
module whose digest is not the kernel's:

```
module: built against another kernel interface (ffi 3fa81c..., this kernel 9b02e7...) -- rebuild it from this tree
```

A module built outside the Makefile carries `unset` and is refused by a kernel
that was not.

## Testing

The boot self-test loads a module on both architectures. `modtest`
(`src/rust/modules/modtest`) is built with the kernel and embedded in its
image (`out/<arch>/modtest_blob.S`); `TestModules` in `kernel/test.cpp` loads
it, and its init checks what a loader can get wrong -- initialised data,
`.bss`, a table of function pointers, a trait object's vtable, allocations
from the kernel heap, a kernel object created and destroyed through the export
table -- and fails the load if anything comes out wrong. The test then runs
the command the module registered through the shell's dispatcher, checks a
second copy is refused, unloads it and checks the command went with it, twice;
and last feeds the loader damaged copies -- truncated, another machine's, an
import the kernel lacks, no header, a bad magic, another kernel interface --
each of which it must refuse.

## Limits

- A module's image is at most 512 KiB (`PageTable::MaxContiguousPages`
  pages): the largest block the page allocator maps in one piece. So is the
  `.ko` file.
- A module that registers a block device, a net device or a softirq handler
  cannot be unloaded (above).
- `rmmod` waits for a running call of the module's commands; one that never
  returns keeps `rmmod` -- and every `insmod`, `rmmod` and `lsmod` after it,
  which queue on the same lock -- waiting with it.
- Modules cannot call each other; each carries its own `kcore`, so they share
  no statics either.
- Only the functions `ffi` declares are exported -- not C++ classes, not
  arbitrary kernel symbols.
- No signatures: whoever can run `insmod` can run code in the kernel. On
  nos that is whoever can reach the shell, which already could.
