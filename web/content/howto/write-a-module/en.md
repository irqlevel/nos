---
title: "Write a loadable kernel module in Rust"
description: "Create a nos kernel module in Rust with kmod and kcore, give it a shell command, build the .ko and load it into a running kernel."
order: 8
updated: 2026-10-22
---

By the end of this guide you will have a module of your own. It is a crate under `src/rust/modules/` that builds into a `.ko` file and registers a shell command. You will load it into a running nos with `insmod` and remove it again with `rmmod`, without rebuilding or rebooting the kernel.

You need:

- the nos tree and a working build, natively or in Docker ([Build nos](/howto/build));
- a nos to load the module into, for example in QEMU ([Run nos in QEMU](/howto/run-in-qemu));
- some Rust: modules are `#![no_std]` crates.

## What a module is

- **Rust only.** A module is a `staticlib` crate, linked into a position-independent ELF shared object: the `.ko` file.
- **It talks to the kernel through one API.** That API is the set of `extern "C"` functions declared in the `ffi` crate, with safe wrappers in `kcore`. The drivers built into the kernel use the same API. The loader binds a module against an export table generated from those declarations and refuses anything else.
- **`insmod` runs the module's init, and `rmmod` drops the state that init returned.** The module's `Drop` is its exit, so there is no exit function to write.
- **Modules are for the optional parts.** Anything a machine can run without, such as a server, a load generator or a diagnostic, is a module. `sshd`, `netblk`, `netload`, `blkload`, `heapbench` and the hypervisor, `hv`, are all modules.
- **A module cannot be a disk driver or a network card driver.** Those drivers register with their layer as Rust trait objects from inside the kernel image, and there is no C name a module could bind to. A module uses disks and network cards (NICs), but it does not provide them.

## Start with the smallest real module: `hello`

`src/rust/modules/hello/` contains two files. The first is `Cargo.toml`:

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

The second is [`src/lib.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/modules/hello/src/lib.rs), shown here without its doc comment:

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

- **`kmod::module!(name: "hello", init: init)`** names the module and tells `insmod` which function to run. `lsmod` shows this name, and `rmmod` takes it. The name can be at most 31 printable characters; a longer one fails the build. The macro also emits `nos_module_info`, the header the loader looks for.
- **`init`** builds the module's state and returns it as a `Box<dyn kmod::Module>`. If it returns an error instead, the `insmod` fails and the kernel log says `module init failed: ...`.
- **`Hello` is the module's state.** It owns everything the module registered. As long as it holds the `Command`, the command stays registered. `rmmod` drops `Hello`, and dropping the `Command` unregisters the command. Every unregister in `kcore` first waits for any callback still running on another CPU, so the command's code is never freed while it runs. Here the `Drop` impl only adds a log line.
- **`impl kmod::Module for Hello {}`** requires `Send`, because the state changes hands: the task that runs `insmod` builds it, and the task that runs `rmmod` drops it.
- **`Command::register(name, help, handler)`** adds a command to the shell, wherever the shell runs: on the console, in the UDP shell or in an SSH session. The handler is `Fn(&str, &mut Output) + Send + Sync + 'static`. It receives the rest of the command line and an `Output` that implements `core::fmt::Write`. The kernel keeps at most 95 characters of the help text. If the name is already taken, `register` fails, and the `?` turns that into a failed `insmod`.
- **`static CALLS`** belongs to this module alone. Every module carries its own copy of `kcore`, so modules share no statics and cannot call each other.

`kmod` provides everything else, once for all modules. It sets the kernel heap as the global allocator. It installs the panic handler, so a panic in a module panics the kernel. And it supplies the header, with the magic number, the layout version, the name, init, exit and the kernel interface digest.

## Make your own

Copy `hello` and give the copy a name of its own. This guide uses `greet`:

```sh
cp -r src/rust/modules/hello src/rust/modules/greet
```

In `src/rust/modules/greet/Cargo.toml`, rename the package to `mod-<name>` and the library to `mod_<name>`. The Makefile expects exactly these names: it builds the package `mod-greet` and links `libmod_greet.a`.

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

Add the crate to the workspace's `members` in `src/rust/Cargo.toml` (the line is shortened here). Leave `default-members` alone, because that list is what the kernel's own build compiles.

```diff
-members = [..., "modules/hv", "modules/heapbench"]
+members = [..., "modules/hv", "modules/heapbench", "modules/greet"]
```

Add the name to `MODULES` in the `Makefile`:

```diff
-MODULES = hello modtest slowexit blkload netblk netload sshd hv heapbench
+MODULES = hello modtest slowexit blkload netblk netload sshd hv heapbench greet
```

Finally, in `src/rust/modules/greet/src/lib.rs`, change the command's name and help text, and set the module's name in `kmod::module!(name: "greet", init: init)`.

### Reach the kernel through `kcore`

A module can use whatever `kcore` wraps. The full map is in [What `kcore` has](/docs/rust#what-kcore-has). These are the parts modules use most:

| `kcore::` | What it gives a module |
|---|---|
| `cmd` | `Command::register`, and `Output::read_input` for a command that reads what an SSH user types |
| `trace` | `trace!(level, ...)`, which writes to the kernel log; level 0 is always visible |
| `sync` | locks that own the data they guard (`Mutex<T>`, `SpinLock<T>`, `IrqSpinLock<T>` and others), plus `Event` and `WaitGroup` |
| `task` | tasks: `spawn_with` and `spawn_on_with` start a task that holds an `Arc` of the module's state; a `TaskHandle` waits for its task when dropped; `sleep_ms` |
| `time`, `random`, `cpu` | the clock, the kernel's random pool, CPU ids and inter-processor interrupts (IPIs) |
| `fs` | files: `read`, `write`, `create`, and `File` to keep a file open |
| `net`, `tcp`, `block` | an existing NIC (UDP listeners, frames), TCP listeners and streams, an existing disk or partition |
| `timer`, `softirq` | a periodic timer; a soft IRQ handler. A soft IRQ handler cannot be unregistered, so it makes the module **permanent**, and `rmmod` refuses to unload it |

When a command needs the module's state, keep the state in an `Arc` and move a clone into the handler. This is how the hypervisor module registers `hv`:

```rust
let for_cmd = state.clone();
let cmd = Command::register("hv", HELP, move |args, out| command(&for_cmd, args, out))?;
```

Modules follow the project's conventions. These rules matter most:

- **Write it without `unsafe`.** Look for a `kcore` type first. If none fits what you need, add the type to `kcore` once, with a comment explaining why it is sound. `scripts/unsafe-count.py modules/greet` counts the `unsafe` sites.
- **Never allocate or free memory while holding a spin lock.** The page allocator flushes the TLB of every other CPU (a TLB shootdown) and waits for all of them to answer. A CPU spinning on your lock has interrupts off, so it can never answer. The two wait for each other until, ten seconds later, the kernel panics and names the CPUs that never answered. Instead, collect what needs freeing under the lock, unlock, and only then free it.
- **Be careful with objects that wait when dropped.** A UDP listener, a `TaskHandle` or an interrupt handle waits for its callbacks when it is dropped. Keep each one in its own `Lock<Option<..>>`: `take()` it under the lock, and drop it after you unlock. If you drop it while holding the lock its wait needs, you get a deadlock.
- **Never run `rmmod` from the module's own command.** The unload would wait for that command, and the command would wait for the unload.
- **A module can call only what `ffi` declares.** A new kernel service takes three steps: the C++ function in `src/cpp/kernel/rust_ffi.cpp`, its declaration in `src/rust/ffi`, and the safe wrapper in `src/rust/kcore` ([Adding a kernel service](/docs/rust#adding-a-kernel-service)). This changes the interface digest, so the kernel and every module have to be rebuilt together.

## Build it

```sh
make modules
make modules ARCH=aarch64
```

These commands produce `out/x86_64/modules/greet.ko` and `out/aarch64/modules/greet.ko`. Plain `make` and `make nocheck` build the modules as well. On a Mac, build inside the `nos-builder` container:

```sh
docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder make modules
```

Each module is compiled separately into `src/rust/target/modules`, with its own copy of `core`, `alloc`, `kcore` and `kmod`. A module needs flags that the kernel's Rust does not use:

- **position-independent code**, because nobody knows in advance where the module will be loaded;
- **on x86, the small code model**, while the kernel itself uses the large one;
- **compiler-builtins' own `memcpy` and similar functions**, so that the module imports nothing except the kernel's API.

Then `ld.lld` links the module as a shared object with `-Bsymbolic`. Every segment gets pages of its own, and a version script exports `nos_module_info` and nothing else. For backtraces, the build adds a `.nos_syms` section with the module's demangled function names. The resulting `hello.ko` is about 25 KiB.

> Build a module from the same tree as the kernel you will load it into. The build hashes the sources of the `ffi` crate into a digest and puts that digest into the kernel and into every module. The loader refuses a module whose digest differs from the kernel's. A module built outside the Makefile carries `unset` as its digest, so it is refused too.

## Get it onto the machine

- **On the root filesystem.** Put the `.ko` in a directory and turn the directory into a root image with `scripts/mkrootfs.sh root.img 64 <dir>`. Attach the image to QEMU as a virtio disk. The ISO boots with `root=auto`, which mounts a filesystem labelled `nos` on `/`. The [Linux guest guide](/howto/linux-guest) has a complete QEMU command.
- **Over HTTP from your machine.** Serve the build directory and fetch the file from the nos shell. On QEMU's user network, your machine is at 10.0.2.2:

  ```sh
  python3 -m http.server 8000 --directory out/x86_64/modules
  ```

  ```sh
  wget http://10.0.2.2:8000/greet.ko /greet.ko
  ```

  Without a root disk, `/` is a ramfs that lives in memory, so the file is gone at the next boot.
- **From a release, for the modules in the tree.** Every release includes each module as `<name>-x86_64.ko` and `<name>-arm64.ko`. Download it from the same release as the running kernel (the `version` command tells you which one):

  ```sh
  wget https://github.com/irqlevel/nos/releases/download/<tag>/hello-x86_64.ko /hello.ko
  ```

On a server installed from the system image, `/lib/modules` belongs to the image, and every update replaces it. Keep your own modules somewhere else ([Install nos on a server](/howto/install-on-a-server)).

## Load it, use it, unload it

Here is a session with `hello`, as recorded in the project's documentation. Your own module behaves the same way:

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

- **The module's log lines** go to the kernel log, not to the shell. Run `dmesg 5 hello` to see them.
- **The five-second wait.** `insmod` and `rmmod` each hand the work to a task of their own and wait at most five seconds for it. If an unload takes longer, for example because one of the module's commands is still running, it carries on in the background. Meanwhile `lsmod` shows the module as unloading, and the kernel log says how the unload ended.
- **Loading at boot.** Run `rc add insmod /greet.ko` to add the line to `/etc/rc`. The shell's task runs `/etc/rc` once the network is up.
- **Shutdown.** Before the filesystems are unmounted, `poweroff` and `reboot` unload every module that can be unloaded, newest first.

## What the loader checks

`insmod` passes the file to the loader in `src/cpp/kernel/module.cpp`. The loader goes through the steps below in order and refuses the module at the first one that fails:

1. **The file.** It must be a little-endian ELF64 shared object for this CPU, with its headers inside the file and no thread-local or interpreter segment. Every loadable segment must be page-aligned, lie inside the file, not overlap the others, and **never be both writable and executable**.
2. **The imports, before anything is mapped.** The loader looks up each undefined symbol in the kernel's export table. If the kernel cannot provide them all, the loader refuses the module and lists everything that is missing.
3. **The image.** The loader maps it into pages of its own. Together with the function names, it can be 16 MiB at most.
4. **The relocations.** The loader applies only three kinds: none, load base plus addend, and symbol plus addend. It refuses any other kind.
5. **The header.** Now that its pointers hold real addresses, the loader checks the magic number, the layout version, the name, an init and an exit that point into the module's own code, and the kernel interface digest.
6. **The page permissions.** Each segment gets the permissions its program header asks for: read-only, read-execute or read-write. The TLB is flushed on every CPU, and on arm64 the instruction caches are synchronised. The kernel stays W^X with a module loaded: no page is both writable and executable.
7. **The init.** A second module with the same name is refused before this step. An init that fails leaves nothing behind.

The refusals you may see, and what to do about each:

| Message | What to do |
|---|---|
| `module: built against another kernel interface (ffi ..., this kernel ...) -- rebuild it from this tree` | Build the module from the kernel's tree, or take it from the kernel's release. |
| `module: needs <symbol>, which this kernel does not export` | The module calls something that this kernel's `ffi` does not declare. Rebuild the kernel and the module together. |
| `module: segment N is both writable and executable` | Build the `.ko` with `make modules`. Its link step gives every segment its own pages and permissions. |
| `module: no nos_module_info -- not a module built with kmod::module!` | The crate never invokes `kmod::module!`. |
| `module: greet is already loaded` | Run `rmmod greet` first. |
| `module: greet: init failed` | `init` returned an error. `dmesg` shows `module init failed: ...` with the reason. |
| `module: greet is permanent: it imports ..., which nothing undoes` | The module registered a soft IRQ handler, so it stays loaded until the next boot. |

Backtraces name the frames in a module's code the same way as the kernel's own, with the module's name after them, for example `mod_hello::init::{closure#0}+0x3c [hello]`. A panic report ends with a `Modules:` line that shows where each module is loaded.

## Test it

- **Every boot tests the loader.** On both architectures, the boot self-test loads `modtest`, a module embedded in the kernel image. It then feeds the loader damaged copies of it: truncated, built for another machine, with an import the kernel lacks, with no header, with a bad magic number, and built for another kernel interface. So the smoke boots cover the loader too ([Run the gates](/howto/run-the-gates)).
- **After changing the loader or the `ffi` declarations,** run `scripts/sshd-test.py`.
- **To run under the undefined-behaviour checks,** build with `make nocheck RUSTUB=1`. This rebuilds the modules with overflow checks and with `core`'s checks of its unsafe preconditions turned on. The first violation then becomes a panic that names where it happened.
- **Test your own module** by hand. If it can fail in a way that looks like success, give it a gate of its own: `netload`, for example, has `scripts/netload-test.py`.

## Troubleshooting

**`make modules` does not build your module.** Its name is missing from `MODULES` in the `Makefile`.

**Cargo says no package matches `mod-greet`.** The crate is missing from the workspace's `members`, or its package is not named `mod-<name>`.

**`insmod` says `init failed`.** Look in `dmesg` for the `module init failed` line. A common cause is the command's name: `register` fails when a built-in command or another module's command already uses it.

**`rmmod` returns, but `lsmod` still shows the module as unloading.** One of its commands is still running, and the unload waits for it to finish. If a command never returns, its module stays in the unloading state for good.

**The kernel panicked in your code.** A panic in a module is a kernel panic. The backtrace marks your frames with `[greet]`.

## Next steps

- [Loadable kernel modules](/docs/modules): the reference, which also describes the larger modules `blkload`, `netload`, `netblk`, `sshd`, `heapbench` and `hv`.
- [Rust in the kernel](/docs/rust): the layers, where `unsafe` goes, and what `kcore` has.
- The source of the real modules: [`src/rust/modules`](https://github.com/irqlevel/nos/blob/master/src/rust/modules).
- [Boot a Linux guest](/howto/linux-guest): the biggest module of all, the hypervisor.
