# The management bus

nos is managed through a structured API: a host API module serves it, and
the panel, `nosctl` and other hosts' panels are its clients. That API needs
something to call. Modules cannot call each other: each is linked on its
own against the kernel's export table. So the bus lives in the kernel image,
which every module reaches (`src/crates/mgmt`).

On the bus, a **provider** answers by name what it is asked:

- one of the kernel's layers, in the image;
- a module, through the C ABI.

Whoever asks -- the host API module, the `mgmt` command -- finds the
provider by its name.

## Providers, methods, descriptors

A provider registers three things, all copied:

- **a name:** a lowercase letter, then up to 23 lowercase letters, digits
  and `_`;
- **its methods:** up to 48, each with a name (a lowercase letter, then up
  to 31 lowercase letters, digits, `_` and `.`) and a kind:
  - **read** asks, and changes nothing;
  - **action** changes something, and is done when it returns;
  - **job** takes long, and runs on a task of the bus's own;
- **a descriptor:** up to 16 KiB of JSON, for a client to build its views
  from. The bus checks it once, as the provider registers (below), keeps
  it, and hands it to whoever asks.

A call's parameters and its answer are JSON as `mgmtwire` has it:

- whole numbers only, from i64::MIN to u64::MAX;
- a count past 2^53 as its digits in a string;
- no key twice;
- every limit checked.

The bus core passes both on without reading them: what a method takes it
takes with `mgmtwire::Params`, by name, and refuses an unknown one. A
failure answers `{"error": {"code", "message", "field"}}` with one of the
statuses in `mgmtwire::Status`: `no_provider`, `no_method`, `invalid`,
`busy`, `failed`, `cancelled`, `too_large`, `gone`, `kind`, `unsupported`,
`conflict`, `not_found`, `forbidden`.

In the image, a provider implements `mgmt::Provider` and registers with
`mgmt::register`. A module uses `kcore::mgmt::Provider::register`; dropping
the `Provider` -- which a module's unload does -- unregisters it.

## Descriptors

A client -- the panel above all -- builds its pages, tables, forms and
charts from the descriptors, and a page for a provider's method from the
method's entry: a descriptor that does not say what its provider has shows
a method that is not there, or lets a form call a read with an action's
parameters. So `mgmt::register` and `kernel_mgmt_register` check it
(`mgmtwire::descriptor::check`) and refuse the provider, `invalid` and a
line in the log saying where and why, if it does not hold:

- **JSON as the bus reads it**, within its limits: 16 KiB, 12 deep;
- **`"schema": 1`**, and **`"provider"`** its name;
- **`"methods"`**: exactly the methods it registers, each once with its
  `kind` (`read`, `action`, `job`), a `title`, and optionally `help`,
  `confirm` (an action or a job: what the client asks before it calls),
  `params` (fields, below) and `result` (a name);
- **`"resources"`**: optional, each a table of things the provider has --
  `list` (a read) and the `items` of its answer that are the rows, `key`
  and `columns` among its `fields`, optionally `get` (a read), `parent`
  (another resource) and `actions` (actions and jobs, each `params` a
  field of the row as `"$name"` or a value);
- **`"metrics"`**: optional, each a number a client charts -- the `method`
  (a read) and the `field`, for each of the `items` by `key` or for the
  answer as a whole, a `gauge` or a `rate`, with its `unit`;
- **`"ui"`**: optional, the `section` of the client it goes in, its `order`
  and an `icon`;
- **fields**: a `type` -- `string`, `text`, `int`, `bool`, `enum` (with its
  `values`), `bytes`, `duration_ns`, `duration_ms`, `time_s`, `ipv4`,
  `mac`, `port`, or a `list` `of` one of them -- a `title`, and optionally
  `help`, `unit`, `min` and `max` (numbers), `required` and a `default`;
- **every text a person reads** -- each `title`, `help`, `confirm` and
  enum value -- an object of exactly `en` and `ru`, neither empty;
- **nothing the format does not have**: a misspelt member is refused rather
  than ignored.

A count says its unit in its name (`_bytes`, `_ns`, `_ms`, `_s`) and is
never a fraction: a client makes a rate from two answers.

The image's descriptors are files beside their providers
(`*.descriptor.json`, `include_str!`-ed), which `web-test.py` checks on the
host as well: as the bus does, for the methods each says it has, and every
text by Python, no key twice and none left untranslated.

## A call

`mgmt::call(provider, method, params, max_reply, sink)`:

1. **Looked up under the bus's lock.** The provider must be there and not
   going, the method must be of the kind asked for (a job is started, not
   called: `kind`), and there must be room among the 64 calls in flight.
2. **Counted** in its provider's `running`, and put on the list of calls in
   flight with the task it runs on.
3. **Made with no lock held.** An image provider's trait is called; a
   module's handler is called through the C ABI.
4. **Answered into a buffer of the call's own.** It is bounded by what the
   caller takes, at most 256 KiB, and handed to the caller's sink after the
   call. A module answers by the call's id (`kernel_mgmt_reply`), and an
   answer is taken only from the task its call runs on, so an id kept past
   its call answers nothing.
5. **Timed.** A call longer than a second says so in the log: what takes
   long should be a job.

## Jobs

`mgmt::job_start(provider, method, params)` gives a job's number at once, and
the job is called on a task of its own (`mgmt/job<N>`).

- **Its record:** queued, running or done; the status it ended with; its
  progress (`done` of `total`); the last 64 lines of its log, numbered; and
  its result, up to 16 KiB.
- **What the record outlives:** the job itself, whoever started it, and its
  provider.
- **Told to stop** (`cancelled`) by `job_cancel`, by its provider's
  unregister and by the halt. Stopping is its method's to do: it looks, and
  ends.
- **Limits:** at most 4 jobs not done (a fifth is refused `busy`) and 32
  records kept, done ones the first to go.
- **A job whose provider went before it started** ends `gone`.

## The providers in the image

Each layer registers its own as it comes up, for good; none takes long, and
each reads its data the way its shell commands do -- copied under the lock
that guards it, and written out with no lock held.

| Provider | Registered by | Reads | Actions |
|---|---|---|---|
| `system` | `src/crates/kernel/src/system.rs` | `info` (version, revision, arch, CPU model, CPUs, uptime, the wall clock, the log level, the command line), `cpus` (each CPU's time in its idle task, the stretch it is in now included), `memory`, `tasks`, `log` (`since`, `max_bytes`), `modules`, `interrupts` | `set_log_level`, `set_nice`, `load_module` (a module of `/lib/modules` by its name, and its parameters), `unload_module`, `power_off`, `reboot` |
| `net` | `src/crates/net/src/provider.rs` | `interfaces` (addresses, and the frames and bytes each has moved), `tcp` (the slots, those used, and each connection), `arp`, `pool` | |
| `block` | `src/crates/block/src/provider.rs` | `disks` (every disk and partition: its size, the disk it is on and where, and who holds it) | |
| `fs` | `src/crates/fs/src/provider.rs` | `mounts` (where, which filesystem, on which device, and its size and free space, from `FileSystem::space`) | |

What the C++ holds it copies out through `kcore::system`, into the
caller's buffer and with nothing allocated: the tasks
(`TaskTable::snapshot`), the modules (`ModuleTable::snapshot`, which gives
no address), the log by number (`Dmesg::read_since`: every message has a
number, given as it goes on the log under its lock, and a message dropped
takes its number too, so a reader sees the gap), and a CPU's idle time
(`Task::get_runtime_ns`, under the idle task's lock). Its actions go
through what the shell's commands call: the module loader's `start_load`
and `start_unload`, which now say how they ended (`JobOutcome`), the
tracer's level, `set_task_nice`, and the shell's own power-off and reboot.

`power_off` and `reboot` are actions, not jobs: each asks the shell's watch
for the halt and returns, its answer on its way before the halt has begun.
As a job it would never end -- the halt waits for every job to (`stop`),
and this one would be waiting for the halt.

## The providers of modules

| Provider | Registered by | Reads | Actions | Jobs |
|---|---|---|---|---|
| `api` | `modules/api` | `status`, `clients` | | |
| `vm` | `modules/hv` (x86-64) | `list`, `get`, `host`, `networks`, `config_get`, `console_read`, `disks`, `forwards` | `config_set`, `forward_add`, `forward_del`, `console_write`, `disk_add`, `disk_grow` | `start`, `stop`, `restart`, `create`, `clone`, `delete` |
| `modtest` | `modules/modtest`, the loader's self-test | `echo` | | `wait`, `nap` |

A module's provider is unregistered with the module, which waits out its
calls and jobs -- told to stop -- before anything they use goes: hv drops
its provider before its forwards and its VMs, so no job of it starts a guest
behind the unload ([On the management bus](hypervisor.md#on-the-management-bus)).
A job that takes long looks at its cancel as it goes: the vm provider's
create looks at it as each piece of a download arrives and after each
cluster it converts, its clone after each quarter of a MiB it copies, and a
stop or a restart stops waiting for the guest.

## When a provider goes

Unregistering follows the protocol of the shell's table of module commands
(`Cmd::unregister_dynamic`):

1. No call of the provider starts from then on (`removing`).
2. Its jobs are told to stop.
3. The unregister returns only once its `running` is 0, saying every 10 s
   which of its methods it still waits for, and on which tasks.

A module's unload unregisters before its code goes, so no call is ever made
into a module that is gone. Unregistering from inside one of the provider's
own calls would wait for itself: that is a bug, and a panic says so.

Handles and call ids carry a generation in their high bits and their slot
in the low eight; they are looked up, never followed. Locks: the job
table's, then a job's; the bus's is never held with either.

The halt, after the modules are unloaded and before the filesystems go,
calls `rust_mgmt_stop`:

- no call, job or provider starts from then on;
- every job is told to stop and waited for;
- any module's provider still registered is named in the log.

## A module asking the bus

The host API is a module, and the bus is in the image: what it calls goes
through the bus's C ABI, as a provider's answers do the other way.
`kcore::mgmt` has it as functions:

- `call(provider, method, params, max_reply, sink)` -- a read or an action,
  its answer handed to the module's sink in pieces;
- `kind(provider, method)` -- what the method is registered as: what a role
  is let call by;
- `providers(kinds, sink)` -- the providers and their methods as JSON, the
  methods of the kinds asked alone (a reader's: the reads);
- `describe(provider, sink)` -- a descriptor;
- `job_start`, `job(id, notes_since, sink)`, `jobs(sink)`, `job_cancel`.

The JSON is the bus's own (`mgmt/src/view.rs`), written so it is handed on as
it is: a job's result is embedded as the JSON it is once the bus has read it
as such, and as a string if it is not. Every length a module hands over is
checked before a slice is made of it; the sink is called only while its call
runs. Every boot's `test_module_bus` goes through all of it once, from
modtest (`modtest bus`).

## The `mgmt` command

```
mgmt                                  the providers, their methods and kinds
mgmt describe <provider>              its descriptor
mgmt call <provider> <method> [json]  a call, and its answer
mgmt start <provider> <method> [json] a job started: its number
mgmt jobs                             every job's record
mgmt job <id>                         one's record, its log and its result
mgmt cancel <id>                      one told to stop
```

It is the bus from the shell, and so from SSH and the boot's own tests.
What it prints is gathered first and printed with no lock held.

## Tested

Every boot runs three tests, on both architectures:

- **`mgmt::selftest`**, in `rust_test`, with a provider of its own:
  - descriptors refused that do not say what the provider has: an empty
    one, one with a method of the wrong kind;
  - calls answered, and refused as they should be: no provider, no method,
    a job called as a call, an answer past its limit;
  - a job run to its end, its progress, log and result kept;
  - one told to stop by its provider's unregister, which waits for it;
  - the name free again after.
- **`system::selftest`**, after it: every read of every provider in the
  image -- system's, net's, block's and fs's -- called as a client would,
  each answer held to being JSON as the bus reads it; and the log read by
  number, a line logged just before found after the number the log had,
  and nothing at or before the number asked after.
- **`test_module_bus`**, after it:
  - modtest loaded, its provider called through `mgmt call`;
  - a job of it started and seen running;
  - the module's unload told the job to stop and waited it out: its record
    says `cancelled`, and the provider is gone with its module.

  It needs tasks, so it runs after `rust_test`, not in `test_modules`.

Two mutants each fail the boot: an unregister that does not tell its jobs to
stop, and one that does not wait for its calls.
