---
title: "Write a loadable kernel module in Rust"
description: "Create a nos kernel module in Rust with kmod and kcore, give it a shell command, build the .ko and load it into a running kernel."
order: 7
updated: 2026-10-22
---

By the end of this guide you will have a module of your own: a crate under `src/rust/modules/`, built into a `.ko` file, that registers a shell command. You will load it into a running nos with `insmod` and take it out again with `rmmod`, without rebuilding or rebooting the kernel.

You need:

- the nos tree and a working build, natively or in Docker ([Build nos](/howto/build));
- a nos to load the module into, for example in QEMU ([Run nos in QEMU](/howto/run-in-qemu));
- some Rust: modules are `#![no_std]` crates.

## What a module is

- **Rust only.** A module is a `staticlib` crate, linked into a position-independent ELF shared object: the `.ko`.
- **It reaches the kernel through one API.** That API is the `extern "C"` functions the `ffi` crate declares, wrapped safely by `kcore` — the same API the drivers built into the kernel use. The loader binds a module against an export table generated from those declarations and refuses anything else.
- **`insmod` runs the module's init; `rmmod` drops the state it returned.** The module's `Drop` is its exit: there is no exit function to write.
- **A module is for what a machine can run without.** A server, a load generator or a diagnostic is a module: `sshd`, `netblk`, `netload`, `blkload`, `heapbench` and the hypervisor, `hv`, are all modules.
- **A module cannot be a disk's or a NIC's driver.** Those register with their layer as Rust trait objects from inside the kernel image, and there is no C name for a module to bind to. A module uses disks and NICs; it does not provide them.

## Read the smallest real one: `hello`

`src/rust/modules/hello/` holds two files. `Cargo.toml`:

```toml
[package]
name = "mod-hello"
version = "0.1.0"
edition = "2021"

[lib]
name = "mod_hello"
crate-type = ["staticlib"]

[dependencies]
kmod = { path = "../../kmod" }
kcore = { path = "../../kcore" }
```

And [`src/lib.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/modules/hello/src/lib.rs), with only its doc comment cut:

```rust
#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};
use kcore::cmd::Command;

static CALLS: AtomicU64 = AtomicU64::new(0);

struct Hello {
    _cmd: Command,
}

impl kmod::Module for Hello {}

impl Drop for Hello {
    fn drop(&mut self) {
        kcore::trace!(0, "hello: unloaded after {} calls", CALLS.load(Ordering::Relaxed));
    }
}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    let cmd = Command::register("hello", "hello [name] - a command from a loadable module", |args, out| {
        let calls = CALLS.fetch_add(1, Ordering::Relaxed) + 1;
        let who = if args.is_empty() { "world" } else { args };
        let _ = writeln!(out, "hello, {} -- call {} since the module was loaded", who, calls);
    })?;

    kcore::trace!(0, "hello: loaded");
    Ok(Box::new(Hello { _cmd: cmd }))
}

kmod::module!(name: "hello", init: init);
```

What each part does:

- **`kmod::module!(name: "hello", init: init)`** names the module and says which function `insmod` runs. The name is what `lsmod` shows and `rmmod` takes, at most 31 printable characters; a longer one fails the build. The macro emits `nos_module_info`, the header the loader looks for.
- **`init`** builds the module's state and returns it as a `Box<dyn kmod::Module>`. An error instead fails the `insmod`, and the kernel log says `module init failed: ...`.
- **`Hello`, the state,** owns everything the module registered. Holding the `Command` keeps the command registered; `rmmod` drops `Hello`, and the `Command`'s drop unregisters it. Every unregister in `kcore` waits out a callback still running on another CPU, so the code under the command is never freed while it runs. The `Drop` impl here only adds a log line.
- **`impl kmod::Module for Hello {}`** requires `Send`: the task that runs `rmmod` drops what the task that ran `insmod` built.
- **`Command::register(name, help, handler)`** puts a command in front of whoever runs the shell: the console, the UDP shell or an SSH session. The handler is `Fn(&str, &mut Output) + Send + Sync + 'static`. It gets the rest of the command line and an `Output` that implements `core::fmt::Write`. The kernel keeps 95 characters of the help text. A name that is already taken makes `register` fail, and the `?` turns that into a failed `insmod`.
- **`static CALLS`** is the module's own. Each module carries its own copy of `kcore`, so modules share no statics and cannot call each other.

`kmod` supplies the rest once for every module: the global allocator, which is the kernel heap; the panic handler, so a module that panics panics the kernel; and the header, with its magic, layout version, name, init, exit and the kernel interface digest.

## Make your own

Copy `hello` and give the copy its own name. Here it is `greet`:

```sh
cp -r src/rust/modules/hello src/rust/modules/greet
```

In `src/rust/modules/greet/Cargo.toml`, rename the package to `mod-<name>` and the library to `mod_<name>`. The Makefile builds the package `mod-greet` and links `libmod_greet.a`:

```diff
 [package]
-name = "mod-hello"
+name = "mod-greet"
 version = "0.1.0"
 edition = "2021"

 [lib]
-name = "mod_hello"
+name = "mod_greet"
 crate-type = ["staticlib"]
```

Add the crate to the workspace's `members` in `src/rust/Cargo.toml` (the line is abbreviated here). Leave `default-members` alone: it is what the kernel's own build compiles.

```diff
-members = [..., "modules/hv", "modules/heapbench"]
+members = [..., "modules/hv", "modules/heapbench", "modules/greet"]
```

Add the name to `MODULES` in the `Makefile`:

```diff
-MODULES = hello modtest slowexit blkload netblk netload sshd hv heapbench
+MODULES = hello modtest slowexit blkload netblk netload sshd hv heapbench greet
```

Finally, in `src/rust/modules/greet/src/lib.rs`, change the command's name and help, and the module's name in `kmod::module!(name: "greet", init: init)`.

### Reach the kernel through `kcore`

What a module can use is what `kcore` wraps. [What `kcore` has](/docs/rust#what-kcore-has) is the full map. The parts a module reaches for most:

| `kcore::` | What it gives a module |
|---|---|
| `cmd` | `Command::register`, and `Output::read_input` for a command that reads what an SSH user types |
| `trace` | `trace!(level, ...)`, into the kernel log; level 0 is always visible |
| `sync` | locks that own what they guard: `Mutex<T>`, `SpinLock<T>`, `IrqSpinLock<T>` and others; `Event`, `WaitGroup` |
| `task` | tasks: `spawn_with`, `spawn_on_with` for a task that holds an `Arc` of the module's state; `TaskHandle` joins on drop; `sleep_ms` |
| `time`, `random`, `cpu` | the clock, the kernel's random pool, CPU ids and IPIs |
| `fs` | files: `read`, `write`, `create`, and `File` for a file held open |
| `net`, `tcp`, `block` | an existing NIC (UDP listeners, frames), TCP listeners and streams, an existing disk or partition |
| `timer`, `softirq` | a periodic timer; a soft IRQ handler, which has no unregister and makes the module **permanent**: `rmmod` refuses it |

When a command needs the module's state, keep the state in an `Arc` and move a clone into the handler. This is how the hypervisor module registers `hv`:

```rust
let for_cmd = state.clone();
let cmd = Command::register("hv", HELP, move |args, out| command(&for_cmd, args, out))?;
```

The rules a module lives by, from the project's conventions:

- **Write it with no `unsafe`.** Reach for the `kcore` type first. When the shape you need has none, add the type to `kcore`, once, with the reason it is sound. `scripts/unsafe-count.py modules/greet` counts the sites.
- **Never allocate or free with a spin lock held.** The page allocator shoots down every other CPU's TLB and waits for all of them to answer. A CPU spinning on your lock has interrupts off and can never answer, so the two wait for each other until the kernel panics, ten seconds later, naming the CPUs that never answered. Collect what needs freeing under the lock, unlock, then free.
- **Mind what waits when it is dropped.** A UDP listener, a `TaskHandle` or an interrupt handle waits for its callbacks when dropped. Keep each one in its own `Lock<Option<..>>`: `take()` it under the lock and drop it after. Dropped under the lock its wait needs, it is a deadlock.
- **Never run `rmmod` from the module's own command.** The unload would wait for that command, which is waiting for the unload.
- **A module can call only what `ffi` declares.** A new kernel service takes three steps: the C++ function in `src/cpp/kernel/rust_ffi.cpp`, its declaration in `src/rust/ffi`, and the safe wrapper in `src/rust/kcore` ([Adding a kernel service](/docs/rust#adding-a-kernel-service)). That changes the interface digest, so the kernel and every module are rebuilt together.

## Build it

```sh
make modules
make modules ARCH=aarch64
```

These produce `out/x86_64/modules/greet.ko` and `out/aarch64/modules/greet.ko`. `make` and `make nocheck` build the modules too. On a Mac, build in the `nos-builder` container:

```sh
docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder make modules
```

Each module is compiled on its own, with its own copy of `core`, `alloc`, `kcore` and `kmod`, into `src/rust/target/modules`, because it needs flags the kernel's Rust does not use:

- **position-independent code**, since nobody knows where it will be loaded;
- **on x86, the small code model**, where the kernel itself uses the large one;
- **compiler-builtins' own `memcpy` and friends**, so the module imports nothing but the kernel's API.

`ld.lld` then links it as a shared object with `-Bsymbolic`, with every segment on pages of its own, and with a version script that exports `nos_module_info` and nothing else. A `.nos_syms` section with the module's demangled function names is added for backtraces. `hello.ko` comes out at around 25 KiB.

> Build a module from the same tree as the kernel you load it into. The build hashes the `ffi` crate's sources into a digest that goes into every module and into the kernel, and the loader refuses a module whose digest is not the kernel's. A module built outside the Makefile carries `unset` and is refused too.

## Get it onto the machine

- **On the root filesystem.** Put the `.ko` in a directory and make a root image of it with `scripts/mkrootfs.sh root.img 64 <dir>`. Attach the image to QEMU as a virtio disk: the ISO's `root=auto` mounts a filesystem labelled `nos` on `/`. The [Linux guest guide](/howto/linux-guest) has a complete QEMU command.
- **Over HTTP from your machine.** Serve the build directory and fetch the file from nos's shell. Under QEMU's user network your machine is 10.0.2.2:

  ```sh
  python3 -m http.server 8000 --directory out/x86_64/modules
  ```

  ```sh
  wget http://10.0.2.2:8000/greet.ko /greet.ko
  ```

  With no root disk, `/` is a ramfs, and the file is gone at the next boot.
- **From a release, for the modules in the tree.** Every release carries each module as `<name>-x86_64.ko` and `<name>-arm64.ko`. Take it from the release the running kernel came from (`version` names it):

  ```sh
  wget https://github.com/irqlevel/nos/releases/download/<tag>/hello-x86_64.ko /hello.ko
  ```

On a server set up from the system image, `/lib/modules` belongs to the image and is replaced by every update, so keep your own modules elsewhere ([Install nos on a server](/howto/install-on-a-server)).

## Load it, use it, unload it

A session with `hello`, as the project's documentation records it; your own module behaves the same way:

```text
$ insmod /hello.ko
module: hello loaded at 0xFFFF8000...
$ hello nos
hello, nos -- call 1 since the module was loaded
$ lsmod
hello  24 KiB at 0xFFFF8000..., 7 kernel imports
$ rmmod hello
module: hello unloaded
```

- **The module's log lines** go to the kernel log, not to the shell: run `dmesg 5 hello` to see them.
- **The five-second wait.** `insmod` and `rmmod` each run in a task of their own and wait for it at most five seconds. An unload that takes longer, for example one waiting on a command of the module that is still running, carries on in the background. `lsmod` shows the module as unloading meanwhile, and the kernel log says how it ended.
- **Loading at boot.** Add the line to `/etc/rc` with `rc add insmod /greet.ko`. The shell's task runs `/etc/rc` once the network is up.
- **Shutdown.** `poweroff` and `reboot` unload every module that can be unloaded, newest first, before the filesystems are unmounted.

## What the loader checks

`insmod` hands the file to the loader, `src/cpp/kernel/module.cpp`, which works through these steps and refuses the module at the first one that fails:

1. **The file.** It must be ELF64, little-endian, a shared object for this CPU, with its headers inside the file and no thread-local or interpreter segment. Every loadable segment must be page-aligned, inside the file and clear of the others, and **never both writable and executable**.
2. **The imports, before anything is mapped.** Each undefined symbol is looked up in the kernel's export table. A module the kernel cannot satisfy is refused, with the list of everything it lacks.
3. **The image.** It is mapped into pages of its own, 16 MiB at most with the function names.
4. **The relocations.** Only three kinds are applied: nothing, load base plus addend, and symbol plus addend. Anything else is refused.
5. **The header**, now that its pointers are real: the magic, the layout version, the name, an init and an exit that point into the module's own code, and the kernel interface digest.
6. **The page permissions.** Each segment gets what its program header asks for: read-only, read-execute or read-write. Every CPU's TLB is shot down, and on arm64 the instruction caches are synchronised. The kernel stays W^X with a module in it.
7. **The init.** A second module with the same name is refused before this step, and an init that fails leaves nothing behind.

What a refusal looks like:

| Message | What to do |
|---|---|
| `module: built against another kernel interface (ffi ..., this kernel ...) -- rebuild it from this tree` | Build the module from the kernel's tree, or take it from the kernel's release. |
| `module: needs <symbol>, which this kernel does not export` | The module calls something this kernel's `ffi` does not declare. Rebuild the kernel and the module together. |
| `module: segment N is both writable and executable` | Build the `.ko` with `make modules`, whose link gives every segment pages and permissions of its own. |
| `module: no nos_module_info -- not a module built with kmod::module!` | The crate never invokes `kmod::module!`. |
| `module: greet is already loaded` | `rmmod greet` first. |
| `module: greet: init failed` | `init` returned an error. `dmesg` shows `module init failed: ...` with the reason. |
| `module: greet is permanent: it imports ..., which nothing undoes` | The module registered a soft IRQ handler; it stays until the next boot. |

A frame in a module's code is named in backtraces like the kernel's own, with the module's name after it, for example `mod_hello::init::{closure#0}+0x3c [hello]`. A panic report ends with a `Modules:` line that says where each module sits.

## Test it

- **Every boot tests the loader.** The boot self-test loads `modtest`, a module embedded in the kernel image, on both architectures, then feeds the loader damaged copies of it: truncated, another machine's, an import the kernel lacks, no header, a bad magic, another kernel interface. The smoke boots therefore cover the loader ([Run the gates](/howto/run-the-gates)).
- **After touching the loader or the `ffi` declarations,** run `scripts/sshd-test.py`.
- **Under the undefined-behaviour checks,** build with `make nocheck RUSTUB=1`. It rebuilds the modules with `core`'s checks of its unsafe preconditions and overflow checks on, so the first violation is a panic that names its site.
- **Your own module** you test by hand. If it can fail in a way that looks like success, give it a gate of its own, as `netload` has `scripts/netload-test.py`.

## Troubleshooting

**`make modules` does not build your module.** Its name is missing from `MODULES` in the `Makefile`.

**Cargo says no package matches `mod-greet`.** The crate is missing from the workspace's `members`, or its package is not named `mod-<name>`.

**`insmod` says `init failed`.** Check `dmesg` for the `module init failed` line. A common cause is the command's name: `register` fails when a built-in command or another module's command already has it.

**`rmmod` returns but `lsmod` still shows the module as unloading.** A call of one of its commands is still running, and the unload waits for it. A command that never returns leaves its module unloading for good.

**The kernel panicked in your code.** A module that panics panics the kernel. The backtrace names your frames with `[greet]`.

## Next steps

- [Loadable kernel modules](/docs/modules): the reference, with the larger modules `blkload`, `netload`, `netblk`, `sshd`, `heapbench` and `hv` described.
- [Rust in the kernel](/docs/rust): the layers, where `unsafe` goes, and what `kcore` has.
- The real modules' source: [`src/rust/modules`](https://github.com/irqlevel/nos/blob/master/src/rust/modules).
- [Boot a Linux guest](/howto/linux-guest): the biggest module there is, the hypervisor.
