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
  from. The bus keeps it and does not read it.

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

Every boot runs two tests, on both architectures:

- **`mgmt::selftest`**, in `rust_test`, with a provider of its own:
  - calls answered, and refused as they should be: no provider, no method,
    a job called as a call, an answer past its limit;
  - a job run to its end, its progress, log and result kept;
  - one told to stop by its provider's unregister, which waits for it;
  - the name free again after.
- **`test_module_bus`**, after it:
  - modtest loaded, its provider called through `mgmt call`;
  - a job of it started and seen running;
  - the module's unload told the job to stop and waited it out: its record
    says `cancelled`, and the provider is gone with its module.

  It needs tasks, so it runs after `rust_test`, not in `test_modules`.

Two mutants each fail the boot: an unregister that does not tell its jobs to
stop, and one that does not wait for its calls.
